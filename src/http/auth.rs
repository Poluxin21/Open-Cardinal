//! HTTP authentication helpers.

use std::sync::Arc;

use axum::Json;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde_json::json;

use crate::app::App;
use crate::tenant::Tenant;

pub enum Caller {
    Admin,
    Tenant(Arc<Tenant>),
}

impl Caller {
    pub fn can_see(&self, tenant: &str) -> bool {
        match self {
            Caller::Admin => true,
            Caller::Tenant(t) => t.id == tenant,
        }
    }

    pub fn require_admin(&self) -> Result<(), ApiError> {
        match self {
            Caller::Admin => Ok(()),
            _ => Err(ApiError::new(StatusCode::FORBIDDEN, "admin token required")),
        }
    }

    /// Tenant filter forced by the credential (`None` = may see everything).
    pub fn tenant_scope(&self) -> Option<&str> {
        match self {
            Caller::Tenant(t) => Some(&t.id),
            _ => None,
        }
    }
}

pub struct ApiError {
    status: StatusCode,
    message: String,
}

impl ApiError {
    pub fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self { status, message: message.into() }
    }

    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, message)
    }

    pub fn internal(e: impl std::fmt::Display) -> Self {
        tracing::error!("http handler failed: {e}");
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, "internal error")
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let mut resp = (self.status, Json(json!({ "error": self.message }))).into_response();
        if self.status == StatusCode::UNAUTHORIZED {
            resp.headers_mut().insert(header::WWW_AUTHENTICATE, "Bearer".parse().expect("static header"));
        }
        resp
    }
}

fn bearer(headers: &HeaderMap) -> Option<&str> {
    let v = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let (scheme, token) = v.split_once(' ')?;
    scheme.eq_ignore_ascii_case("bearer").then_some(token.trim()).filter(|t| !t.is_empty())
}

/// Credentials are mandatory.
pub fn authenticate(app: &Arc<App>, headers: &HeaderMap) -> Result<Caller, ApiError> {
    let Some(token) = bearer(headers) else {
        return Err(ApiError::new(StatusCode::UNAUTHORIZED, "missing bearer token"));
    };
    if app.admin.verify(token) {
        return Ok(Caller::Admin);
    }
    match app.tenants.authenticate(Some(token)) {
        Ok(p) => Ok(Caller::Tenant(p.tenant)),
        Err(_) => {
            app.metrics.observe_rejected(None, false);
            app.audit_rejected(None, "http: invalid token", None, None);
            Err(ApiError::new(StatusCode::UNAUTHORIZED, "invalid credentials"))
        }
    }
}

/// Endpoints that were open in the original release stay open in "open" security mode
/// (bare daemon on loopback) and require credentials everywhere else.
pub fn open_or_authenticated(app: &Arc<App>, headers: &HeaderMap) -> Result<(), ApiError> {
    if !app.tenants.auth_required() && bearer(headers).is_none() {
        return Ok(());
    }
    authenticate(app, headers).map(|_| ())
}
