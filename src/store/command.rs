//! Replicated commands and the values they carry.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// A state-machine command. This is exactly what is written to the Raft log.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Command {
    /// Does nothing; a new leader appends one to commit entries of previous terms.
    Noop,
    KvSet {
        tenant: String,
        key: String,
        value: u64,
    },
    /// Atomic add (cluster-safe counter). The result saturates in `0..=u64::MAX`.
    KvAdd {
        tenant: String,
        key: String,
        delta: i64,
    },
    KvDelete {
        tenant: String,
        key: String,
    },
    /// Heathcliff: force a reaction for an agent, overriding its rules.
    ForceSet {
        tenant: String,
        agent: String,
        reaction: ForcedReaction,
    },
    ForceClear {
        tenant: String,
        agent: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForcedReaction {
    /// `Reaction.ActionType` as an integer (0 IDLE, 1 SHUTDOWN, 2 RESTART, 3 CUSTOM).
    pub kind: i32,
    #[serde(default)]
    pub command_name: String,
    #[serde(default)]
    pub params: BTreeMap<String, String>,
    #[serde(default)]
    pub set_at_ms: u64,
    /// Unix ms after which the override no longer applies (`None` = until revoked).
    #[serde(default)]
    pub expires_at_ms: Option<u64>,
}

impl ForcedReaction {
    pub fn new(kind: i32) -> Self {
        Self {
            kind,
            command_name: String::new(),
            params: BTreeMap::new(),
            set_at_ms: crate::util::now_ms(),
            expires_at_ms: None,
        }
    }

    pub fn is_expired(&self, now_ms: u64) -> bool {
        self.expires_at_ms.is_some_and(|t| now_ms >= t)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    None,
    Value(u64),
    Deleted(bool),
}

/// Full replicated state, used for Raft snapshots.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct StateDump {
    pub kv: Vec<(String, String, u64)>,
    /// `"<tenant>\u{1f}<agent>"` → reaction.
    pub forced: BTreeMap<String, ForcedReaction>,
}
