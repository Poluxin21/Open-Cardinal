//! Execution and traffic limits. Global defaults live in `config.json`; every tenant can
//! override any subset in `tenants.json` (see [`LimitsPatch`]).

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Limits {
    /// Max `telemetry` map entries per pulse.
    pub max_telemetry_entries: usize,
    /// Max total bytes of telemetry keys + values per pulse.
    pub max_telemetry_bytes: usize,
    /// Wall-clock budget for *all* rules of one pulse (safety net; the deterministic
    /// budgets below normally trip first).
    pub rule_timeout_ms: u64,
    /// Lua: heap limit per evaluation.
    pub lua_memory_bytes: usize,
    /// Lua: VM instruction budget per rule.
    pub lua_instructions: u64,
    /// WASM: fuel (≈ instructions) per rule.
    pub wasm_fuel: u64,
    /// WASM: linear memory limit per rule.
    pub wasm_memory_bytes: usize,
    /// Budget for AI (ONNX / prompt) rules of one pulse.
    pub ai_timeout_ms: u64,
    /// Concurrent rule evaluations (0 = 2 × CPU cores).
    pub max_concurrent_evaluations: usize,
    /// Sustained pulses per second (0 = unlimited).
    pub rate_limit_per_sec: u32,
    /// Burst size for the rate limiter (0 = same as the rate).
    pub rate_limit_burst: u32,
    /// Max keys of shared memory a tenant may hold.
    pub kv_max_entries: u64,
    /// Max shared-memory writes one pulse evaluation may issue.
    pub kv_max_writes_per_eval: u32,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_telemetry_entries: 256,
            max_telemetry_bytes: 64 * 1024,
            rule_timeout_ms: 250,
            lua_memory_bytes: 16 * 1024 * 1024,
            lua_instructions: 5_000_000,
            wasm_fuel: 20_000_000,
            wasm_memory_bytes: 16 * 1024 * 1024,
            ai_timeout_ms: 2_000,
            max_concurrent_evaluations: 0,
            rate_limit_per_sec: 0,
            rate_limit_burst: 0,
            kv_max_entries: 100_000,
            kv_max_writes_per_eval: 64,
        }
    }
}

impl Limits {
    pub fn validate(&self) -> Result<()> {
        if self.max_telemetry_entries == 0 || self.max_telemetry_bytes == 0 {
            return Err(Error::config("limits: telemetry limits must be non-zero"));
        }
        if self.rule_timeout_ms == 0 || self.ai_timeout_ms == 0 {
            return Err(Error::config("limits: timeouts must be non-zero"));
        }
        if self.lua_instructions == 0 || self.wasm_fuel == 0 {
            return Err(Error::config("limits: instruction budgets must be non-zero"));
        }
        Ok(())
    }

    pub fn concurrency(&self) -> usize {
        if self.max_concurrent_evaluations > 0 {
            self.max_concurrent_evaluations
        } else {
            std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4) * 2
        }
    }
}

/// Partial limits as written in a tenant definition.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LimitsPatch {
    pub max_telemetry_entries: Option<usize>,
    pub max_telemetry_bytes: Option<usize>,
    pub rule_timeout_ms: Option<u64>,
    pub lua_memory_bytes: Option<usize>,
    pub lua_instructions: Option<u64>,
    pub wasm_fuel: Option<u64>,
    pub wasm_memory_bytes: Option<usize>,
    pub ai_timeout_ms: Option<u64>,
    pub max_concurrent_evaluations: Option<usize>,
    pub rate_limit_per_sec: Option<u32>,
    pub rate_limit_burst: Option<u32>,
    pub kv_max_entries: Option<u64>,
    pub kv_max_writes_per_eval: Option<u32>,
}

impl LimitsPatch {
    pub fn apply(&self, base: &Limits) -> Limits {
        Limits {
            max_telemetry_entries: self.max_telemetry_entries.unwrap_or(base.max_telemetry_entries),
            max_telemetry_bytes: self.max_telemetry_bytes.unwrap_or(base.max_telemetry_bytes),
            rule_timeout_ms: self.rule_timeout_ms.unwrap_or(base.rule_timeout_ms),
            lua_memory_bytes: self.lua_memory_bytes.unwrap_or(base.lua_memory_bytes),
            lua_instructions: self.lua_instructions.unwrap_or(base.lua_instructions),
            wasm_fuel: self.wasm_fuel.unwrap_or(base.wasm_fuel),
            wasm_memory_bytes: self.wasm_memory_bytes.unwrap_or(base.wasm_memory_bytes),
            ai_timeout_ms: self.ai_timeout_ms.unwrap_or(base.ai_timeout_ms),
            max_concurrent_evaluations: self.max_concurrent_evaluations.unwrap_or(base.max_concurrent_evaluations),
            rate_limit_per_sec: self.rate_limit_per_sec.unwrap_or(base.rate_limit_per_sec),
            rate_limit_burst: self.rate_limit_burst.unwrap_or(base.rate_limit_burst),
            kv_max_entries: self.kv_max_entries.unwrap_or(base.kv_max_entries),
            kv_max_writes_per_eval: self.kv_max_writes_per_eval.unwrap_or(base.kv_max_writes_per_eval),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn patch_overrides_only_what_it_sets() {
        let base = Limits::default();
        let patch = LimitsPatch { wasm_fuel: Some(10), rate_limit_per_sec: Some(5), ..Default::default() };
        let merged = patch.apply(&base);
        assert_eq!(merged.wasm_fuel, 10);
        assert_eq!(merged.rate_limit_per_sec, 5);
        assert_eq!(merged.lua_instructions, base.lua_instructions);
    }

    #[test]
    fn zero_budgets_are_rejected() {
        let l = Limits { lua_instructions: 0, ..Default::default() };
        assert!(l.validate().is_err());
    }
}
