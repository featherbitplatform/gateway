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
use subtle::ConstantTimeEq;

/// Header the embedded web UI sets on every Admin API call. A 401 for such a
/// request carries no `WWW-Authenticate` challenge, so the browser does not
/// pop its native Basic Auth dialog over the UI's own sign-in screen. Every
/// other client (curl, scripts) still gets the standard challenge.
pub const UI_CLIENT_HEADER: &str = "x-featherbit-client";

/// Accepted Basic Auth accounts, sourced from `AdminConfig` (which in turn
/// supports `${ENV_VAR}` interpolation). Every account has full access.
#[derive(Clone)]
pub struct AuthState {
    /// Each account pre-rendered as the `user:pass` bytes a client presents.
    expected: Vec<Vec<u8>>,
}

impl AuthState {
    /// Builds the state from `(username, password)` pairs; see
    /// [`from_config`](Self::from_config) for the usual entry point.
    pub fn new(users: Vec<(String, String)>) -> Self {
        Self {
            expected: users
                .into_iter()
                .map(|(u, p)| format!("{u}:{p}").into_bytes())
                .collect(),
        }
    }

    /// The accounts `admin.username`/`admin.password` plus `admin.users` declare.
    pub fn from_config(admin: &crate::config::AdminConfig) -> Self {
        Self::new(
            admin
                .users()
                .into_iter()
                .map(|u| (u.username, u.password))
                .collect(),
        )
    }

    /// Compares a presented `user:pass` against every configured account in
    /// constant time per account (no early exit on a hit, per-byte compare
    /// of the expected value), so response timing reveals neither how much
    /// of a guess was right nor which usernames exist.
    fn matches(&self, presented: &[u8]) -> bool {
        let mut hit = subtle::Choice::from(0u8);
        for expected in &self.expected {
            let same_len = presented.len() == expected.len();
            // Compare against the expected bytes either way so a length
            // mismatch costs the same as a content mismatch.
            let candidate = if same_len {
                presented
            } else {
                expected.as_slice()
            };
            hit |= subtle::Choice::from(same_len as u8) & candidate.ct_eq(expected);
        }
        bool::from(hit)
    }
}

/// Axum middleware enforcing HTTP Basic Auth on admin endpoints.
///
/// `/healthz` and `/readyz` bypass authentication so orchestrators can probe
/// them without credentials. Every other request must carry an
/// `Authorization: Basic <base64(user:pass)>` header matching [`AuthState`];
/// otherwise the middleware responds `401 Unauthorized` and the inner
/// handler is never invoked. The 401 carries a
/// `WWW-Authenticate: Basic realm="featherbit admin"` challenge unless the
/// request came from the web UI (see [`UI_CLIENT_HEADER`]).
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
            auth.matches(&decoded)
        }
        _ => false,
    };

    if authorized {
        next.run(req).await
    } else if req.headers().contains_key(UI_CLIENT_HEADER) {
        (StatusCode::UNAUTHORIZED, "Unauthorized").into_response()
    } else {
        (
            StatusCode::UNAUTHORIZED,
            [("www-authenticate", "Basic realm=\"featherbit admin\"")],
            "Unauthorized",
        )
            .into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::routing::get;
    use axum::Router;
    use tower::ServiceExt;

    fn app() -> Router {
        Router::new()
            .route("/api/status", get(|| async { "ok" }))
            .layer(axum::middleware::from_fn_with_state(
                Arc::new(state_for(&[("u", "p"), ("ops", "s")])),
                basic_auth_middleware,
            ))
    }

    async fn send(req: Request<Body>) -> Response {
        app().oneshot(req).await.unwrap()
    }

    fn basic(creds: &str) -> String {
        format!("Basic {}", STANDARD.encode(creds))
    }

    fn state_for(users: &[(&str, &str)]) -> AuthState {
        AuthState::new(
            users
                .iter()
                .map(|(u, p)| (u.to_string(), p.to_string()))
                .collect(),
        )
    }

    #[test]
    fn test_matches_exact_credentials_only() {
        let auth = state_for(&[("u", "p")]);
        assert!(auth.matches(b"u:p"));
        for bad in [&b"u:q"[..], b"u:pp", b"u:", b"", b"U:p"] {
            assert!(!auth.matches(bad), "{bad:?}");
        }
    }

    #[test]
    fn test_any_configured_user_matches_but_pairs_do_not_cross() {
        let auth = state_for(&[("u", "p"), ("ops", "s")]);
        assert!(auth.matches(b"u:p"));
        assert!(auth.matches(b"ops:s"));
        for bad in [&b"u:s"[..], b"ops:p", b"ops:", b"u:p:ops:s"] {
            assert!(!auth.matches(bad), "{bad:?}");
        }
    }

    #[tokio::test]
    async fn test_second_user_passes_middleware() {
        let resp = send(
            Request::get("/api/status")
                .header("authorization", basic("ops:s"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_valid_credentials_pass() {
        let resp = send(
            Request::get("/api/status")
                .header("authorization", basic("u:p"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_non_ui_401_carries_the_basic_challenge() {
        let resp = send(
            Request::get("/api/status")
                .header("authorization", basic("u:wrong"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        assert!(resp.headers().contains_key("www-authenticate"));
    }

    /// The UI draws its own sign-in screen; a challenge would make the
    /// browser pop its native dialog on top of it.
    #[tokio::test]
    async fn test_ui_401_has_no_challenge() {
        let resp = send(
            Request::get("/api/status")
                .header(UI_CLIENT_HEADER, "ui")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        assert!(!resp.headers().contains_key("www-authenticate"));
    }
}
