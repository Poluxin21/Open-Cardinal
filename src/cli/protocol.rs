use serde::{Deserialize, Serialize};

pub const SOCKET_ADDR: &str = "127.0.0.1:19876";

#[derive(Debug, Serialize, Deserialize, Clone)]
pub enum CliRequest {
    Status,
    Stop,
    Reload,
    Stats,
    Exec { command: String, args: Vec<String> },
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