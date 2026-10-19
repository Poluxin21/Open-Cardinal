//! Wire format of the local control plane (CLI ⇄ daemon).
//!
//! One JSON [`Envelope`] per TCP connection (client half-closes after sending), one
//! [`CliResponse`] back. The envelope carries the admin token: the original protocol had
//! no authentication, so any local process could stop the daemon or force a SHUTDOWN
//! reaction on any agent (see docs/SECURITY.md, finding F-01).

use serde::{Deserialize, Serialize};

pub const MAX_REQUEST_BYTES: usize = 64 * 1024;
pub const MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;

#[derive(Debug, Serialize, Deserialize)]
pub struct Envelope {
    pub token: String,
    pub request: CliRequest,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub enum CliRequest {
    Status,
    Stop,
    Reload,
    Stats,
    Heathcliff { command: String, args: Vec<String> },
    Raft { command: String, args: Vec<String> },
    Tenant { command: String, args: Vec<String> },
    Rules { command: String, args: Vec<String> },
    Audit { command: String, args: Vec<String> },
}

impl CliRequest {
    /// Short name used in the audit trail (arguments are never logged: they may hold secrets).
    pub fn audit_name(&self) -> String {
        match self {
            CliRequest::Status => "status".into(),
            CliRequest::Stop => "stop".into(),
            CliRequest::Reload => "reload".into(),
            CliRequest::Stats => "stats".into(),
            CliRequest::Heathcliff { command, .. } => format!("heathcliff {command}"),
            CliRequest::Raft { command, .. } => format!("raft {command}"),
            CliRequest::Tenant { command, .. } => format!("tenant {command}"),
            CliRequest::Rules { command, .. } => format!("rules {command}"),
            CliRequest::Audit { command, .. } => format!("audit {command}"),
        }
    }

    /// Read-only requests are not worth an audit record each.
    pub fn is_mutating(&self) -> bool {
        match self {
            CliRequest::Status | CliRequest::Stats => false,
            CliRequest::Heathcliff { command, .. } => command != "list",
            CliRequest::Raft { command, .. } => !matches!(command.as_str(), "status" | "members"),
            CliRequest::Tenant { command, .. } => command != "list",
            CliRequest::Rules { .. } | CliRequest::Audit { .. } => false,
            CliRequest::Stop | CliRequest::Reload => true,
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub enum CliResponse {
    Ok { message: String },
    Data { payload: serde_json::Value },
    Error { message: String },
}

impl CliResponse {
    pub fn ok(msg: impl Into<String>) -> Self {
        Self::Ok { message: msg.into() }
    }

    pub fn error(msg: impl Into<String>) -> Self {
        Self::Error { message: msg.into() }
    }

    pub fn data(payload: serde_json::Value) -> Self {
        Self::Data { payload }
    }
}

/// Minimal `--key value` / `--key=value` / `--flag` parser; repeated keys accumulate.
#[derive(Debug, Default)]
pub struct Flags {
    map: std::collections::HashMap<String, Vec<String>>,
    pub positional: Vec<String>,
}

impl Flags {
    pub fn parse(args: &[String]) -> Self {
        let mut f = Flags::default();
        let mut i = 0;
        while i < args.len() {
            let a = &args[i];
            if let Some(rest) = a.strip_prefix("--") {
                if rest.is_empty() {
                    i += 1;
                    continue; // a bare `--` separator
                }
                if let Some((k, v)) = rest.split_once('=') {
                    f.map.entry(k.to_string()).or_default().push(v.to_string());
                    i += 1;
                } else if let Some(v) = args.get(i + 1).filter(|v| !v.starts_with("--")) {
                    f.map.entry(rest.to_string()).or_default().push(v.clone());
                    i += 2;
                } else {
                    f.map.entry(rest.to_string()).or_default().push(String::new());
                    i += 1;
                }
            } else {
                f.positional.push(a.clone());
                i += 1;
            }
        }
        f
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        self.map.get(key).and_then(|v| v.last()).map(String::as_str)
    }

    pub fn all(&self, key: &str) -> &[String] {
        self.map.get(key).map(Vec::as_slice).unwrap_or(&[])
    }

    pub fn has(&self, key: &str) -> bool {
        self.map.contains_key(key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn parses_the_documented_heathcliff_syntax() {
        let f = Flags::parse(&s(&["--agent", "Rocket_01", "--force", "1"]));
        assert_eq!(f.get("agent"), Some("Rocket_01"));
        assert_eq!(f.get("force"), Some("1"));
    }

    #[test]
    fn supports_equals_repeats_flags_and_positionals() {
        let f = Flags::parse(&s(&["acme", "--param", "a=1", "--param=b=2", "--dry-run", "--label", "x"]));
        assert_eq!(f.positional, ["acme"]);
        assert_eq!(f.all("param"), ["a=1", "b=2"]);
        assert!(f.has("dry-run"));
        assert_eq!(f.get("label"), Some("x"));
        assert_eq!(f.get("missing"), None);
    }

    #[test]
    fn mutating_classification() {
        assert!(!CliRequest::Status.is_mutating());
        assert!(CliRequest::Heathcliff { command: "force".into(), args: vec![] }.is_mutating());
        assert!(!CliRequest::Heathcliff { command: "list".into(), args: vec![] }.is_mutating());
        assert!(CliRequest::Stop.is_mutating());
    }
}
