//! Local control server: the daemon side of `open-cardinal <command>`.
//!
//! Hardened compared with the original: authenticated (admin token, constant-time compare),
//! bounded request size, read timeout, bounded concurrency, loopback only, and every
//! mutating command lands in the audit trail.

use std::sync::Arc;
use std::time::Duration;

use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Semaphore;

use super::protocol::{CliRequest, CliResponse, Envelope, Flags, MAX_REQUEST_BYTES};
use crate::app::App;
use crate::audit::{Event, Query};
use crate::store::{Command, DEFAULT_TENANT, ForcedReaction};
use crate::util;

const READ_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_CONNECTIONS: usize = 32;

pub async fn serve(app: Arc<App>) -> crate::Result<std::net::SocketAddr> {
    let addr = app.settings.config.control_addr;
    let listener = TcpListener::bind(addr).await.map_err(|e| {
        crate::Error::Other(format!("control server cannot listen on {addr}: {e} (is another Cardinal running?)"))
    })?;
    let local = listener.local_addr()?;
    tracing::info!("Command server listening on {local}");
    let gate = Arc::new(Semaphore::new(MAX_CONNECTIONS));

    tokio::spawn(async move {
        loop {
            let accepted = tokio::select! {
                r = listener.accept() => r,
                _ = app.shutdown.wait() => break,
            };
            match accepted {
                Ok((stream, peer)) => {
                    let Ok(permit) = gate.clone().try_acquire_owned() else {
                        tracing::warn!("control server saturated; dropping connection from {peer}");
                        continue;
                    };
                    let app = app.clone();
                    tokio::spawn(async move {
                        let _permit = permit;
                        if let Err(e) = handle_connection(app, stream).await {
                            tracing::debug!("control connection from {peer}: {e}");
                        }
                    });
                }
                Err(e) => {
                    tracing::error!("control accept failed: {e}");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            }
        }
    });
    Ok(local)
}

async fn handle_connection(app: Arc<App>, mut stream: TcpStream) -> std::io::Result<()> {
    let mut buf = Vec::new();
    let read =
        tokio::time::timeout(READ_TIMEOUT, (&mut stream).take(MAX_REQUEST_BYTES as u64 + 1).read_to_end(&mut buf))
            .await;
    match read {
        Ok(Ok(_)) => {}
        Ok(Err(e)) => return Err(e),
        Err(_) => return Ok(()), // slow client: just hang up
    }
    if buf.len() > MAX_REQUEST_BYTES {
        return reply(&mut stream, &CliResponse::error("request too large")).await;
    }

    let envelope: Envelope = match serde_json::from_slice(&buf) {
        Ok(e) => e,
        Err(_) => {
            // includes the legacy un-enveloped protocol: tell the user why it no longer works
            return reply(
                &mut stream,
                &CliResponse::error(
                    "unauthenticated or malformed request; use the open-cardinal CLI from the daemon's home directory",
                ),
            )
            .await;
        }
    };
    if !app.admin.verify(&envelope.token) {
        app.metrics.observe_rejected(None, false);
        app.audit_rejected(None, "control plane: bad token", None, stream.peer_addr().ok().map(|a| a.to_string()));
        tokio::time::sleep(Duration::from_millis(150)).await; // blunt brute-force speed bump
        return reply(&mut stream, &CliResponse::error("authentication failed")).await;
    }

    let request = envelope.request;
    let name = request.audit_name();
    let mutating = request.is_mutating();
    let stop = matches!(request, CliRequest::Stop);
    let response = handle_request(&app, request).await;
    if mutating {
        let ok = !matches!(response, CliResponse::Error { .. });
        app.audit.record(Event::new("control", "-", json!({ "command": name, "ok": ok })));
    }
    reply(&mut stream, &response).await?;
    if stop {
        // answer first, then stop: the original called `process::exit` mid-request
        tracing::info!("shutdown requested via CLI");
        app.shutdown.trigger();
    }
    Ok(())
}

async fn reply(stream: &mut TcpStream, response: &CliResponse) -> std::io::Result<()> {
    let bytes = serde_json::to_vec(response).unwrap_or_default();
    stream.write_all(&bytes).await?;
    stream.shutdown().await
}

pub async fn handle_request(app: &Arc<App>, request: CliRequest) -> CliResponse {
    match request {
        CliRequest::Status => status(app),
        CliRequest::Stats => stats(app),
        CliRequest::Reload => match app.reload().await {
            Ok(set) => CliResponse::data(json!({
                "message": "Configuração recarregada",
                "rules": set.total_rules(),
                "agents": set.total_agent_dirs(),
                "issues": set.issues,
            })),
            Err(e) => CliResponse::error(format!("reload failed: {e}")),
        },
        CliRequest::Stop => CliResponse::ok("Cardinal encerrando gracefully..."),
        CliRequest::Heathcliff { command, args } => heathcliff(app, &command, &args).await,
        CliRequest::Tenant { command, args } => tenant(app, &command, &args).await,
        CliRequest::Rules { command, args } => rules(app, &command, &args),
        CliRequest::Audit { command, args } => audit(app, &command, &args).await,
        CliRequest::Raft { command, args } => crate::cluster::control(app, &command, &args).await,
    }
}

fn status(app: &Arc<App>) -> CliResponse {
    let set = app.engine.registry().current();
    CliResponse::data(json!({
        "status": "running",
        "uptime": format_duration(app.uptime_secs()),
        "uptime_secs": app.uptime_secs(),
        "active_connections": app.metrics.in_flight(),
        "pid": std::process::id(),
        "node": app.node,
        "runtime": app.settings.runtime.as_str(),
        "edition": app.edition,
        "ai": crate::ai::available(),
        "auth_required": app.tenants.auth_required(),
        "tenants": app.tenants.list().len(),
        "rules": set.total_rules(),
        "rule_issues": set.issues.len(),
        "audit_records": app.audit.stats().records,
        "cluster": crate::cluster::headline(app),
    }))
}

fn stats(app: &Arc<App>) -> CliResponse {
    let memory_kb = process_memory_kb();
    CliResponse::data(json!({
        "pid": std::process::id(),
        "uptime": format_duration(app.uptime_secs()),
        "active_connections": app.metrics.in_flight(),
        "memory": format_memory(memory_kb),
        "memory_kb": memory_kb,
        "pulses": app.metrics.pulses.load(std::sync::atomic::Ordering::Relaxed),
        "rejected": app.metrics.rejected.load(std::sync::atomic::Ordering::Relaxed),
        "latency_p50_us": app.metrics.latency_quantile_us(0.5),
        "latency_p99_us": app.metrics.latency_quantile_us(0.99),
    }))
}

fn tenant_or_default(f: &Flags) -> &str {
    f.get("tenant").filter(|t| !t.is_empty()).unwrap_or(DEFAULT_TENANT)
}

async fn heathcliff(app: &Arc<App>, command: &str, args: &[String]) -> CliResponse {
    let f = Flags::parse(args);
    let tenant = tenant_or_default(&f).to_string();
    if app.tenants.get(&tenant).is_none() {
        return CliResponse::error(format!("unknown tenant '{}'", util::log_safe(&tenant)));
    }
    match command {
        "force" => {
            // The original silently did nothing when --agent/--force were missing or
            // invalid and answered "ok". A safety override must never fail quietly.
            let Some(agent) = f.get("agent").filter(|a| !a.is_empty()) else {
                return CliResponse::error("missing --agent <id>");
            };
            if agent.len() > 256 || agent.chars().any(|c| c.is_control()) {
                return CliResponse::error("invalid agent id");
            }
            let Some(kind) = f.get("force").and_then(|s| s.parse::<i32>().ok()).filter(|k| (0..=3).contains(k)) else {
                return CliResponse::error(
                    "missing or invalid --force (0 = IDLE, 1 = SHUTDOWN, 2 = RESTART, 3 = CUSTOM)",
                );
            };
            let mut reaction = ForcedReaction::new(kind);
            if kind == 3 {
                let Some(cmd) = f.get("cmd").filter(|c| !c.is_empty()) else {
                    return CliResponse::error("--force 3 (CUSTOM) requires --cmd <name>");
                };
                reaction.command_name = cmd.to_string();
            }
            for p in f.all("param") {
                match p.split_once('=') {
                    Some((k, v)) if !k.is_empty() => {
                        reaction.params.insert(k.to_string(), v.to_string());
                    }
                    _ => return CliResponse::error(format!("--param expects key=value, got '{}'", util::log_safe(p))),
                }
            }
            if let Some(ttl) = f.get("ttl") {
                match ttl.parse::<u64>() {
                    Ok(s) if s > 0 => reaction.expires_at_ms = Some(util::now_ms() + s * 1000),
                    _ => return CliResponse::error("--ttl expects a positive number of seconds"),
                }
            }
            let name = ["IDLE", "SHUTDOWN", "RESTART", "CUSTOM"][kind as usize];
            match app
                .replicator
                .submit(Command::ForceSet { tenant: tenant.clone(), agent: agent.to_string(), reaction })
                .await
            {
                Ok(_) => CliResponse::ok(format!("Agent {agent} forced to {name} (tenant {tenant})")),
                Err(e) => CliResponse::error(format!("could not apply override: {e}")),
            }
        }
        "revoke_force" => {
            let Some(agent) = f.get("agent").filter(|a| !a.is_empty()) else {
                return CliResponse::error("missing --agent <id>");
            };
            match app.replicator.submit(Command::ForceClear { tenant, agent: agent.to_string() }).await {
                Ok(crate::store::Outcome::Deleted(true)) => {
                    CliResponse::ok(format!("Override for agent {agent} revoked"))
                }
                Ok(_) => CliResponse::ok(format!("Agent {agent} had no override")),
                Err(e) => CliResponse::error(format!("could not revoke override: {e}")),
            }
        }
        "list" => match app.store.forced_list() {
            Ok(rows) => {
                let now = util::now_ms();
                CliResponse::data(json!({
                    "overrides": rows.into_iter().map(|(t, a, r)| json!({
                        "tenant": t, "agent": a, "type": r.kind, "command": r.command_name,
                        "params": r.params, "expired": r.is_expired(now), "set_at_ms": r.set_at_ms,
                    })).collect::<Vec<_>>()
                }))
            }
            Err(e) => CliResponse::error(e.to_string()),
        },
        other => CliResponse::error(format!("Comando desconhecido: {other} (use force | revoke_force | list)")),
    }
}

async fn tenant(app: &Arc<App>, command: &str, args: &[String]) -> CliResponse {
    let f = Flags::parse(args);
    let first = f.positional.first().map(String::as_str);
    let reload_rules = |app: &Arc<App>| {
        let app = app.clone();
        tokio::spawn(async move {
            let _ = app.reload().await;
        });
    };
    match (command, first) {
        ("list", _) => {
            let file = match app.tenants.file() {
                Ok(f) => f,
                Err(e) => return CliResponse::error(e.to_string()),
            };
            let rows: Vec<_> = app
                .tenants
                .list()
                .iter()
                .map(|t| {
                    let keys: Vec<&str> = file
                        .tenants
                        .iter()
                        .find(|d| d.id == t.id)
                        .map(|d| d.api_keys.iter().map(|k| k.label.as_str()).collect())
                        .unwrap_or_default();
                    json!({
                        "id": t.id, "enabled": t.enabled, "keys": keys,
                        "lua": t.lua_enabled, "wasm": t.wasm_enabled, "ai": t.ai.enabled,
                        "rate_limit_per_sec": t.limits.rate_limit_per_sec,
                        "rules_dir": t.rules_dir.display().to_string(),
                    })
                })
                .collect();
            CliResponse::data(json!({ "tenants": rows }))
        }
        ("add", Some(id)) => match app.tenants.edit(|f| f.add_tenant(id)) {
            Ok(()) => {
                reload_rules(app);
                CliResponse::ok(format!(
                    "Tenant '{id}' created. Add rules under tenants/{id}/rules/ and issue a key with: tenant issue-key {id} --label <name>"
                ))
            }
            Err(e) => CliResponse::error(e.to_string()),
        },
        ("issue-key", Some(id)) => {
            let label = f.get("label").unwrap_or("default").to_string();
            let prefix = f.get("agent-prefix").map(str::to_string);
            match app.tenants.edit(|file| file.issue_key(id, &label, prefix.clone())) {
                Ok(key) => {
                    reload_rules(app);
                    CliResponse::data(json!({
                        "tenant": id, "label": label, "api_key": key,
                        "note": "store this key now; only its SHA-256 is kept and it cannot be shown again",
                    }))
                }
                Err(e) => CliResponse::error(e.to_string()),
            }
        }
        ("revoke-key", Some(id)) => {
            let Some(label) = f.positional.get(1).map(String::as_str).or(f.get("label")) else {
                return CliResponse::error("usage: tenant revoke-key <tenant> <label>");
            };
            match app.tenants.edit(|file| file.revoke_key(id, label)) {
                Ok(()) => CliResponse::ok(format!("Key '{label}' of tenant '{id}' revoked")),
                Err(e) => CliResponse::error(e.to_string()),
            }
        }
        ("enable", Some(id)) | ("disable", Some(id)) => {
            let enabled = command == "enable";
            match app.tenants.edit(|file| file.set_enabled(id, enabled)) {
                Ok(()) => {
                    reload_rules(app);
                    CliResponse::ok(format!("Tenant '{id}' {}", if enabled { "enabled" } else { "disabled" }))
                }
                Err(e) => CliResponse::error(e.to_string()),
            }
        }
        _ => CliResponse::error(
            "usage: tenant list | add <id> | issue-key <id> [--label L] [--agent-prefix P] | revoke-key <id> <label> | enable|disable <id>",
        ),
    }
}

fn rules(app: &Arc<App>, command: &str, args: &[String]) -> CliResponse {
    let f = Flags::parse(args);
    let set = app.engine.registry().current();
    match command {
        "list" => {
            let only = f.get("tenant");
            let mut rows = Vec::new();
            for (tenant, tr) in &set.tenants {
                if only.is_some_and(|t| t != tenant) {
                    continue;
                }
                let mut push = |scope: &str, rs: &[Arc<crate::engine::Rule>]| {
                    for r in rs {
                        rows.push(json!({ "tenant": tenant, "scope": scope, "rule": r.name, "kind": r.kind.as_str(), "priority_cap": r.priority_cap }));
                    }
                };
                if let Some(d) = &tr.default {
                    push("default", d);
                }
                let mut agents: Vec<_> = tr.agents.iter().collect();
                agents.sort_by(|a, b| a.0.cmp(b.0));
                for (agent, rs) in agents {
                    push(agent, rs);
                }
            }
            CliResponse::data(json!({ "generation": set.generation, "total": set.total_rules(), "rules": rows }))
        }
        "issues" => CliResponse::data(json!({ "issues": set.issues })),
        other => CliResponse::error(format!("Comando desconhecido: {other} (use list | issues)")),
    }
}

async fn audit(app: &Arc<App>, command: &str, args: &[String]) -> CliResponse {
    // reads see everything queued so far
    app.audit.flush().await;
    let f = Flags::parse(args);
    match command {
        "verify" => match app.audit.verify() {
            Ok(rep) => {
                let ok = rep.ok;
                let payload = serde_json::to_value(&rep).unwrap_or_default();
                if ok {
                    CliResponse::data(payload)
                } else {
                    CliResponse::error(format!("AUDIT CHAIN BROKEN: {payload}"))
                }
            }
            Err(e) => CliResponse::error(e.to_string()),
        },
        "tail" => {
            let q = Query {
                tenant: f.get("tenant").map(str::to_string),
                agent: f.get("agent").map(str::to_string),
                event: f.get("event").map(str::to_string),
                limit: f.get("limit").and_then(|l| l.parse().ok()).unwrap_or(20),
                ..Default::default()
            };
            match app.audit.query(&q) {
                Ok(recs) => CliResponse::data(json!({ "records": recs })),
                Err(e) => CliResponse::error(e.to_string()),
            }
        }
        other => CliResponse::error(format!("Comando desconhecido: {other} (use verify | tail)")),
    }
}

pub fn format_duration(secs: u64) -> String {
    let (h, m, s) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    if h > 0 {
        format!("{h}h {m}m {s}s")
    } else if m > 0 {
        format!("{m}m {s}s")
    } else {
        format!("{s}s")
    }
}

pub fn format_memory(kb: u64) -> String {
    if kb > 1024 * 1024 {
        format!("{:.1} GB", kb as f64 / 1_048_576.0)
    } else if kb > 1024 {
        format!("{:.1} MB", kb as f64 / 1024.0)
    } else {
        format!("{kb} KB")
    }
}

/// Resident memory of this process, in KiB, on every OS (the original only handled Linux
/// and reported `0 KB` elsewhere).
fn process_memory_kb() -> u64 {
    use sysinfo::{Pid, System};
    let pid = Pid::from_u32(std::process::id());
    let mut sys = System::new();
    sys.refresh_process(pid);
    sys.process(pid).map(|p| p.memory() / 1024).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duration_and_memory_formatting() {
        assert_eq!(format_duration(5), "5s");
        assert_eq!(format_duration(65), "1m 5s");
        assert_eq!(format_duration(3_725), "1h 2m 5s");
        assert_eq!(format_memory(512), "512 KB");
        assert_eq!(format_memory(2048), "2.0 MB");
    }
}
