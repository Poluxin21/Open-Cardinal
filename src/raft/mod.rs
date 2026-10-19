//! Raft consensus: replicated shared memory for Cardinal clusters.
//!
//! * [`core`] — the pure state machine (election, replication, membership, snapshots)
//! * [`storage`] — log and hard state in the same redb file as the replicated state
//! * [`transport`] / [`auth`] — authenticated gRPC between peers
//! * [`node`] — the async driver and the [`RaftHandle`] the rest of the process uses
//! * [`discovery`] — from a list of bare IPs to a running node (self-detection, found-or-join)
//! * [`config`] — `config/raft.json`

pub mod auth;
pub mod config;
pub mod core;
pub mod discovery;
pub mod node;
pub mod storage;
pub mod transport;
pub mod types;

#[cfg(test)]
mod sim;

pub use config::RaftSettings;
pub use node::{RaftHandle, Status as RaftStatus};
