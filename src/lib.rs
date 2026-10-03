//! Open Cardinal — a deterministic sidecar supervisor for critical systems.

pub mod ai;
pub mod app;
pub mod audit;
pub mod cli;
pub mod cluster;
pub mod config;
pub mod control;
pub mod daemon;
pub mod engine;
pub mod error;
pub mod ext;
pub mod grpc;
pub mod http;
pub mod kernel;
pub mod metrics;
pub mod raft;
pub mod replication;
pub mod runtime;
pub mod store;
pub mod tenant;
pub mod tls;
pub mod util;

/// Generated protobuf/gRPC types.
pub mod pb {
    pub mod core {
        tonic::include_proto!("cardinal.core");
    }
    pub mod raft {
        tonic::include_proto!("cardinal.raft");
    }
}

pub use error::{Error, Result};
