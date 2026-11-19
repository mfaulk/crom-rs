//! The admin API (ADR 0002 §2.3): a separate listener whose only credential
//! is the admin token. Session handles never authenticate here.

use std::sync::Arc;

use axum::extract::rejection::JsonRejection;
use axum::extract::{Path, Request, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use crom_guard::{GrantId, GrantSpec, GrantStore, GrantStoreError};
use serde_json::json;

use crate::net::Peer;
use crate::Shared;

pub(crate) fn router(shared: Arc<Shared>) -> Router {
    Router::new()
        .route("/admin/health", get(health))
        .route("/admin/grants", post(issue_grant))
        .route("/admin/grants/{id}/revoke", post(revoke_grant))
        .route("/admin/credentials/reload", post(reload_credentials))
        .route("/admin/audit/head", get(audit_head))
        // Extension point for authenticated approvals (#354).
        .route("/admin/approvals", post(not_implemented))
        .fallback(not_found)
        .layer(middleware::from_fn_with_state(shared.clone(), authenticate))
        .with_state(shared)
}

fn error(status: StatusCode, code: &str, message: Option<String>) -> Response {
    let mut body = json!({ "error": code });
    if let Some(m) = message {
        body["message"] = m.into();
    }
    (status, Json(body)).into_response()
}

/// `Host` names accepted on the admin TCP listener (DNS-rebinding defense).
const LOOPBACK_HOSTS: [&str; 4] = ["localhost", "127.0.0.1", "[::1]", "::1"];

/// The host part of a `Host` header, without any port.
fn host_name(value: &str) -> &str {
    match value.strip_prefix('[') {
        Some(rest) => rest.find(']').map_or(value, |i| &value[..i + 2]),
        None => value.rsplit_once(':').map_or(value, |(h, _)| h),
    }
}

/// Admin token only, compared in constant time. Browser-originated
/// requests (any `Origin` header) are refused outright: there is no CORS.
/// On TCP the `Host` must be a loopback name.
async fn authenticate(
    State(s): State<Arc<Shared>>,
    headers: HeaderMap,
    req: Request,
    next: Next,
) -> Response {
    if headers.contains_key(header::ORIGIN) {
        return error(StatusCode::FORBIDDEN, "forbidden", None);
    }
    if matches!(req.extensions().get::<Peer>(), Some(Peer::Tcp(_))) {
        let host = headers
            .get(header::HOST)
            .and_then(|v| v.to_str().ok())
            .map(host_name);
        if !host.is_some_and(|h| LOOPBACK_HOSTS.iter().any(|l| h.eq_ignore_ascii_case(l))) {
            return error(StatusCode::FORBIDDEN, "forbidden", None);
        }
    }
    let presented = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(crate::strip_bearer)
        .unwrap_or("");
    if !s.admin_token.verify(presented) {
        tracing::warn!(
            path = req.uri().path(),
            "admin request rejected: bad credential"
        );
        return error(StatusCode::UNAUTHORIZED, "unauthorized", None);
    }
    next.run(req).await
}

async fn health(State(s): State<Arc<Shared>>) -> Response {
    let failed = s.guard.is_failed_closed();
    let status = if failed {
        StatusCode::SERVICE_UNAVAILABLE
    } else {
        StatusCode::OK
    };
    let body = json!({
        "status": if failed { "failed_closed" } else { "ok" },
        "operations": s.guard.registry().len(),
    });
    (status, Json(body)).into_response()
}

/// The response is the only place a session handle leaves Guard; it goes to
/// the trusted application, which hands it to the agent out of band.
async fn issue_grant(
    State(s): State<Arc<Shared>>,
    spec: Result<Json<GrantSpec>, JsonRejection>,
) -> Response {
    let spec = match spec {
        Ok(Json(spec)) => spec,
        Err(e) => return error(e.status(), "invalid_request", Some(e.body_text())),
    };
    match s.store.issue(&s.app_id, spec, s.clock.now()).await {
        Ok(issued) => {
            tracing::info!(grant_id = issued.grant.grant_id().as_str(), "grant issued");
            let body = json!({
                "grant": issued.grant,
                "handle": issued.handle.expose_secret(),
            });
            (StatusCode::CREATED, Json(body)).into_response()
        }
        Err(e) => store_error(e),
    }
}

async fn revoke_grant(State(s): State<Arc<Shared>>, Path(id): Path<String>) -> Response {
    let Ok(id) = GrantId::new(id) else {
        return error(StatusCode::NOT_FOUND, "unknown_grant", None);
    };
    match s.store.revoke(&s.app_id, &id, s.clock.now()).await {
        Ok(()) => {
            tracing::info!(grant_id = id.as_str(), "grant revoked");
            (StatusCode::OK, Json(json!({ "revoked": id }))).into_response()
        }
        Err(e) => store_error(e),
    }
}

fn store_error(e: GrantStoreError) -> Response {
    match e {
        GrantStoreError::UnknownGrant => error(StatusCode::NOT_FOUND, "unknown_grant", None),
        GrantStoreError::InvalidSpec(m) => error(StatusCode::BAD_REQUEST, "invalid_spec", Some(m)),
        GrantStoreError::Unavailable => {
            error(StatusCode::SERVICE_UNAVAILABLE, "store_unavailable", None)
        }
    }
}

/// `FileCredentialBroker::reload` / `EnvCredentialBroker::reload`. A failed
/// reload keeps the previous table; its error never carries secrets.
async fn reload_credentials(State(s): State<Arc<Shared>>) -> Response {
    match s.broker.reload() {
        Ok(()) => {
            tracing::info!("downstream credentials reloaded");
            (StatusCode::OK, Json(json!({ "status": "reloaded" }))).into_response()
        }
        Err(e) => {
            tracing::error!(error = %e, "credential reload failed; previous credentials kept");
            error(
                StatusCode::UNPROCESSABLE_ENTITY,
                "reload_failed",
                Some(e.to_string()),
            )
        }
    }
}

async fn audit_head(State(s): State<Arc<Shared>>) -> Response {
    Json(json!({ "head": s.audit.head() })).into_response()
}

async fn not_implemented() -> Response {
    error(
        StatusCode::NOT_IMPLEMENTED,
        "not_implemented",
        Some("approvals arrive with #354".into()),
    )
}

async fn not_found() -> Response {
    error(StatusCode::NOT_FOUND, "not_found", None)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The port is stripped from `Host`, including bracketed IPv6.
    #[test]
    fn host_name_strips_port() {
        assert_eq!(host_name("localhost:8471"), "localhost");
        assert_eq!(host_name("127.0.0.1"), "127.0.0.1");
        assert_eq!(host_name("[::1]:9"), "[::1]");
        assert_eq!(host_name("evil.example:80"), "evil.example");
    }
}
