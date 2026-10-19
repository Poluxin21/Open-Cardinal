//! Types shared by every rule backend.

use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::sync::Arc;
use std::time::Instant;

use serde::{Deserialize, Deserializer, Serialize};

use crate::config::Limits;
use crate::pb::core::{Reaction, reaction::ActionType};
use crate::store::{Command, Outcome};

/// What a rule sees (`pulse` in Lua, the JSON input of a WASM rule).
#[derive(Debug, Clone, Serialize)]
pub struct PulseInput {
    pub agent_id: String,
    pub tenant: String,
    pub timestamp: i64,
    pub trace_id: String,
    pub telemetry: HashMap<String, String>,
}

/// What a rule returns. Same shape as the original Lua contract.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct RuleOutput {
    pub action: String,
    #[serde(default)]
    pub cmd_name: Option<String>,
    #[serde(default, deserialize_with = "lenient_params")]
    pub params: Option<BTreeMap<String, String>>,
    #[serde(default)]
    pub priority: Option<i32>,
    /// Set by AI backends (model, prompt hash, raw output digest...). Never read from rule
    /// output; it only travels into the audit record.
    #[serde(skip)]
    pub evidence: Option<String>,
}

/// Rules naturally write `params = { retries = 3, urgent = true }`; accept scalars and
/// store them as strings (the wire format is `map<string,string>`).
fn lenient_params<'de, D>(d: D) -> Result<Option<BTreeMap<String, String>>, D::Error>
where
    D: Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Scalar {
        Str(String),
        Int(i64),
        UInt(u64),
        Float(f64),
        Bool(bool),
    }
    let raw: Option<BTreeMap<String, Scalar>> = Option::deserialize(d)?;
    Ok(raw.map(|m| {
        m.into_iter()
            .map(|(k, v)| {
                let v = match v {
                    Scalar::Str(s) => s,
                    Scalar::Int(i) => i.to_string(),
                    Scalar::UInt(u) => u.to_string(),
                    Scalar::Float(f) => f.to_string(),
                    Scalar::Bool(b) => b.to_string(),
                };
                (k, v)
            })
            .collect()
    }))
}

impl RuleOutput {
    pub fn idle() -> Self {
        Self { action: "IDLE".into(), cmd_name: None, params: None, priority: Some(0), evidence: None }
    }

    pub fn action_type(&self) -> Option<ActionType> {
        parse_action(&self.action)
    }

    pub fn priority(&self) -> i32 {
        self.priority.unwrap_or(0)
    }

    pub fn into_reaction(self, trace_id: &str) -> Reaction {
        let kind = self.action_type().unwrap_or(ActionType::Idle);
        Reaction {
            trace_id: trace_id.to_string(),
            r#type: kind as i32,
            command_name: self.cmd_name.unwrap_or_default(),
            parameters: self.params.unwrap_or_default().into_iter().collect(),
        }
    }
}

pub fn parse_action(s: &str) -> Option<ActionType> {
    match s.trim().to_ascii_uppercase().as_str() {
        "IDLE" => Some(ActionType::Idle),
        "SHUTDOWN" => Some(ActionType::Shutdown),
        "RESTART" => Some(ActionType::Restart),
        "CUSTOM" => Some(ActionType::Custom),
        _ => None,
    }
}

pub fn idle_reaction(trace_id: &str, command_name: &str) -> Reaction {
    Reaction {
        trace_id: trace_id.to_string(),
        r#type: ActionType::Idle as i32,
        command_name: command_name.to_string(),
        parameters: HashMap::new(),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum RuleKind {
    Lua,
    Wasm,
    Model,
    Prompt,
}

impl RuleKind {
    pub fn as_str(self) -> &'static str {
        match self {
            RuleKind::Lua => "lua",
            RuleKind::Wasm => "wasm",
            RuleKind::Model => "model",
            RuleKind::Prompt => "prompt",
        }
    }

    /// AI rules run after deterministic ones and can be skipped by an emergency decision.
    pub fn is_ai(self) -> bool {
        matches!(self, RuleKind::Model | RuleKind::Prompt)
    }
}

impl fmt::Display for RuleKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuleErrorKind {
    /// Wall-clock deadline reached.
    Timeout,
    /// Instruction / fuel budget exhausted.
    Budget,
    /// The rule itself failed (bug, bad return value, trap).
    Runtime,
    /// Output did not satisfy the rule contract.
    Contract,
    /// Shared-memory access failed.
    Host,
}

#[derive(Debug, Clone)]
pub struct RuleError {
    pub kind: RuleErrorKind,
    pub message: String,
}

impl RuleError {
    pub fn new(kind: RuleErrorKind, message: impl Into<String>) -> Self {
        Self { kind, message: message.into() }
    }
    pub fn runtime(message: impl fmt::Display) -> Self {
        Self::new(RuleErrorKind::Runtime, message.to_string())
    }
    pub fn contract(message: impl fmt::Display) -> Self {
        Self::new(RuleErrorKind::Contract, message.to_string())
    }
}

impl fmt::Display for RuleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}: {}", self.kind, self.message)
    }
}

impl std::error::Error for RuleError {}

/// Shared memory as seen by rules. All keys are implicitly scoped to the tenant.
pub trait Host: Send + Sync {
    fn kv_get(&self, tenant: &str, key: &str) -> crate::Result<Option<u64>>;
    fn kv_len(&self, tenant: &str) -> crate::Result<u64>;
    /// Make sure reads see every write acknowledged cluster-wide before this call. Called
    /// once per evaluation, before the first read. No-op on a single node.
    fn read_barrier(&self, _deadline: Instant) -> crate::Result<()> {
        Ok(())
    }
    /// Replicated write (`KvSet` / `KvAdd` / `KvDelete`). Blocks until committed, but never
    /// past `deadline`: a rule must not outlive its budget waiting for a cluster quorum.
    fn kv_apply(&self, cmd: Command, deadline: Instant) -> crate::Result<Outcome>;
}

pub const MAX_KEY_LEN: usize = 256;

/// Everything a backend needs to evaluate one rule.
pub struct EvalCtx<'a> {
    pub input: &'a PulseInput,
    pub mem: Arc<super::mem::SharedMem>,
    pub limits: &'a Limits,
    /// Overall deadline for this pulse.
    pub deadline: Instant,
}

pub trait RuleBackend: Send + Sync {
    fn kind(&self) -> RuleKind;
    /// `Ok(None)` means "no opinion" (the rule returned nothing).
    fn evaluate(&self, cx: &EvalCtx<'_>) -> Result<Option<RuleOutput>, RuleError>;
}

pub fn valid_key(key: &str) -> bool {
    !key.is_empty() && key.len() <= MAX_KEY_LEN
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_accepts_the_original_lua_contract() {
        let v: RuleOutput = serde_json::from_str(
            r#"{"action":"SHUTDOWN","cmd_name":"EMERGENCY_CUTOFF","priority":1000,"params":{"reason":"Overheating"}}"#,
        )
        .unwrap();
        let r = v.into_reaction("t1");
        assert_eq!(r.r#type, 1);
        assert_eq!(r.command_name, "EMERGENCY_CUTOFF");
        assert_eq!(r.parameters["reason"], "Overheating");
        assert_eq!(r.trace_id, "t1");
    }

    #[test]
    fn scalar_params_are_stringified() {
        let v: RuleOutput = serde_json::from_str(r#"{"action":"CUSTOM","params":{"n":3,"f":1.5,"b":true}}"#).unwrap();
        let p = v.params.unwrap();
        assert_eq!((p["n"].as_str(), p["f"].as_str(), p["b"].as_str()), ("3", "1.5", "true"));
    }

    #[test]
    fn unknown_action_is_not_silently_idle() {
        assert!(parse_action("SHUTDWN").is_none());
        assert_eq!(parse_action(" shutdown ").unwrap() as i32, 1);
    }

    #[test]
    fn missing_action_is_a_contract_error() {
        assert!(serde_json::from_str::<RuleOutput>(r#"{"priority":3}"#).is_err());
    }
}
