//! `/v1/audit*`: query, verify and export the audit trail.

use std::collections::HashMap;
use std::sync::Arc;

use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde_json::json;

use super::auth::{self, ApiError, Caller};
use crate::app::App;
use crate::audit::Query as AuditQuery;

pub fn routes() -> Router<Arc<App>> {
    Router::new()
        .route("/v1/audit", get(query))
        .route("/v1/audit/verify", get(verify))
        .route("/v1/audit/export", get(export))
}

fn parse_u64(q: &HashMap<String, String>, key: &str) -> Result<Option<u64>, ApiError> {
    match q.get(key) {
        None => Ok(None),
        Some(v) => {
            v.parse().map(Some).map_err(|_| ApiError::bad_request(format!("'{key}' must be a non-negative integer")))
        }
    }
}

/// A tenant credential can only ever read its own tenant, whatever `?tenant=` says.
fn scoped_tenant(caller: &Caller, q: &HashMap<String, String>) -> Result<Option<String>, ApiError> {
    match (caller.tenant_scope(), q.get("tenant")) {
        (Some(own), Some(asked)) if own != asked => {
            Err(ApiError::new(StatusCode::FORBIDDEN, "this credential can only read its own tenant"))
        }
        (Some(own), _) => Ok(Some(own.to_string())),
        (None, asked) => Ok(asked.cloned()),
    }
}

fn build_query(caller: &Caller, q: &HashMap<String, String>, default_limit: usize) -> Result<AuditQuery, ApiError> {
    Ok(AuditQuery {
        tenant: scoped_tenant(caller, q)?,
        agent: q.get("agent").cloned(),
        event: q.get("event").cloned(),
        trace_id: q.get("trace_id").cloned(),
        since_ms: parse_u64(q, "since")?,
        until_ms: parse_u64(q, "until")?,
        before: parse_u64(q, "before")?,
        limit: parse_u64(q, "limit")?.map(|l| l as usize).unwrap_or(default_limit),
    })
}

async fn query(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Query(q): Query<HashMap<String, String>>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let caller = auth::authenticate(&app, &headers)?;
    let aq = build_query(&caller, &q, 100)?;
    app.audit.flush().await;
    let limit = aq.limit.clamp(1, 1000);
    let app2 = app.clone();
    let records = tokio::task::spawn_blocking(move || app2.audit.query(&aq))
        .await
        .map_err(ApiError::internal)?
        .map_err(ApiError::internal)?;
    let next = (records.len() >= limit).then(|| records.last().map(|r| r.body.seq)).flatten();
    Ok(Json(json!({ "records": records, "next_before": next })))
}

async fn verify(State(app): State<Arc<App>>, headers: HeaderMap) -> Result<Response, ApiError> {
    auth::authenticate(&app, &headers)?.require_admin()?;
    app.audit.flush().await;
    let app2 = app.clone();
    let report = tokio::task::spawn_blocking(move || app2.audit.verify())
        .await
        .map_err(ApiError::internal)?
        .map_err(ApiError::internal)?;
    let status = if report.ok { StatusCode::OK } else { StatusCode::CONFLICT };
    Ok((status, Json(report)).into_response())
}

/// NDJSON, oldest first, so a SIEM can resume with `?from=<next seq>`.
async fn export(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Query(q): Query<HashMap<String, String>>,
) -> Result<Response, ApiError> {
    let caller = auth::authenticate(&app, &headers)?;
    let from = parse_u64(&q, "from")?.unwrap_or(1);
    let limit = parse_u64(&q, "limit")?.map(|l| l as usize).unwrap_or(1000).clamp(1, 10_000);
    let tenant = scoped_tenant(&caller, &q)?;
    app.audit.flush().await;

    let app2 = app.clone();
    let records = tokio::task::spawn_blocking(move || app2.audit.export_from(from, limit))
        .await
        .map_err(ApiError::internal)?
        .map_err(ApiError::internal)?;

    let next = records.last().map(|r| r.body.seq + 1).unwrap_or(from);
    let mut body = String::new();
    for r in records.iter().filter(|r| tenant.as_ref().is_none_or(|t| &r.body.tenant == t)) {
        body.push_str(&serde_json::to_string(r).map_err(ApiError::internal)?);
        body.push('\n');
    }
    Ok((
        [
            (header::CONTENT_TYPE, "application/x-ndjson".to_string()),
            (header::HeaderName::from_static("x-next-seq"), next.to_string()),
        ],
        body,
    )
        .into_response())
}
