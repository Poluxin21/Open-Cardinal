use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::SystemTime;

use tokio::net::TcpListener;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tracing::{info, warn, error};
use serde_json::json;

use crate::cardinal_core::Reaction;
use crate::cli::protocol::{CliRequest, CliResponse, SOCKET_ADDR};
use crate::g_rpc::send_to_queue::{force_reaction, revoke_force};

static START_SECS: std::sync::OnceLock<u64> = std::sync::OnceLock::new();

fn uptime_secs() -> u64 {
    let start = START_SECS.get_or_init(|| {
        SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
    });
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    now.saturating_sub(*start)
}

pub async fn run_command_server(
    active_connections: Arc<AtomicUsize>,
) -> Result<(), Box<dyn std::error::Error>> {
    uptime_secs();

    let listener = TcpListener::bind(SOCKET_ADDR).await?;
    info!("Command server ouvindo em {}", SOCKET_ADDR);

    loop {
        match listener.accept().await {
            Ok((mut stream, addr)) => {
                let connections = active_connections.clone();
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 4096];
                    match stream.read(&mut buf).await {
                        Ok(n) if n > 0 => {
                            let response = match serde_json::from_slice::<CliRequest>(&buf[..n]) {
                                Ok(request) => handle_request(request, connections).await,
                                Err(e) => CliResponse::error(format!("Erro de parsing: {}", e)),
                            };
                            let bytes = serde_json::to_vec(&response).unwrap_or_default();
                            if let Err(e) = stream.write_all(&bytes).await {
                                error!("Erro ao enviar resposta CLI: {}", e);
                            }
                        }
                        Ok(_) => {}
                        Err(e) => warn!("Erro ao ler comando CLI de {}: {}", addr, e),
                    }
                });
            }
            Err(e) => error!("Erro no command server accept: {}", e),
        }
    }
}

async fn handle_request(
    request: CliRequest,
    active_connections: Arc<AtomicUsize>,
) -> CliResponse {
    match request {
        CliRequest::Status => {
            let secs = uptime_secs();
            let conns = active_connections.load(Ordering::Relaxed);
            CliResponse::data(json!({
                "status": "running",
                "uptime": format_duration(secs),
                "uptime_secs": secs,
                "active_connections": conns,
                "pid": std::process::id(),
            }))
        }

        CliRequest::Stats => {
            let secs = uptime_secs();
            let conns = active_connections.load(Ordering::Relaxed);
            let memory_kb = read_process_memory();
            CliResponse::data(json!({
                "pid": std::process::id(),
                "uptime": format_duration(secs),
                "active_connections": conns,
                "memory": format_memory(memory_kb),
                "memory_kb": memory_kb,
            }))
        }

        CliRequest::Reload => {
            info!("Reload solicitado via CLI");
            CliResponse::ok("Configuração recarregada")
        }

        CliRequest::Stop => {
            info!("Shutdown solicitado via CLI");
            #[cfg(unix)]
            unsafe { libc::kill(std::process::id() as i32, libc::SIGTERM); }
            #[cfg(windows)]
            CliResponse::ok("Cardinal encerrando gracefully...");
            std::process::exit(0);
        }

        CliRequest::Heathcliff { command, args } => {
            let flags = parse_flags(&args);

            info!("Exec: {} {:?}", command, args);
            match command.as_str() {
                "revoke_force" => {
                    let agent = flags.get("agent").map(|s| s.as_str()).unwrap_or("default");
                    
                    let _ = revoke_force(agent);

                    CliResponse::Ok { message: "Revoke rule with sucess".to_string() }

                },
                
                "force" => {
                    let agent = flags.get("agent").map(|s| s.as_str()).unwrap_or("default");
                    
                    let force: Option<i32> = flags
                    .get("force")
                    .and_then(|s| s.parse::<i32>().ok());
                    
                    let params = HashMap::new(); 

                    let reaction: Reaction;

                    if let Some(action) = force {
                            if action != 3 {
                                reaction = Reaction {
                                    trace_id: agent.to_string(),
                                    r#type: action,
                                    command_name: "".to_string(),
                                    parameters: params 
                                };

                                
                                force_reaction(reaction).await.unwrap();
                                info!("heathcliff force reaction")
                            } else {
                                // Not Implemented!!
                            }
                    }

                    info!("healthcliff: agent={}", agent);
                    CliResponse::ok(format!("Agent {}", agent))
                }
                
                other     => CliResponse::error(format!("Comando desconhecido: {}", other)),
            }
        }
    }
}

fn format_duration(secs: u64) -> String {
    let h = secs / 3600;
    let m = (secs % 3600) / 60;
    let s = secs % 60;
    if h > 0 { format!("{}h {}m {}s", h, m, s) }
    else if m > 0 { format!("{}m {}s", m, s) }
    else { format!("{}s", s) }
}

fn format_memory(kb: u64) -> String {
    if kb > 1024 * 1024 { format!("{:.1} GB", kb as f64 / 1048576.0) }
    else if kb > 1024   { format!("{:.1} MB", kb as f64 / 1024.0) }
    else                { format!("{} KB", kb) }
}

fn read_process_memory() -> u64 {
    #[cfg(target_os = "linux")]
    {
        let path = format!("/proc/{}/status", std::process::id());
        if let Ok(s) = std::fs::read_to_string(path) {
            for line in s.lines() {
                if line.starts_with("VmRSS:") {
                    return line.split_whitespace().nth(1)
                        .and_then(|v| v.parse().ok()).unwrap_or(0);
                }
            }
        }
    }
    0
}

fn parse_flags(args: &[String]) -> std::collections::HashMap<String, String> {
    let mut map = std::collections::HashMap::new();
    let mut i = 0;
    while i < args.len() {
        if args[i].starts_with("--") {
            let key = args[i].trim_start_matches("--").to_string();
            let value = args.get(i + 1)
                .filter(|v| !v.starts_with("--"))
                .cloned()
                .unwrap_or_default();
            map.insert(key, value);
            i += 2;
        } else {
            i += 1;
        }
    }
    map
}