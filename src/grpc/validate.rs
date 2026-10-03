//! Input validation for pulses. Everything here is attacker-controlled.

use crate::config::Limits;
use crate::error::{Error, Result};
use crate::pb::core::Pulse;

pub const MAX_AGENT_ID_LEN: usize = 256;
pub const MAX_TELEMETRY_KEY_LEN: usize = 128;

fn has_control_chars(s: &str) -> bool {
    s.chars().any(|c| c.is_control())
}

pub fn validate_pulse(p: &Pulse, limits: &Limits) -> Result<()> {
    if p.agent_id.is_empty() {
        return Err(Error::invalid("agent_id is required"));
    }
    if p.agent_id.len() > MAX_AGENT_ID_LEN {
        return Err(Error::invalid(format!("agent_id is longer than {MAX_AGENT_ID_LEN} bytes")));
    }
    if has_control_chars(&p.agent_id) {
        return Err(Error::invalid("agent_id must not contain control characters"));
    }
    if p.telemetry.len() > limits.max_telemetry_entries {
        return Err(Error::invalid(format!("telemetry has more than {} entries", limits.max_telemetry_entries)));
    }
    let mut bytes = 0usize;
    for (k, v) in &p.telemetry {
        if k.is_empty() || k.len() > MAX_TELEMETRY_KEY_LEN || has_control_chars(k) {
            return Err(Error::invalid("telemetry keys must be 1-128 bytes without control characters"));
        }
        bytes += k.len() + v.len();
    }
    if bytes > limits.max_telemetry_bytes {
        return Err(Error::invalid(format!("telemetry is larger than {} bytes", limits.max_telemetry_bytes)));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn pulse(agent: &str, t: &[(&str, &str)]) -> Pulse {
        Pulse {
            agent_id: agent.into(),
            timestamp: 0,
            telemetry: t.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect::<HashMap<_, _>>(),
        }
    }

    #[test]
    fn accepts_the_normal_case() {
        validate_pulse(&pulse("Rocket_01", &[("fuel", "80%")]), &Limits::default()).unwrap();
    }

    #[test]
    fn rejects_bad_agent_ids() {
        let l = Limits::default();
        assert!(validate_pulse(&pulse("", &[]), &l).is_err());
        assert!(validate_pulse(&pulse(&"a".repeat(257), &[]), &l).is_err());
        assert!(validate_pulse(&pulse("a\nb", &[]), &l).is_err(), "log-forging newline");
        assert!(validate_pulse(&pulse("a\x1b[31m", &[]), &l).is_err());
    }

    #[test]
    fn enforces_telemetry_limits() {
        let l = Limits { max_telemetry_entries: 2, max_telemetry_bytes: 20, ..Limits::default() };
        assert!(validate_pulse(&pulse("a", &[("a", "1"), ("b", "2"), ("c", "3")]), &l).is_err());
        assert!(validate_pulse(&pulse("a", &[("a", &"x".repeat(30))]), &l).is_err());
        assert!(validate_pulse(&pulse("a", &[("", "1")]), &l).is_err());
        assert!(validate_pulse(&pulse("a", &[("a\n", "1")]), &l).is_err());
    }
}
