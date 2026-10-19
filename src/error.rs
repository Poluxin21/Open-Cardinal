//! Crate-wide error type.

use std::fmt;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("configuration error: {0}")]
    Config(String),

    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),

    #[error("storage error: {0}")]
    Storage(String),

    #[error("rule error: {0}")]
    Rule(String),

    #[error("invalid input: {0}")]
    Invalid(String),

    #[error("unauthenticated: {0}")]
    Unauthenticated(String),

    #[error("permission denied: {0}")]
    PermissionDenied(String),

    #[error("rate limit exceeded")]
    RateLimited,

    /// Concurrency gate exhausted; the caller may retry.
    #[error("overloaded: {0}")]
    Overloaded(String),

    #[error("cluster error: {0}")]
    Cluster(String),

    /// The local node is not the Raft leader; carries the leader hint when known.
    #[error("not the leader (leader: {0:?})")]
    NotLeader(Option<String>),

    #[error("{0}")]
    Other(String),
}

impl Error {
    pub fn config(msg: impl fmt::Display) -> Self {
        Self::Config(msg.to_string())
    }
    pub fn rule(msg: impl fmt::Display) -> Self {
        Self::Rule(msg.to_string())
    }
    pub fn invalid(msg: impl fmt::Display) -> Self {
        Self::Invalid(msg.to_string())
    }
    pub fn cluster(msg: impl fmt::Display) -> Self {
        Self::Cluster(msg.to_string())
    }
}

macro_rules! storage_from {
    ($($t:ty),* $(,)?) => {
        $(impl From<$t> for Error {
            fn from(e: $t) -> Self { Error::Storage(e.to_string()) }
        })*
    };
}

storage_from!(
    redb::Error,
    redb::DatabaseError,
    redb::TransactionError,
    redb::TableError,
    redb::StorageError,
    redb::CommitError,
);

impl From<serde_json::Error> for Error {
    fn from(e: serde_json::Error) -> Self {
        Error::Invalid(format!("json: {e}"))
    }
}
