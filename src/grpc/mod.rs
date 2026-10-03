//! gRPC `Sentinel` service: agents send a `Pulse`, Cardinal answers with a `Reaction`.

mod validate;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Instant;

use serde_json::json;
use tokio::net::TcpListener;
use tokio_stream::wrappers::TcpListenerStream;
use tonic::{Request, Response, Status};

use crate::app::App;
use crate::audit::Event;
use crate::engine::{OutcomeKind, PulseInput};
use crate::error::Error;
use crate::pb::core::sentinel_server::{Sentinel, SentinelServer};
use crate::pb::core::{Pulse, Reaction};
use crate::tenant::Principal;
use crate::util;

pub use validate::validate_pulse;

/// A decision plus what the audit trail should say about it.
struct Decided {
    reaction: Reaction,
    audit: serde_json::Value,
    /// Did something or went wrong (recorded even in `audit.decisions = "actions"` mode).
    noteworthy: bool,
}

#[derive(Clone)]
pub struct SentinelService {
    app: Arc<App>,
}

impl SentinelService {
    pub fn new(app: Arc<App>) -> Self {
        Self { app }
    }
}

/// `authorization: Bearer <api key>` → the key.
fn bearer<T>(req: &Request<T>) -> Option<&str> {
    let v = req.metadata().get("authorization")?.to_str().ok()?;
    let (scheme, token) = v.split_once(' ')?;
    scheme.eq_ignore_ascii_case("bearer").then_some(token.trim()).filter(|t| !t.is_empty())
}

fn status_from(e: &Error) -> Status {
    match e {
        Error::Unauthenticated(m) => Status::unauthenticated(m.clone()),
        Error::PermissionDenied(m) => Status::permission_denied(m.clone()),
        Error::RateLimited => Status::resource_exhausted("rate limit exceeded"),
        Error::Overloaded(m) => Status::resource_exhausted(m.clone()),
        Error::Invalid(m) => Status::invalid_argument(m.clone()),
        other => {
            // internals stay in the log; the agent only learns that the service failed
            tracing::error!("internal error serving pulse: {other}");
            Status::internal("internal error")
        }
    }
}

impl SentinelService {
    async fn decide(&self, principal: &Principal, pulse: Pulse) -> Result<Decided, Error> {
        let app = &self.app;
        let tenant = &principal.tenant;
        let trace_id = util::random_hex(8);

        // Heathcliff: an operator override beats every rule.
        if let Some(forced) = app.store.forced_get(&tenant.id, &pulse.agent_id)?
            && !forced.is_expired(util::now_ms())
        {
            app.metrics.forced.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let reaction = Reaction {
                trace_id: trace_id.clone(),
                r#type: forced.kind,
                command_name: if forced.command_name.is_empty() {
                    "heathcliff".into()
                } else {
                    forced.command_name.clone()
                },
                parameters: forced.params.clone().into_iter().collect(),
            };
            let audit = json!({
                "source": "forced",
                "action": forced.kind,
                "command": reaction.command_name,
                "forced_at_ms": forced.set_at_ms,
                "telemetry": app.audit.telemetry_value(&pulse.telemetry),
            });
            return Ok(Decided { reaction, audit, noteworthy: true });
        }

        let input = PulseInput {
            agent_id: pulse.agent_id.clone(),
            tenant: tenant.id.clone(),
            timestamp: pulse.timestamp,
            trace_id: trace_id.clone(),
            telemetry: pulse.telemetry.clone(),
        };
        let telemetry_for_audit = app.audit.telemetry_value(&pulse.telemetry);
        let decision = app.engine.evaluate(tenant, input).await?;

        let rules: Vec<_> = decision
            .outcomes
            .iter()
            .map(|o| {
                let (result, extra) = match &o.result {
                    OutcomeKind::NoOpinion => ("none", json!({})),
                    OutcomeKind::Skipped => ("skipped", json!({})),
                    OutcomeKind::Output { action, priority, evidence } => {
                        ("output", json!({ "action": action, "priority": priority, "evidence": evidence }))
                    }
                    OutcomeKind::Error { kind, message } => {
                        ("error", json!({ "error_kind": format!("{kind:?}"), "error": message }))
                    }
                };
                json!({ "rule": o.rule, "kind": o.kind.as_str(), "us": o.micros, "result": result, "detail": extra })
            })
            .collect();

        app.metrics.observe_pulse(
            &tenant.id,
            decision.reaction.r#type,
            decision.elapsed.as_micros() as u64,
            decision.rule_errors(),
        );

        let audit = json!({
            "source": if decision.winner.is_some() { "rules" } else { "idle" },
            "action": decision.reaction.r#type,
            "command": decision.reaction.command_name,
            "priority": decision.priority,
            "winner": decision.winner,
            "winner_kind": decision.winner_kind.map(|k| k.as_str()),
            "latency_us": decision.elapsed.as_micros() as u64,
            "rules": rules,
            "telemetry": telemetry_for_audit,
        });
        let noteworthy = decision.reaction.r#type != 0 || decision.rule_errors() > 0;
        Ok(Decided { reaction: decision.reaction, audit, noteworthy })
    }
}

#[tonic::async_trait]
impl Sentinel for SentinelService {
    async fn sync(&self, request: Request<Pulse>) -> Result<Response<Reaction>, Status> {
        let app = &self.app;
        let _in_flight = app.metrics.enter();
        let started = Instant::now();
        let peer = request.remote_addr().map(|a| a.to_string());

        let principal = match app.tenants.authenticate(bearer(&request)) {
            Ok(p) => p,
            Err(e) => {
                app.metrics.observe_rejected(None, false);
                app.audit_rejected(None, "authentication failed", None, peer);
                return Err(status_from(&e));
            }
        };
        let tenant_id = principal.tenant.id.clone();
        let pulse = request.into_inner();

        let checks =
            validate_pulse(&pulse, &principal.tenant.limits).and_then(|_| principal.check_agent(&pulse.agent_id));
        if let Err(e) = checks {
            app.metrics.observe_rejected(Some(&tenant_id), false);
            app.audit_rejected(Some(&tenant_id), &e.to_string(), Some(&pulse.agent_id), peer);
            return Err(status_from(&e));
        }
        if !principal.tenant.limiter.try_acquire() {
            app.metrics.observe_rejected(Some(&tenant_id), true);
            app.audit_rejected(Some(&tenant_id), "rate limit exceeded", Some(&pulse.agent_id), peer);
            return Err(status_from(&Error::RateLimited));
        }

        let agent = pulse.agent_id.clone();
        match self.decide(&principal, pulse).await {
            Ok(Decided { reaction, audit, noteworthy }) => {
                tracing::debug!(
                    tenant = %tenant_id,
                    agent = %util::log_safe(&agent),
                    action = reaction.r#type,
                    command = %util::log_safe(&reaction.command_name),
                    elapsed_us = started.elapsed().as_micros() as u64,
                    "pulse decided"
                );
                // routine IDLE decisions are only recorded with `audit.decisions = "all"`
                if noteworthy || app.audit.decisions_mode() == crate::config::AuditDecisions::All {
                    app.audit
                        .record(Event::new("decision", tenant_id, audit).agent(agent).trace(reaction.trace_id.clone()));
                }
                Ok(Response::new(reaction))
            }
            Err(e) => {
                app.metrics.observe_rejected(Some(&tenant_id), false);
                app.audit
                    .record(Event::new("decision_failed", tenant_id, json!({ "error": e.to_string() })).agent(agent));
                Err(status_from(&e))
            }
        }
    }
}

/// Bind every configured address and serve until shutdown. Individual bind failures (for
/// instance IPv6 loopback on a host without IPv6) are logged; only "nothing bound" is fatal.
pub async fn serve(app: Arc<App>) -> crate::Result<Vec<SocketAddr>> {
    let svc = SentinelService::new(app.clone());
    #[cfg(feature = "tls")]
    let tls = app
        .settings
        .config
        .security
        .tls
        .as_ref()
        .map(|cfg| -> crate::Result<_> {
            let material = crate::tls::load(cfg, &app.settings.paths.home)?;
            tracing::info!(mtls = material.client_ca_pem.is_some(), "TLS enabled for the agent API");
            Ok(crate::tls::tonic_server(&material))
        })
        .transpose()?;
    let mut bound = Vec::new();
    for addr in app.settings.grpc_addrs() {
        match TcpListener::bind(addr).await {
            Ok(listener) => {
                bound.push(listener.local_addr()?);
                let svc = SentinelServer::new(svc.clone()).max_decoding_message_size(1024 * 1024);
                let shutdown = app.shutdown.clone();
                let builder = tonic::transport::Server::builder()
                    .tcp_nodelay(true)
                    .http2_keepalive_interval(Some(std::time::Duration::from_secs(30)));
                #[cfg(feature = "tls")]
                let builder = match tls.clone() {
                    Some(cfg) => builder.tls_config(cfg).map_err(|e| Error::config(format!("tls: {e}")))?,
                    None => builder,
                };
                let mut builder = builder;
                tokio::spawn(async move {
                    let res = builder
                        .add_service(svc)
                        .serve_with_incoming_shutdown(TcpListenerStream::new(listener), shutdown.wait())
                        .await;
                    if let Err(e) = res {
                        tracing::error!("gRPC server on {addr} stopped: {e}");
                    }
                });
            }
            Err(e) => tracing::warn!("gRPC cannot listen on {addr}: {e}"),
        }
    }
    if bound.is_empty() {
        return Err(Error::Other("gRPC server could not bind to any address".into()));
    }
    for a in &bound {
        tracing::info!("Cardinal gRPC Server listening on {a}");
    }
    Ok(bound)
}
