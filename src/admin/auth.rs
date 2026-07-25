//! HTTP Basic Auth middleware for the admin API.
//!
//! Guards every admin endpoint except the health probes, comparing the
//! `Authorization: Basic` header against credentials from `AdminConfig`.

use std::sync::Arc;

use axum::body::Body;
use axum::extract::State;
use axum::http::{Request, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use base64::engine::general_purpose::STANDARD;
use base64::Engine;

/// Expected Basic Auth credentials, sourced from `AdminConfig`
/// (which in turn supports `${ENV_VAR}` interpolation).
#[derive(Clone)]
pub struct AuthState {
    /// Username the client must present.
    pub username: String,
    /// Password the client must present.
    pub password: String,
}

/// Axum middleware enforcing HTTP Basic Auth on admin endpoints.
///
/// `/healthz` and `/readyz` bypass authentication so orchestrators can probe
/// them without credentials. Every other request must carry an
/// `Authorization: Basic <base64(user:pass)>` header matching [`AuthState`];
/// otherwise the middleware responds `401 Unauthorized` with a
/// `WWW-Authenticate: Basic realm="featherbit admin"` challenge and the
/// inner handler is never invoked.
pub async fn basic_auth_middleware(
    State(auth): State<Arc<AuthState>>,
    req: Request<Body>,
    next: Next,
) -> Response {
    // Skip auth for health/ready endpoints
    let path = req.uri().path();
    if path == "/healthz" || path == "/readyz" {
        return next.run(req).await;
    }

    let auth_header = req
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok());

    let authorized = match auth_header {
        Some(header) if header.starts_with("Basic ") => {
            let decoded = STANDARD.decode(&header[6..]).unwrap_or_default();
            let credentials = String::from_utf8(decoded).unwrap_or_default();
            let expected = format!("{}:{}", auth.username, auth.password);
            credentials == expected
        }
        _ => false,
    };

    if authorized {
        next.run(req).await
    } else {
        (
            StatusCode::UNAUTHORIZED,
            [("www-authenticate", "Basic realm=\"featherbit admin\"")],
            "Unauthorized",
        )
            .into_response()
    }
}
