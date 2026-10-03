//! Daemon lifecycle: start every service, wait for a shutdown request, stop cleanly.

use std::sync::Arc;
use std::time::Duration;

use serde_json::json;

use crate::app::App;
use crate::audit::Event;
use crate::config::{Paths, Settings};
use crate::error::Result;
use crate::kernel::{logging, monitor, signals, watcher};
use crate::runtime::Runtime;
use crate::{control, grpc, http};

const DRAIN_TIMEOUT: Duration = Duration::from_secs(5);

pub async fn run(paths: Paths) -> Result<()> {
    run_with(paths, crate::ext::Extensions::default()).await
}

/// Run the daemon with extensions registered (what an extension crate's `main` calls).
pub async fn run_with(paths: Paths, ext: crate::ext::Extensions) -> Result<()> {
    let runtime = Runtime::detect();
    let settings = Settings::load(paths, runtime)?;
    let _log_guard = logging::init(&settings);

    tracing::info!("Started Cardinal General System (runtime: {runtime})");
    let app = App::build_with(settings, ext).await?;
    let started = start_services(&app).await?;

    app.audit.record(Event::new(
        "startup",
        "-",
        json!({
            "version": env!("CARGO_PKG_VERSION"),
            "edition": app.edition,
            "runtime": runtime.as_str(),
            "node": app.node,
            "grpc": started.grpc.iter().map(|a| a.to_string()).collect::<Vec<_>>(),
            "http": started.http.iter().map(|a| a.to_string()).collect::<Vec<_>>(),
            "auth_required": app.tenants.auth_required(),
        }),
    ));
    if app.tenants.auth_required() && !app.tenants.has_any_key() {
        tracing::warn!(
            "authentication is required but no API key exists yet: agents will be refused. Create one with `open-cardinal tenant issue-key default`"
        );
        println!(
            "⚠️  authentication is required but no API key exists yet. Run: open-cardinal tenant issue-key default"
        );
    }

    let signal = tokio::select! {
        s = signals::wait_for_signal() => Some(s),
        _ = app.shutdown.wait() => None,
    };
    if let Some(s) = signal {
        tracing::info!("Shutdown signal received ({s})");
    }
    stop(&app, signal.unwrap_or("control")).await;
    Ok(())
}

pub struct Started {
    pub grpc: Vec<std::net::SocketAddr>,
    pub http: Vec<std::net::SocketAddr>,
    pub control: std::net::SocketAddr,
}

/// Spawn gRPC, HTTP, control server, monitor and hot-reload watcher. Used by the daemon
/// and by integration tests.
pub async fn start_services(app: &Arc<App>) -> Result<Started> {
    let grpc_addrs = grpc::serve(app.clone()).await?;
    println!("Cardinal gRPC Server listening on {}", join(&grpc_addrs));
    let http_addrs = http::serve(app.clone()).await?;
    let control_addr = control::server::serve(app.clone()).await?;

    tokio::spawn(monitor::run(app.clone()));
    spawn_hot_reload(app.clone());
    Ok(Started { grpc: grpc_addrs, http: http_addrs, control: control_addr })
}

fn join(addrs: &[std::net::SocketAddr]) -> String {
    addrs.iter().map(|a| a.to_string()).collect::<Vec<_>>().join(", ")
}

fn spawn_hot_reload(app: Arc<App>) {
    match watcher::watch(&app.settings.paths) {
        Ok(mut w) => {
            tokio::spawn(async move {
                loop {
                    let changed = tokio::select! {
                        c = w.next_change() => c,
                        _ = app.shutdown.wait() => false,
                    };
                    if !changed {
                        break;
                    }
                    if let Err(e) = app.reload().await {
                        tracing::error!("hot reload failed: {e}");
                    }
                }
            });
        }
        Err(e) => tracing::warn!("hot reload disabled (cannot watch files: {e}); use `open-cardinal reload`"),
    }
}

/// Stop accepting work, let in-flight pulses finish, make the audit trail durable.
pub async fn stop(app: &Arc<App>, reason: &str) {
    // a leader hands over first, so a rolling restart does not cost an election timeout
    if let Some(raft) = &app.raft {
        raft.step_down().await;
    }
    app.shutdown.trigger();
    let deadline = tokio::time::Instant::now() + DRAIN_TIMEOUT;
    while app.metrics.in_flight() > 0 && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    app.audit.record(Event::new("shutdown", "-", json!({ "reason": reason, "uptime_secs": app.uptime_secs() })));
    app.audit.flush().await;
    tracing::info!("Cardinal stopped");
}

/// `open-cardinal health`: GET /readyz on the local daemon; returns the process exit code.
pub async fn health_probe(home: Option<&std::path::Path>) -> i32 {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let paths = Paths::resolve(home);
    let port = std::env::var("CARDINAL_HTTP_PORT")
        .ok()
        .and_then(|v| v.trim().parse::<u16>().ok())
        .or_else(|| {
            std::fs::read_to_string(paths.config_file())
                .ok()
                .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
                .and_then(|v| v.get("http_port")?.as_u64())
                .map(|p| p as u16)
        })
        .unwrap_or(crate::config::DEFAULT_HTTP_PORT);
    // the daemon may listen on loopback only, on a specific address, or everywhere
    let candidates = [
        std::net::SocketAddr::from((std::net::Ipv4Addr::LOCALHOST, port)),
        std::net::SocketAddr::from((std::net::Ipv6Addr::LOCALHOST, port)),
    ];
    for addr in candidates {
        let Ok(Ok(mut s)) = tokio::time::timeout(Duration::from_secs(2), tokio::net::TcpStream::connect(addr)).await
        else {
            continue;
        };
        let req = format!("GET /readyz HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n");
        if s.write_all(req.as_bytes()).await.is_err() {
            continue;
        }
        let mut buf = Vec::new();
        let _ = tokio::time::timeout(Duration::from_secs(3), s.read_to_end(&mut buf)).await;
        let text = String::from_utf8_lossy(&buf);
        return if text.starts_with("HTTP/1.1 200") {
            0
        } else {
            eprintln!("not ready: {}", text.lines().next().unwrap_or(""));
            1
        };
    }
    eprintln!("the daemon is not answering on port {port}");
    1
}
