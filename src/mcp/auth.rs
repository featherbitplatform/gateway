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
pub struct McpPrincipal {
    /// The token's optional label (`admin.mcp.tokens[].name`), for logs.
    pub name: Option<String>,
    /// What this request may do.
    pub scope: McpScope,
}

/// Configured tokens, ready for constant-time lookup.
pub struct McpAuthState {
    tokens: Vec<(Vec<u8>, McpPrincipal)>,
    allowed_origins: Vec<String>,
}

impl McpAuthState {
    /// Builds the lookup table from validated config.
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
pub enum AuthFailure {
    /// An `Origin` header was present, not allow-listed, and not same-origin.
    OriginNotAllowed,
    /// No usable bearer token.
    Unauthorized,
}

/// Extracts `host[:port]` from an `Origin` value such as `https://a.b:9091`.
fn origin_authority(origin: &str) -> Option<&str> {
    let rest = origin.split_once("://")?.1;
    let authority = rest.split('/').next()?;
    (!authority.is_empty()).then_some(authority)
}

/// Resolves the principal for a request from its headers, taking the request
/// authority (`Host`, or the HTTP/2 `:authority`) from `authority`.
///
/// An `Origin` header is accepted when it is allow-listed **or** when its
/// authority equals the request's own authority (the embedded web UI calling
/// `/mcp` on whatever hostname it was served from). Cross-site pages fail
/// both tests. Under DNS rebinding both values name the attacker's domain and
/// the request reaches the endpoint — but without the bearer token, which a
/// foreign origin cannot read from this origin's storage, it is still `401`.
pub fn authenticate_for(
    auth: &McpAuthState,
    headers: &HeaderMap,
    authority: Option<&str>,
) -> Result<McpPrincipal, AuthFailure> {
    if let Some(origin) = headers.get("origin") {
        let origin = origin.to_str().map_err(|_| AuthFailure::OriginNotAllowed)?;
        let listed = auth.allowed_origins.iter().any(|a| a == origin);
        let same_origin = match (origin_authority(origin), authority) {
            (Some(o), Some(h)) => o.eq_ignore_ascii_case(h),
            _ => false,
        };
        if !listed && !same_origin {
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

/// [`authenticate_for`] with the authority taken from the `Host` header.
///
/// The middleware calls [`authenticate_for`] directly so it can fall back to
/// the URI authority for HTTP/2; this wrapper is the plain entry point kept
/// for direct callers (and exercised throughout the tests below).
#[allow(dead_code)]
pub fn authenticate(auth: &McpAuthState, headers: &HeaderMap) -> Result<McpPrincipal, AuthFailure> {
    let host = headers.get("host").and_then(|v| v.to_str().ok());
    authenticate_for(auth, headers, host)
}

/// axum middleware for the MCP path: authenticates, then stores the
/// [`McpPrincipal`] in request extensions for the server handler to read.
pub async fn bearer_middleware(
    State(auth): State<Arc<McpAuthState>>,
    mut req: Request<Body>,
    next: Next,
) -> Response {
    let authority = req
        .headers()
        .get("host")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned)
        .or_else(|| req.uri().authority().map(|a| a.as_str().to_owned()));
    match authenticate_for(&auth, req.headers(), authority.as_deref()) {
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

    #[test]
    fn same_origin_is_accepted_without_an_allow_list() {
        let mut none = cfg();
        none.allowed_origins.clear();
        let auth = McpAuthState::from_config(&none);
        let ok = headers(&[
            ("host", "127.0.0.1:19091"),
            ("origin", "http://127.0.0.1:19091"),
            ("authorization", &format!("Bearer {READ}")),
        ]);
        assert!(
            authenticate(&auth, &ok).is_ok(),
            "Origin authority == Host authority"
        );

        // Case-insensitive host comparison; scheme is ignored.
        let https = headers(&[
            ("host", "Gateway.Example:9091"),
            ("origin", "https://gateway.example:9091"),
            ("authorization", &format!("Bearer {READ}")),
        ]);
        assert!(authenticate(&auth, &https).is_ok());

        // A different authority is still refused.
        let cross = headers(&[
            ("host", "127.0.0.1:19091"),
            ("origin", "http://evil.example"),
            ("authorization", &format!("Bearer {READ}")),
        ]);
        assert_eq!(
            authenticate(&auth, &cross),
            Err(AuthFailure::OriginNotAllowed)
        );

        // Same host but a different port is a different origin.
        let port = headers(&[
            ("host", "127.0.0.1:19091"),
            ("origin", "http://127.0.0.1:5173"),
            ("authorization", &format!("Bearer {READ}")),
        ]);
        assert_eq!(
            authenticate(&auth, &port),
            Err(AuthFailure::OriginNotAllowed)
        );

        // No Host header and no allow-list: an Origin is still refused.
        let no_host = headers(&[
            ("origin", "http://127.0.0.1:19091"),
            ("authorization", &format!("Bearer {READ}")),
        ]);
        assert_eq!(
            authenticate(&auth, &no_host),
            Err(AuthFailure::OriginNotAllowed)
        );
    }

    #[test]
    fn authenticate_for_uses_the_uri_authority_when_host_is_absent() {
        let mut none = cfg();
        none.allowed_origins.clear();
        let auth = McpAuthState::from_config(&none);
        let h = headers(&[
            ("origin", "http://127.0.0.1:19091"),
            ("authorization", &format!("Bearer {READ}")),
        ]);
        assert!(authenticate_for(&auth, &h, Some("127.0.0.1:19091")).is_ok());
        assert_eq!(
            authenticate_for(&auth, &h, Some("other.example")),
            Err(AuthFailure::OriginNotAllowed)
        );
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
