//! Bearer-token authentication for the MCP endpoint.
//!
//! Separate from the Admin API's Basic Auth on purpose: an agent gets a
//! narrower credential (`read` or `write`) that is useless on `/api/*`, and
//! the Admin credentials are useless here. The token is resolved on **every**
//! request (never cached on the MCP session) so a client cannot keep a scope
//! it no longer presents.

use std::sync::Arc;

use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, Request, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::Json;
use subtle::ConstantTimeEq;

use crate::config::{McpConfig, McpScope};

/// The identity behind an authenticated MCP request.
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(dead_code)] // mounted by src/mcp/server.rs (Task 9)
pub struct McpPrincipal {
    /// The token's optional label (`admin.mcp.tokens[].name`), for logs.
    pub name: Option<String>,
    /// What this request may do.
    pub scope: McpScope,
}

/// Configured tokens, ready for constant-time lookup.
#[allow(dead_code)] // mounted by src/mcp/server.rs (Task 9)
pub struct McpAuthState {
    tokens: Vec<(Vec<u8>, McpPrincipal)>,
    allowed_origins: Vec<String>,
}

impl McpAuthState {
    /// Builds the lookup table from validated config.
    #[allow(dead_code)] // mounted by src/mcp/server.rs (Task 9)
    pub fn from_config(cfg: &McpConfig) -> Self {
        Self {
            tokens: cfg
                .tokens
                .iter()
                .map(|t| {
                    (
                        t.token.as_bytes().to_vec(),
                        McpPrincipal {
                            name: t.name.clone(),
                            scope: t.scope,
                        },
                    )
                })
                .collect(),
            allowed_origins: cfg.allowed_origins.clone(),
        }
    }
}

/// Why a request was refused. Deliberately coarse: callers must not leak
/// whether a token was unknown, malformed, or absent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)] // mounted by src/mcp/server.rs (Task 9)
pub enum AuthFailure {
    /// An `Origin` header was present and not allow-listed.
    OriginNotAllowed,
    /// No usable bearer token.
    Unauthorized,
}

/// Resolves the principal for a request from its headers.
#[allow(dead_code)] // mounted by src/mcp/server.rs (Task 9)
pub fn authenticate(auth: &McpAuthState, headers: &HeaderMap) -> Result<McpPrincipal, AuthFailure> {
    if let Some(origin) = headers.get("origin") {
        let allowed = origin
            .to_str()
            .map(|o| auth.allowed_origins.iter().any(|a| a == o))
            .unwrap_or(false);
        if !allowed {
            return Err(AuthFailure::OriginNotAllowed);
        }
    }

    let presented = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|h| {
            // RFC 6750: the scheme is case-insensitive.
            let (scheme, rest) = h.split_at_checked(7)?;
            scheme.eq_ignore_ascii_case("Bearer ").then(|| rest.trim())
        })
        .filter(|t| !t.is_empty())
        .ok_or(AuthFailure::Unauthorized)?;

    // Compare against every configured token without early exit so timing
    // does not reveal which entry (if any) matched.
    let mut matched: Option<McpPrincipal> = None;
    for (token, principal) in &auth.tokens {
        let same_len = token.len() == presented.len();
        let eq = same_len && bool::from(token.as_slice().ct_eq(presented.as_bytes()));
        if eq && matched.is_none() {
            matched = Some(principal.clone());
        }
    }
    matched.ok_or(AuthFailure::Unauthorized)
}

/// axum middleware for the MCP path: authenticates, then stores the
/// [`McpPrincipal`] in request extensions for the server handler to read.
#[allow(dead_code)] // mounted by src/mcp/server.rs (Task 9)
pub async fn bearer_middleware(
    State(auth): State<Arc<McpAuthState>>,
    mut req: Request<Body>,
    next: Next,
) -> Response {
    match authenticate(&auth, req.headers()) {
        Ok(principal) => {
            req.extensions_mut().insert(principal);
            next.run(req).await
        }
        Err(AuthFailure::OriginNotAllowed) => (
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({"error": "origin_not_allowed"})),
        )
            .into_response(),
        Err(AuthFailure::Unauthorized) => (
            StatusCode::UNAUTHORIZED,
            [("www-authenticate", "Bearer realm=\"featherbit-mcp\"")],
            Json(serde_json::json!({"error": "unauthorized"})),
        )
            .into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::McpConfig;
    use axum::body::Body;
    use axum::http::{HeaderMap, HeaderValue, Request, StatusCode};
    use axum::routing::get;
    use axum::Router;
    use tower::ServiceExt;

    const READ: &str = "read-token-0123456789";
    const WRITE: &str = "write-token-0123456789";

    fn cfg() -> McpConfig {
        serde_yaml::from_str(&format!(
            "enabled: true\ntokens:\n  - token: {READ}\n    scope: read\n    name: local\n  - token: {WRITE}\n    scope: write\nallowed_origins: [\"http://localhost:5173\"]\n"
        ))
        .unwrap()
    }

    fn headers(pairs: &[(&'static str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(*k, HeaderValue::from_str(v).unwrap());
        }
        h
    }

    #[test]
    fn read_token_yields_read_principal_with_name() {
        let auth = McpAuthState::from_config(&cfg());
        let p = authenticate(
            &auth,
            &headers(&[("authorization", &format!("Bearer {READ}"))]),
        )
        .unwrap();
        assert_eq!(p.scope, McpScope::Read);
        assert_eq!(p.name.as_deref(), Some("local"));
    }

    #[test]
    fn write_token_yields_write_principal_without_name() {
        let auth = McpAuthState::from_config(&cfg());
        let p = authenticate(
            &auth,
            &headers(&[("authorization", &format!("bearer {WRITE}"))]),
        )
        .unwrap();
        assert_eq!(p.scope, McpScope::Write);
        assert_eq!(p.name, None);
    }

    #[test]
    fn missing_malformed_and_unknown_tokens_are_unauthorized() {
        let auth = McpAuthState::from_config(&cfg());
        assert_eq!(
            authenticate(&auth, &headers(&[])),
            Err(AuthFailure::Unauthorized)
        );
        assert_eq!(
            authenticate(&auth, &headers(&[("authorization", "Basic dTpw")])),
            Err(AuthFailure::Unauthorized)
        );
        assert_eq!(
            authenticate(
                &auth,
                &headers(&[("authorization", "Bearer nope-nope-nope-nope")])
            ),
            Err(AuthFailure::Unauthorized)
        );
        // A prefix of a real token must not match.
        assert_eq!(
            authenticate(
                &auth,
                &headers(&[("authorization", "Bearer read-token-012345678")])
            ),
            Err(AuthFailure::Unauthorized)
        );
    }

    #[test]
    fn origin_is_checked_before_the_token() {
        let auth = McpAuthState::from_config(&cfg());
        let bad = headers(&[
            ("origin", "http://evil.example"),
            ("authorization", &format!("Bearer {READ}")),
        ]);
        assert_eq!(
            authenticate(&auth, &bad),
            Err(AuthFailure::OriginNotAllowed)
        );
        let ok = headers(&[
            ("origin", "http://localhost:5173"),
            ("authorization", &format!("Bearer {READ}")),
        ]);
        assert!(authenticate(&auth, &ok).is_ok());
        // With no allowed origins, ANY Origin header is refused.
        let mut none = cfg();
        none.allowed_origins.clear();
        let auth = McpAuthState::from_config(&none);
        assert_eq!(authenticate(&auth, &ok), Err(AuthFailure::OriginNotAllowed));
    }

    async fn echo_scope(req: Request<Body>) -> String {
        req.extensions()
            .get::<McpPrincipal>()
            .map(|p| p.scope.as_str().to_string())
            .unwrap_or_else(|| "none".into())
    }

    fn app() -> Router {
        Router::new().route("/mcp", get(echo_scope)).route_layer(
            axum::middleware::from_fn_with_state(
                Arc::new(McpAuthState::from_config(&cfg())),
                bearer_middleware,
            ),
        )
    }

    #[tokio::test]
    async fn middleware_inserts_principal_and_rejects_properly() {
        let resp = app()
            .oneshot(
                Request::get("/mcp")
                    .header("authorization", format!("Bearer {WRITE}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(&body[..], b"write");

        let resp = app()
            .oneshot(Request::get("/mcp").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(
            resp.headers().get("www-authenticate").unwrap(),
            "Bearer realm=\"featherbit-mcp\""
        );

        let resp = app()
            .oneshot(
                Request::get("/mcp")
                    .header("origin", "http://evil.example")
                    .header("authorization", format!("Bearer {WRITE}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }
}
