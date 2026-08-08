//! Keycloak UMA authorization plugin (`authz-keycloak`).
//!
//! Ports a faithful subset of Apache APISIX's `authz-keycloak` plugin: the
//! **UMA 2.0 permission check** against a Keycloak token endpoint. For each
//! request the plugin takes the caller's bearer access token and asks Keycloak
//! whether it grants the configured permissions, using the
//! `urn:ietf:params:oauth:grant-type:uma-ticket` grant with
//! `response_mode=decision` — exactly the request APISIX's `evaluate_permissions`
//! builds. A `200` decision allows the request; a `401`/`403` is Keycloak
//! refusing it; any other status means no decision was obtained at all (see
//! [`classify_decision`]).
//!
//! ## Implemented subset
//!
//! - Static, pre-configured `permissions` (with optional `http_method_as_scope`).
//! - `policy_enforcement_mode` (`ENFORCING` / `PERMISSIVE`) for the empty-permission case.
//! - `ssl_verify` and `timeout`.
//!
//! ## Deliberately NOT ported (documented deviations)
//!
//! - **Discovery** (`discovery` URL): configure `token_endpoint` directly.
//! - **`lazy_load_paths`** / resource-registration lookups and the
//!   service-account (`client_credentials`) token dance — no dynamic resource
//!   resolution.
//! - **`password_grant_token_generation_incoming_uri`** token minting.
//! - Response/token caching and `access_denied_redirect_uri` redirects.
//!
//! Deliberate denials (missing bearer, no configured permission under
//! `ENFORCING`, or a `401`/`403` UMA decision) map to `403` and exit through
//! the dedicated **`denied`** port. Genuine failures exit through the ordinary
//! **`error`** port instead, since the node could not do its job: the Keycloak
//! token endpoint unreachable or timing out, *and* any unexpected status from
//! it (`400`, `404` from a misconfigured endpoint path, `5xx`). Only a status
//! Keycloak uses to express an access verdict is treated as a verdict —
//! otherwise a broken deployment would masquerade as a legitimate 403.

use async_trait::async_trait;
use bytes::Bytes;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use crate::context::{Context, GatewayError};
use crate::outbound::{OutboundClient, OutboundError, OutboundRequest};
use crate::plugins::resources::PluginResources;
use crate::plugins::{Plugin, PluginExecutionError, PluginOutput, PluginResult};

const UMA_GRANT_TYPE: &str = "urn:ietf:params:oauth:grant-type:uma-ticket";

/// Performs a Keycloak UMA permission check per request.
pub struct AuthzKeycloakPlugin {
    /// Keycloak token endpoint (`.../protocol/openid-connect/token`).
    token_endpoint: String,
    /// OAuth client id, sent as the UMA `audience`.
    client_id: String,
    /// Statically configured permissions (`resource` or `resource#scope`).
    permissions: Vec<String>,
    /// `ENFORCING` (default) denies when no permission is configured;
    /// `PERMISSIVE` allows.
    enforcing: bool,
    /// When true, the request method is appended as the permission scope.
    http_method_as_scope: bool,
    /// TLS certificate verification for the callout.
    ssl_verify: bool,
    /// Whole-call timeout for the callout.
    timeout: Duration,
    /// Shared pooled outbound HTTP client.
    outbound: Arc<OutboundClient>,
}

impl AuthzKeycloakPlugin {
    /// Builds the plugin from node config.
    ///
    /// Accepted keys:
    /// - `token_endpoint` (string, **required**): Keycloak token endpoint URL.
    ///   (APISIX's `discovery` auto-resolution is not supported.)
    /// - `client_id` (string, **required**): OAuth client id, sent as the UMA
    ///   `audience`.
    /// - `permissions` (array of strings, default `[]`): requested permissions,
    ///   each `resource` or `resource#scope`.
    /// - `policy_enforcement_mode` (string, default `"ENFORCING"`): `ENFORCING`
    ///   denies when `permissions` is empty; `PERMISSIVE` allows without a callout.
    /// - `http_method_as_scope` (bool, default `false`): append the request
    ///   method as the scope of each permission.
    /// - `ssl_verify` (bool, default `true`): verify the endpoint's TLS certificate.
    /// - `timeout` (integer ms, default `3000`): callout timeout.
    ///
    /// ```yaml
    /// - id: authz
    ///   type: authz-keycloak
    ///   config:
    ///     token_endpoint: https://kc.example.com/realms/myrealm/protocol/openid-connect/token
    ///     client_id: my-api
    ///     permissions: ["Default Resource#read"]
    ///     policy_enforcement_mode: ENFORCING
    ///     ssl_verify: true
    ///     timeout: 3000
    /// ```
    pub fn from_config(
        config: &HashMap<String, serde_json::Value>,
        resources: &Arc<PluginResources>,
    ) -> Result<Self, String> {
        let token_endpoint = config
            .get("token_endpoint")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                "authz-keycloak requires 'token_endpoint' (discovery is not supported)".to_string()
            })?
            .to_string();

        let client_id = config
            .get("client_id")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| "authz-keycloak requires 'client_id'".to_string())?
            .to_string();

        let permissions: Vec<String> = config
            .get("permissions")
            .and_then(|v| v.as_array())
            .map(|seq| {
                seq.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();

        let mode = config
            .get("policy_enforcement_mode")
            .and_then(|v| v.as_str())
            .unwrap_or("ENFORCING");
        let enforcing = match mode {
            "ENFORCING" => true,
            "PERMISSIVE" => false,
            other => {
                return Err(format!(
                    "authz-keycloak: invalid policy_enforcement_mode '{other}' \
                     (expected ENFORCING or PERMISSIVE)"
                ))
            }
        };

        let http_method_as_scope = config
            .get("http_method_as_scope")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        let ssl_verify = config
            .get("ssl_verify")
            .and_then(|v| v.as_bool())
            .unwrap_or(true);

        let timeout_ms = config
            .get("timeout")
            .and_then(|v| v.as_u64())
            .unwrap_or(3000);

        Ok(Self {
            token_endpoint,
            client_id,
            permissions,
            enforcing,
            http_method_as_scope,
            ssl_verify,
            timeout: Duration::from_millis(timeout_ms),
            outbound: resources.outbound.clone(),
        })
    }

    /// Builds the 403 denial and exits on the `denied` port. Reserved for
    /// deliberate denials — a missing bearer token, no permission configured
    /// under `ENFORCING`, or Keycloak actively refusing the UMA decision.
    fn deny(ctx: Context, message: impl Into<String>) -> PluginResult {
        // The reason never reaches the client (the `denied` port carries no
        // error record, unlike `Err`), but it's still useful for operators
        // debugging why a request was denied.
        tracing::debug!("authz-keycloak: denying request: {}", message.into());
        let mut ctx = ctx;
        ctx.response.status_code = 403;
        ctx.response.body =
            Bytes::from(r#"{"error":"access_denied","error_description":"not_authorized"}"#);
        ctx.response.headers.insert(
            "content-type".to_string(),
            vec!["application/json".to_string()],
        );
        Ok(PluginOutput::on_port(ctx, "denied"))
    }

    /// Builds a genuine infrastructure-failure `Err` (the Keycloak token
    /// endpoint unreachable, timed out, or otherwise untransportable) — exits
    /// through the `error` port because the node could not do its job, unlike
    /// `deny` which is a deliberate, client-facing decision.
    fn callout_error(ctx: Context, message: String) -> PluginResult {
        let mut ctx = ctx;
        ctx.response.status_code = 403;
        ctx.response.body =
            Bytes::from(r#"{"error":"access_denied","error_description":"not_authorized"}"#);
        ctx.response.headers.insert(
            "content-type".to_string(),
            vec!["application/json".to_string()],
        );
        Err(PluginExecutionError {
            context: ctx,
            error: GatewayError {
                node_id: String::new(),
                code: "AUTHZ_KEYCLOAK_ERROR".to_string(),
                message,
                metadata: HashMap::new(),
            },
        })
    }
}

/// Extracts the bearer token from the `Authorization` header, normalizing to a
/// `Bearer `-prefixed value (mirroring APISIX's `fetch_jwt_token`).
fn fetch_bearer(ctx: &Context) -> Option<String> {
    let raw = ctx
        .request
        .headers
        .get("authorization")
        .and_then(|v| v.first())?
        .trim();
    if raw.is_empty() {
        return None;
    }
    let lower = raw.to_ascii_lowercase();
    if lower.starts_with("bearer ") {
        Some(raw.to_string())
    } else {
        Some(format!("Bearer {raw}"))
    }
}

/// Applies `http_method_as_scope`: appends `#<method>` to each permission, or
/// `, <method>` when a scope is already present (matching APISIX's logic).
fn scoped_permissions(permissions: &[String], method: Option<&str>) -> Vec<String> {
    match method {
        None => permissions.to_vec(),
        Some(m) => permissions
            .iter()
            .map(|p| {
                if p.contains('#') {
                    format!("{p}, {m}")
                } else {
                    format!("{p}#{m}")
                }
            })
            .collect(),
    }
}

/// Encodes the UMA permission-check request body as
/// `application/x-www-form-urlencoded`, repeating `permission` per entry.
fn encode_uma_body(client_id: &str, permissions: &[String]) -> String {
    let mut pairs: Vec<(String, String)> = vec![
        ("grant_type".to_string(), UMA_GRANT_TYPE.to_string()),
        ("audience".to_string(), client_id.to_string()),
        ("response_mode".to_string(), "decision".to_string()),
    ];
    for p in permissions {
        pairs.push(("permission".to_string(), p.clone()));
    }
    pairs
        .iter()
        .map(|(k, v)| format!("{}={}", form_encode(k), form_encode(v)))
        .collect::<Vec<_>>()
        .join("&")
}

/// Percent-encodes a value for `application/x-www-form-urlencoded` bodies
/// (unreserved characters pass through; space becomes `+`).
fn form_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// What a Keycloak UMA `response_mode=decision` reply means.
#[derive(Debug, PartialEq, Eq)]
enum Decision {
    /// `200` — the decision endpoint granted the requested permissions.
    Granted,
    /// Keycloak evaluated the request and refused it: `403` (`access_denied`)
    /// or `401` (the bearer token was rejected). A deliberate, client-facing
    /// decision → the `denied` port.
    Denied,
    /// Anything else — `5xx`, or a `4xx` that means the *request to Keycloak*
    /// was wrong rather than the client's access (`400 invalid_grant`,
    /// `404` from a misconfigured token endpoint). The node never obtained a
    /// decision → the `error` port.
    Unexpected,
}

/// Classifies a Keycloak UMA response status.
///
/// The split matters: a misconfigured `token_endpoint` answering `404`, or a
/// Keycloak having a bad day answering `502`, is *not* "this client may not
/// pass" — reporting it as a denial hides a broken deployment behind a
/// plausible-looking 403.
fn classify_decision(status: u16) -> Decision {
    match status {
        200 => Decision::Granted,
        401 | 403 => Decision::Denied,
        _ => Decision::Unexpected,
    }
}

#[async_trait]
impl Plugin for AuthzKeycloakPlugin {
    fn plugin_type(&self) -> &str {
        "authz-keycloak"
    }

    async fn execute(
        &self,
        ctx: Context,
    ) -> PluginResult {
        // Empty permissions: deny under ENFORCING, allow under PERMISSIVE.
        if self.permissions.is_empty() {
            return if self.enforcing {
                Self::deny(ctx, "no permissions configured (ENFORCING)")
            } else {
                Ok(PluginOutput::success(ctx))
            };
        }

        let token = match fetch_bearer(&ctx) {
            Some(t) => t,
            None => return Self::deny(ctx, "missing bearer token"),
        };

        let method_scope = if self.http_method_as_scope {
            Some(ctx.request.method.as_str())
        } else {
            None
        };
        let permissions = scoped_permissions(&self.permissions, method_scope);
        let body = encode_uma_body(&self.client_id, &permissions);

        let request = OutboundRequest {
            method: http::Method::POST,
            url: self.token_endpoint.clone(),
            headers: vec![
                (
                    "content-type".to_string(),
                    "application/x-www-form-urlencoded".to_string(),
                ),
                ("authorization".to_string(), token),
            ],
            body: Bytes::from(body),
            timeout: self.timeout,
            ssl_verify: self.ssl_verify,
            tls: None,
        };

        match self.outbound.request(request).await {
            Ok(resp) => match classify_decision(resp.status) {
                Decision::Granted => Ok(PluginOutput::success(ctx)),
                Decision::Denied => Self::deny(
                    ctx,
                    format!("Keycloak denied permission (status {})", resp.status),
                ),
                // The node never got a decision — a broken endpoint or a
                // failing Keycloak, not a verdict about this client.
                Decision::Unexpected => Self::callout_error(
                    ctx,
                    format!(
                        "unexpected status {} from the Keycloak token endpoint \
                         (expected 200/401/403)",
                        resp.status
                    ),
                ),
            },
            Err(e) => {
                let detail = match &e {
                    OutboundError::Timeout(d) => format!("Keycloak request timed out after {d:?}"),
                    OutboundError::InvalidRequest(m) => format!("invalid Keycloak request: {m}"),
                    OutboundError::Transport(m) => format!("Keycloak request failed: {m}"),
                };
                Self::callout_error(ctx, detail)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::{GatewayRequest, GatewayResponse, Protocol};

    fn ctx_with_auth(auth: Option<&str>) -> Context {
        let mut headers = HashMap::new();
        if let Some(a) = auth {
            headers.insert("authorization".to_string(), vec![a.to_string()]);
        }
        Context {
            request: GatewayRequest {
                method: "GET".to_string(),
                path: "/data".to_string(),
                host: "h".to_string(),
                scheme: "http".to_string(),
                headers,
                query_params: HashMap::new(),
                body: Bytes::new(),
                remote_addr: "1.2.3.4:5".to_string(),
                protocol: Protocol::Http1,
            },
            response: GatewayResponse {
                status_code: 0,
                headers: HashMap::new(),
                body: Bytes::new(),
            },
            message: HashMap::new(),
            errors: Vec::new(),
        }
    }

    #[test]
    fn test_fetch_bearer_normalizes_prefix() {
        assert_eq!(
            fetch_bearer(&ctx_with_auth(Some("Bearer abc"))).as_deref(),
            Some("Bearer abc")
        );
        // missing prefix gets one
        assert_eq!(
            fetch_bearer(&ctx_with_auth(Some("abc"))).as_deref(),
            Some("Bearer abc")
        );
        // lowercase prefix preserved
        assert_eq!(
            fetch_bearer(&ctx_with_auth(Some("bearer abc"))).as_deref(),
            Some("bearer abc")
        );
        assert_eq!(fetch_bearer(&ctx_with_auth(None)), None);
    }

    #[test]
    fn test_scoped_permissions() {
        let perms = vec!["res".to_string(), "res2#read".to_string()];
        assert_eq!(scoped_permissions(&perms, None), perms);
        assert_eq!(
            scoped_permissions(&perms, Some("GET")),
            vec!["res#GET".to_string(), "res2#read, GET".to_string()]
        );
    }

    #[test]
    fn test_encode_uma_body() {
        let body = encode_uma_body("my-api", &["Default Resource#read".to_string()]);
        assert!(body.contains("grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Auma-ticket"));
        assert!(body.contains("audience=my-api"));
        assert!(body.contains("response_mode=decision"));
        // space -> +, '#' -> %23
        assert!(body.contains("permission=Default+Resource%23read"));
    }

    /// Only a status Keycloak uses to express an access verdict counts as a
    /// verdict. Everything else means the node never got one, so it must not
    /// be laundered into a 403 denial.
    #[test]
    fn test_classify_decision_splits_verdicts_from_failures() {
        assert_eq!(classify_decision(200), Decision::Granted);
        assert_eq!(classify_decision(401), Decision::Denied);
        assert_eq!(classify_decision(403), Decision::Denied);
        for status in [400u16, 404, 500, 502, 503] {
            assert_eq!(
                classify_decision(status),
                Decision::Unexpected,
                "status {status}"
            );
        }
    }

    #[tokio::test]
    async fn test_permissive_empty_permissions_allows() {
        let mut config = HashMap::new();
        config.insert(
            "token_endpoint".to_string(),
            serde_json::json!("https://kc/realms/r/protocol/openid-connect/token"),
        );
        config.insert("client_id".to_string(), serde_json::json!("my-api"));
        config.insert(
            "policy_enforcement_mode".to_string(),
            serde_json::json!("PERMISSIVE"),
        );
        let plugin = AuthzKeycloakPlugin::from_config(&config, &PluginResources::empty()).unwrap();
        assert!(plugin
            .execute(ctx_with_auth(Some("Bearer x")))
            .await
            .is_ok());
    }

    #[tokio::test]
    async fn test_enforcing_empty_permissions_denies() {
        let mut config = HashMap::new();
        config.insert(
            "token_endpoint".to_string(),
            serde_json::json!("https://kc/realms/r/protocol/openid-connect/token"),
        );
        config.insert("client_id".to_string(), serde_json::json!("my-api"));
        let plugin = AuthzKeycloakPlugin::from_config(&config, &PluginResources::empty()).unwrap();
        let out = plugin
            .execute(ctx_with_auth(Some("Bearer x")))
            .await
            .unwrap();
        assert_eq!(out.port, Some("denied"));
        assert_eq!(out.context.response.status_code, 403);
    }

    #[tokio::test]
    async fn test_missing_bearer_denies() {
        let mut config = HashMap::new();
        config.insert(
            "token_endpoint".to_string(),
            serde_json::json!("https://kc/realms/r/protocol/openid-connect/token"),
        );
        config.insert("client_id".to_string(), serde_json::json!("my-api"));
        config.insert(
            "permissions".to_string(),
            serde_json::json!(["Default Resource#read"]),
        );
        let plugin = AuthzKeycloakPlugin::from_config(&config, &PluginResources::empty()).unwrap();
        let out = plugin.execute(ctx_with_auth(None)).await.unwrap();
        assert_eq!(out.port, Some("denied"));
        assert_eq!(out.context.response.status_code, 403);
    }

    /// Regression: before the port split, a Keycloak callout failure (nothing
    /// listening on the token endpoint) was folded into the same denial as an
    /// actual permission refusal. It is a genuine infra failure and must stay
    /// on `Err`.
    #[tokio::test]
    async fn test_token_endpoint_unreachable_stays_on_error_port() {
        let mut config = HashMap::new();
        config.insert(
            "token_endpoint".to_string(),
            serde_json::json!("http://127.0.0.1:1/token"),
        );
        config.insert("client_id".to_string(), serde_json::json!("my-api"));
        config.insert(
            "permissions".to_string(),
            serde_json::json!(["Default Resource#read"]),
        );
        config.insert("timeout".to_string(), serde_json::json!(200));
        let plugin = AuthzKeycloakPlugin::from_config(&config, &PluginResources::empty()).unwrap();
        let err = plugin
            .execute(ctx_with_auth(Some("Bearer x")))
            .await
            .unwrap_err();
        assert_eq!(err.error.code, "AUTHZ_KEYCLOAK_ERROR");
        assert_eq!(err.context.response.status_code, 403);
    }

    /// Minimal one-shot HTTP server that answers any request with a fixed
    /// status line and no body. Returns its port.
    async fn spawn_status_server(status_line: &'static str) -> u16 {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            if let Ok((mut stream, _)) = listener.accept().await {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                let mut buf = [0u8; 4096];
                let _ = stream.read(&mut buf).await;
                let _ = stream
                    .write_all(
                        format!("HTTP/1.1 {status_line}\r\ncontent-length: 0\r\n\r\n").as_bytes(),
                    )
                    .await;
                let _ = stream.shutdown().await;
            }
        });
        port
    }

    fn enforcing_cfg(port: u16) -> HashMap<String, serde_json::Value> {
        let mut config = HashMap::new();
        config.insert(
            "token_endpoint".to_string(),
            serde_json::json!(format!("http://127.0.0.1:{port}/token")),
        );
        config.insert("client_id".to_string(), serde_json::json!("my-api"));
        config.insert(
            "permissions".to_string(),
            serde_json::json!(["Default Resource#read"]),
        );
        config.insert("timeout".to_string(), serde_json::json!(2000));
        config
    }

    /// Keycloak evaluated the request and refused it (`403 access_denied`):
    /// a deliberate decision → the `denied` port.
    #[tokio::test]
    async fn test_keycloak_403_decision_is_denied() {
        let port = spawn_status_server("403 Forbidden").await;
        let plugin =
            AuthzKeycloakPlugin::from_config(&enforcing_cfg(port), &PluginResources::empty())
                .unwrap();
        let out = plugin
            .execute(ctx_with_auth(Some("Bearer x")))
            .await
            .unwrap();
        assert_eq!(out.port, Some("denied"));
        assert_eq!(out.context.response.status_code, 403);
    }

    /// Regression: a `500` (or a `404` from a misconfigured token endpoint)
    /// used to be laundered into the same 403 denial as a real refusal, hiding
    /// a broken deployment. It must exit on `error`.
    #[tokio::test]
    async fn test_keycloak_5xx_is_error_port_not_denied() {
        let port = spawn_status_server("500 Internal Server Error").await;
        let plugin =
            AuthzKeycloakPlugin::from_config(&enforcing_cfg(port), &PluginResources::empty())
                .unwrap();
        let err = plugin
            .execute(ctx_with_auth(Some("Bearer x")))
            .await
            .unwrap_err();
        assert_eq!(err.error.code, "AUTHZ_KEYCLOAK_ERROR");
        assert!(err.error.message.contains("unexpected status 500"), "{}", err.error.message);
        assert_eq!(err.context.response.status_code, 403);
    }

    /// A `404` from a wrong `token_endpoint` path is the same class of problem.
    #[tokio::test]
    async fn test_keycloak_404_is_error_port_not_denied() {
        let port = spawn_status_server("404 Not Found").await;
        let plugin =
            AuthzKeycloakPlugin::from_config(&enforcing_cfg(port), &PluginResources::empty())
                .unwrap();
        let err = plugin
            .execute(ctx_with_auth(Some("Bearer x")))
            .await
            .unwrap_err();
        assert_eq!(err.error.code, "AUTHZ_KEYCLOAK_ERROR");
    }

    #[test]
    fn test_requires_token_endpoint_and_client_id() {
        assert!(
            AuthzKeycloakPlugin::from_config(&HashMap::new(), &PluginResources::empty()).is_err()
        );
        let mut config = HashMap::new();
        config.insert(
            "token_endpoint".to_string(),
            serde_json::json!("https://kc/token"),
        );
        assert!(AuthzKeycloakPlugin::from_config(&config, &PluginResources::empty()).is_err());
    }
}
