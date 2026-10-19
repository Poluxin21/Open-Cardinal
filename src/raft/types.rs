//! Raft data types shared by the core, storage and transport.

use std::collections::BTreeSet;
use std::net::IpAddr;

use serde::{Deserialize, Serialize};

use crate::store::Command;

/// A node is identified by the IP address its peers reach it on.
pub type NodeId = IpAddr;
pub type Term = u64;
pub type Index = u64;

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClusterConfig {
    /// Nodes that vote and count towards quorum.
    pub voters: BTreeSet<NodeId>,
    /// Nodes that receive the log but do not vote (catching up before promotion).
    pub learners: BTreeSet<NodeId>,
}

impl ClusterConfig {
    pub fn new(voters: impl IntoIterator<Item = NodeId>) -> Self {
        Self { voters: voters.into_iter().collect(), learners: BTreeSet::new() }
    }

    pub fn contains(&self, id: &NodeId) -> bool {
        self.voters.contains(id) || self.learners.contains(id)
    }

    pub fn is_empty(&self) -> bool {
        self.voters.is_empty() && self.learners.is_empty()
    }

    pub fn quorum(&self) -> usize {
        self.voters.len() / 2 + 1
    }

    /// Everyone that has to receive the log.
    pub fn members(&self) -> impl Iterator<Item = &NodeId> {
        self.voters.iter().chain(self.learners.iter())
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Payload {
    /// Appended by a new leader to commit entries of earlier terms.
    Noop,
    Command(Command),
    /// The complete new membership (single-server change).
    Config(ClusterConfig),
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    pub index: Index,
    pub term: Term,
    pub payload: Payload,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HardState {
    pub term: Term,
    pub voted_for: Option<NodeId>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    Follower,
    PreCandidate,
    Candidate,
    Leader,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Role::Follower => "follower",
            Role::PreCandidate => "pre-candidate",
            Role::Candidate => "candidate",
            Role::Leader => "leader",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct VoteReq {
    pub term: Term,
    pub last_index: Index,
    pub last_term: Term,
    pub transfer: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct VoteResp {
    pub term: Term,
    pub granted: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct AppendReq {
    pub term: Term,
    pub prev_index: Index,
    pub prev_term: Term,
    pub entries: Vec<Entry>,
    pub commit: Index,
}

#[derive(Clone, Debug, PartialEq)]
pub struct AppendResp {
    pub term: Term,
    pub success: bool,
    pub match_index: Index,
    pub hint_index: Index,
}

/// Snapshot header; the state itself travels out-of-band (it is not part of the pure core).
#[derive(Clone, Debug, PartialEq)]
pub struct SnapshotMeta {
    pub term: Term,
    pub last_index: Index,
    pub last_term: Term,
    pub config: ClusterConfig,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SnapshotResp {
    pub term: Term,
    pub success: bool,
    pub last_index: Index,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Message {
    PreVote(VoteReq),
    PreVoteResp(VoteResp),
    Vote(VoteReq),
    VoteResp(VoteResp),
    Append(AppendReq),
    AppendResp(AppendResp),
    /// Leader → follower. The core emits it only as a request for the driver to send
    /// the actual snapshot (see `Ready::send_snapshot`); followers receive it from the driver.
    Snapshot(SnapshotMeta),
    SnapshotResp(SnapshotResp),
    TimeoutNow {
        term: Term,
    },
    /// Local pseudo-message: the transport could not deliver the last message to `from`.
    Unreachable,
}

impl Message {
    pub fn name(&self) -> &'static str {
        match self {
            Message::PreVote(_) => "pre_vote",
            Message::PreVoteResp(_) => "pre_vote_resp",
            Message::Vote(_) => "vote",
            Message::VoteResp(_) => "vote_resp",
            Message::Append(_) => "append",
            Message::AppendResp(_) => "append_resp",
            Message::Snapshot(_) => "snapshot",
            Message::SnapshotResp(_) => "snapshot_resp",
            Message::TimeoutNow { .. } => "timeout_now",
            Message::Unreachable => "unreachable",
        }
    }
}

/// Things the driver should know about, in order.
#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    BecameLeader {
        term: Term,
    },
    BecameFollower {
        term: Term,
        leader: Option<NodeId>,
    },
    LeaderChanged {
        leader: Option<NodeId>,
    },
    /// This node is no longer part of the cluster (its removal was committed).
    Removed,
    ConfigChanged(ClusterConfig),
}

#[derive(Clone, Debug, PartialEq)]
pub struct InstallSnapshot {
    pub from: NodeId,
    pub meta: SnapshotMeta,
}

/// Work produced by the core that the driver must perform, in this order:
/// persist (`hard_state`, `truncate_from`, `append`) → send `messages` → apply `committed`.
#[derive(Debug, Default)]
pub struct Ready {
    pub hard_state: Option<HardState>,
    /// Delete persisted entries with `index >= truncate_from` before appending.
    pub truncate_from: Option<Index>,
    pub append: Vec<Entry>,
    pub messages: Vec<(NodeId, Message)>,
    pub committed: Vec<Entry>,
    /// Followers that need a full snapshot (their next entry was compacted away).
    pub send_snapshot: Vec<NodeId>,
    pub install_snapshot: Option<InstallSnapshot>,
    pub events: Vec<Event>,
}

impl Ready {
    pub fn is_empty(&self) -> bool {
        self.hard_state.is_none()
            && self.truncate_from.is_none()
            && self.append.is_empty()
            && self.messages.is_empty()
            && self.committed.is_empty()
            && self.send_snapshot.is_empty()
            && self.install_snapshot.is_none()
            && self.events.is_empty()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProposeError {
    NotLeader(Option<NodeId>),
    /// Too many uncommitted entries, or a membership change is already in flight.
    Busy(&'static str),
    Invalid(&'static str),
}

impl std::fmt::Display for ProposeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProposeError::NotLeader(Some(l)) => write!(f, "not the leader (leader is {l})"),
            ProposeError::NotLeader(None) => write!(f, "not the leader (no leader elected)"),
            ProposeError::Busy(m) => write!(f, "busy: {m}"),
            ProposeError::Invalid(m) => write!(f, "invalid: {m}"),
        }
    }
}

impl std::error::Error for ProposeError {}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConfChange {
    AddLearner(NodeId),
    Promote(NodeId),
    Remove(NodeId),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReadError {
    NotLeader(Option<NodeId>),
    /// The leader has not committed an entry of its own term yet.
    NotReady,
    /// A majority has not been heard from recently enough to vouch for leadership.
    LeaseExpired,
}
