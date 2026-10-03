//! HTTP service: health probes, metrics, system info and the audit API.
//!
//! * `/healthz`, `/readyz` — orchestrator probes (never authenticated, reveal nothing)
//! * `/metrics`, `/info` — the original endpoints, same JSON; `/metrics` also speaks
//!   Prometheus text when asked (`?format=prometheus` or a Prometheus `Accept` header)
//! * `/v1/audit`, `/v1/audit/verify`, `/v1/audit/export` — query, verify and export the
//!   tamper-evident audit trail
//! * `/v1/status`, `/v1/rules`, `/v1/tenants`, `/v1/cluster` — operator views
//!
//! Authentication: `Authorization: Bearer <token>` where the token is either the admin
//! token (sees everything) or a tenant API key (sees only its own tenant). Whether
//! `/metrics` and `/info` require it follows the global security mode; everything under
//! `/v1` always does.

mod audit_api;
mod auth;

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde_json::json;

use crate::app::App;
use auth::{ApiError, Caller};

type Shared = State<Arc<App>>;

pub fn router(app: Arc<App>) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz))
        .route("/metrics", get(metrics))
        .route("/info", get(info))
        .route("/v1/status", get(status))
        .route("/v1/rules", get(rules))
        .route("/v1/tenants", get(tenants))
        .route("/v1/cluster", get(cluster))
        .merge(audit_api::routes())
        .with_state(app)
}

pub async fn serve(app: Arc<App>) -> crate::Result<Vec<SocketAddr>> {
    #[cfg(feature = "tls")]
    let tls = app
        .settings
        .config
        .security
        .tls
        .as_ref()
        .map(|cfg| -> crate::Result<_> {
            let material = crate::tls::load(cfg, &app.settings.paths.home)?;
            Ok(axum_server::tls_rustls::RustlsConfig::from_config(crate::tls::rustls_server(&material)?))
        })
        .transpose()?;
    let mut bound = Vec::new();
    for addr in app.settings.http_addrs() {
        match tokio::net::TcpListener::bind(addr).await {
            Ok(listener) => {
                bound.push(listener.local_addr()?);
                let router = router(app.clone());
                let shutdown = app.shutdown.clone();
                #[cfg(feature = "tls")]
                if let Some(cfg) = tls.clone() {
                    let std_listener = listener.into_std()?;
                    let handle = axum_server::Handle::new();
                    let h2 = handle.clone();
                    tokio::spawn(async move {
                        shutdown.wait().await;
                        h2.graceful_shutdown(Some(std::time::Duration::from_secs(5)));
                    });
                    tokio::spawn(async move {
                        let server = match axum_server::from_tcp_rustls(std_listener, cfg) {
                            Ok(s) => s,
                            Err(e) => {
                                tracing::error!("HTTPS server on {addr} cannot start: {e}");
                                return;
                            }
                        };
                        if let Err(e) = server.handle(handle).serve(router.into_make_service()).await {
                            tracing::error!("HTTPS server on {addr} stopped: {e}");
                        }
                    });
                    continue;
                }
                tokio::spawn(async move {
                    let res = axum::serve(listener, router)
                        .with_graceful_shutdown(async move { shutdown.wait().await })
                        .await;
                    if let Err(e) = res {
                        tracing::error!("HTTP server on {addr} stopped: {e}");
                    }
                });
            }
            Err(e) => tracing::warn!("HTTP cannot listen on {addr}: {e}"),
        }
    }
    if bound.is_empty() {
        return Err(crate::Error::Other("HTTP server could not bind to any address".into()));
    }
    for a in &bound {
        tracing::info!("Cardinal HTTP listening on {a}");
    }
    Ok(bound)
}

async fn healthz() -> &'static str {
    "ok"
}

async fn readyz(State(app): Shared) -> Response {
    let rules_loaded = app.engine.registry().current().generation > 0;
    let store_ok = app.store.kv_len(crate::store::DEFAULT_TENANT).is_ok();
    let cluster = crate::cluster::readiness(&app);
    let shutting_down = app.shutdown.is_triggered();
    let ready = rules_loaded && store_ok && cluster.0 && !shutting_down;
    let body = json!({
        "ready": ready,
        "rules_loaded": rules_loaded,
        "store": store_ok,
        "cluster": cluster.1,
        "shutting_down": shutting_down,
    });
    (if ready { StatusCode::OK } else { StatusCode::SERVICE_UNAVAILABLE }, Json(body)).into_response()
}

fn wants_prometheus(headers: &HeaderMap, q: &HashMap<String, String>) -> bool {
    if let Some(f) = q.get("format") {
        return f.eq_ignore_ascii_case("prometheus") || f.eq_ignore_ascii_case("prom");
    }
    headers
        .get(header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|a| a.contains("openmetrics-text") || a.contains("version=0.0.4"))
}

async fn metrics(
    State(app): Shared,
    headers: HeaderMap,
    Query(q): Query<HashMap<String, String>>,
) -> Result<Response, ApiError> {
    auth::open_or_authenticated(&app, &headers)?;
    if wants_prometheus(&headers, &q) {
        let set = app.engine.registry().current();
        let sys = app.sys.snapshot();
        let audit = app.audit.stats();
        let mut gauges: Vec<(String, String, f64)> = vec![
            ("cardinal_rules_loaded".into(), "Compiled rules.".into(), set.total_rules() as f64),
            ("cardinal_rule_load_issues".into(), "Rules that failed to load.".into(), set.issues.len() as f64),
            ("cardinal_uptime_seconds".into(), "Process uptime.".into(), app.uptime_secs() as f64),
            ("cardinal_cpu_usage_percent".into(), "Host CPU usage.".into(), sys.cpu_usage as f64),
            ("cardinal_memory_used_kib".into(), "Host memory in use.".into(), sys.used_mem),
            ("cardinal_audit_records".into(), "Records in the audit trail.".into(), audit.records as f64),
            (
                "cardinal_audit_dropped_total".into(),
                "Audit events lost to queue overflow.".into(),
                audit.dropped as f64,
            ),
        ];
        gauges.extend(crate::cluster::gauges(&app));
        let extra: Vec<(&str, &str, f64)> = gauges.iter().map(|(a, b, c)| (a.as_str(), b.as_str(), *c)).collect();
        let body = app.metrics.render_prometheus(&extra);
        return Ok(([(header::CONTENT_TYPE, "text/plain; version=0.0.4; charset=utf-8")], body).into_response());
    }
    // original payload, byte for byte the same shape
    Ok((
        [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        serde_json::to_string(&app.legacy_metrics()).unwrap_or_default(),
    )
        .into_response())
}

async fn info(State(app): Shared, headers: HeaderMap) -> Result<Response, ApiError> {
    auth::open_or_authenticated(&app, &headers)?;
    Ok((
        [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        serde_json::to_string(&*app.sys.snapshot()).unwrap_or_default(),
    )
        .into_response())
}

async fn status(State(app): Shared, headers: HeaderMap) -> Result<Json<serde_json::Value>, ApiError> {
    let caller = auth::authenticate(&app, &headers)?;
    let set = app.engine.registry().current();
    let mut body = json!({
        "node": app.node,
        "runtime": app.settings.runtime.as_str(),
        "version": env!("CARGO_PKG_VERSION"),
        "edition": app.edition,
        "features": { "lua": cfg!(feature = "lua"), "wasm": cfg!(feature = "wasm"), "ai": crate::ai::available() },
        "uptime_secs": app.uptime_secs(),
        "rules": { "loaded": set.total_rules(), "agents": set.total_agent_dirs(), "generation": set.generation },
        "in_flight": app.metrics.in_flight(),
    });
    if matches!(caller, Caller::Admin) {
        body["tenants"] = json!(app.tenants.list().len());
        body["auth_required"] = json!(app.tenants.auth_required());
        body["audit"] = json!(app.audit.stats());
        body["cluster"] = crate::cluster::summary(&app);
    }
    Ok(Json(body))
}

async fn rules(State(app): Shared, headers: HeaderMap) -> Result<Json<serde_json::Value>, ApiError> {
    let caller = auth::authenticate(&app, &headers)?;
    let set = app.engine.registry().current();
    let mut rows = Vec::new();
    for (tenant, tr) in &set.tenants {
        if !caller.can_see(tenant) {
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
        for (agent, rs) in &tr.agents {
            push(agent, rs);
        }
    }
    let issues: Vec<_> = set.issues.iter().filter(|i| caller.can_see(&i.tenant)).collect();
    Ok(Json(json!({ "generation": set.generation, "rules": rows, "issues": issues })))
}

async fn tenants(State(app): Shared, headers: HeaderMap) -> Result<Json<serde_json::Value>, ApiError> {
    let caller = auth::authenticate(&app, &headers)?;
    let counters = app.metrics.tenant_counters();
    let rows: Vec<_> = app
        .tenants
        .list()
        .iter()
        .filter(|t| caller.can_see(&t.id))
        .map(|t| {
            let c = counters.get(&t.id).cloned().unwrap_or_default();
            json!({
                "id": t.id, "enabled": t.enabled, "lua": t.lua_enabled, "wasm": t.wasm_enabled, "ai": t.ai.enabled,
                "limits": { "rate_limit_per_sec": t.limits.rate_limit_per_sec, "rule_timeout_ms": t.limits.rule_timeout_ms },
                "pulses": c.pulses, "rejected": c.rejected, "rate_limited": c.rate_limited,
            })
        })
        .collect();
    Ok(Json(json!({ "tenants": rows })))
}

async fn cluster(State(app): Shared, headers: HeaderMap) -> Result<Json<serde_json::Value>, ApiError> {
    auth::authenticate(&app, &headers)?.require_admin()?;
    Ok(Json(crate::cluster::summary(&app)))
}
