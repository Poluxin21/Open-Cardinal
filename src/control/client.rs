//! CLI side of the control plane.

use std::net::SocketAddr;
use std::path::Path;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use super::protocol::{CliRequest, CliResponse, Envelope, MAX_RESPONSE_BYTES};
use crate::config::{DEFAULT_CONTROL_ADDR, Paths};

pub struct Target {
    pub addr: SocketAddr,
    pub token: String,
}

/// Where the daemon listens and the secret to talk to it.
///
/// Token: `--token` / `CARDINAL_TOKEN` → `CARDINAL_ADMIN_TOKEN` → `<home>/config/admin.token`.
/// The control address is read from `<home>/config/config.json` (never created or modified here).
pub fn resolve_target(home: Option<&Path>, token: Option<&str>) -> Result<Target, String> {
    let paths = Paths::resolve(home);
    let addr = control_addr(&paths);

    let token = match token
        .map(str::to_string)
        .or_else(|| std::env::var("CARDINAL_ADMIN_TOKEN").ok())
        .or_else(|| std::env::var("CARDINAL_ADMIN_TOKEN_FILE").ok().and_then(|f| std::fs::read_to_string(f).ok()))
    {
        Some(t) if !t.trim().is_empty() => t.trim().to_string(),
        _ => {
            let file = paths.admin_token_file();
            std::fs::read_to_string(&file).map(|t| t.trim().to_string()).map_err(|_| {
                format!(
                    "cannot read the admin token ({}). Run the CLI from the daemon's home directory, \
                     pass --home <dir>, or provide --token / CARDINAL_TOKEN.",
                    file.display()
                )
            })?
        }
    };
    Ok(Target { addr, token })
}

fn control_addr(paths: &Paths) -> SocketAddr {
    if let Ok(v) = std::env::var("CARDINAL_CONTROL_ADDR")
        && let Ok(a) = v.trim().parse()
    {
        return a;
    }
    std::fs::read_to_string(paths.config_file())
        .ok()
        .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
        .and_then(|v| v.get("control_addr")?.as_str()?.parse().ok())
        .unwrap_or_else(|| DEFAULT_CONTROL_ADDR.parse().expect("valid default"))
}

pub async fn send(target: &Target, request: CliRequest) -> Result<CliResponse, String> {
    let exchange = async {
        let mut stream = TcpStream::connect(target.addr).await.map_err(|_| {
            format!(
                "❌ Não foi possível conectar ao Cardinal daemon em {}.\n   Verifique se o daemon está rodando.",
                target.addr
            )
        })?;
        let envelope = Envelope { token: target.token.clone(), request };
        let bytes = serde_json::to_vec(&envelope).map_err(|e| e.to_string())?;
        stream.write_all(&bytes).await.map_err(|e| e.to_string())?;
        stream.shutdown().await.map_err(|e| e.to_string())?;

        let mut buf = Vec::new();
        (&mut stream).take(MAX_RESPONSE_BYTES as u64).read_to_end(&mut buf).await.map_err(|e| e.to_string())?;
        serde_json::from_slice::<CliResponse>(&buf).map_err(|e| format!("invalid response from daemon: {e}"))
    };
    tokio::time::timeout(Duration::from_secs(30), exchange)
        .await
        .map_err(|_| "timed out waiting for the daemon".to_string())?
}

/// Pretty-print and return the process exit code.
pub fn print_response(response: CliResponse) -> i32 {
    match response {
        CliResponse::Ok { message } => {
            println!("✅ {message}");
            0
        }
        CliResponse::Error { message } => {
            eprintln!("❌ {message}");
            1
        }
        CliResponse::Data { payload } => {
            print_data(&payload);
            0
        }
    }
}

fn print_data(payload: &serde_json::Value) {
    let Some(obj) = payload.as_object() else {
        println!("{}", serde_json::to_string_pretty(payload).unwrap_or_default());
        return;
    };
    // flat scalars render as the original box; anything nested is pretty JSON
    if obj.values().all(|v| !v.is_object() && !v.is_array()) {
        println!("┌─────────────────────────────────────────┐");
        for (key, value) in obj {
            let val = match value {
                serde_json::Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            println!("│  {:<22} {}", format!("{key}:"), val);
        }
        println!("└─────────────────────────────────────────┘");
    } else {
        println!("{}", serde_json::to_string_pretty(payload).unwrap_or_default());
    }
}
