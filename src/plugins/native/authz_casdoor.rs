//! Casdoor authorization plugin (`authz-casdoor`).
//!
//! Port of Apache APISIX's `authz-casdoor` plugin, with two modes:
//!
//! - **Stateless (default)** — when no session secret is configured, the node
//!   acts as a **bearer-token validator**: a Casdoor access token presented in
//!   the `Authorization` header is validated by calling Casdoor's OAuth **token
//!   introspection** endpoint (`/api/login/oauth/introspect`, RFC 7662)
//!   authenticated with the client credentials. `active: true` allows the
//!   request; anything else denies it with `AUTHZ_CASDOOR_DENIED` (`403`).
//! - **Interactive (opt-in)** — set `session_secret` (or `session.secret`) to
//!   turn on the full **OAuth Authorization Code** login flow using the shared
//!   [encrypted-cookie session primitive](crate::plugins::util::cookie_session).
//!   Unauthenticated browsers are redirected to Casdoor's authorize URL; the
//!   callback exchanges the `code` for an access token, which is sealed into an
//!   encrypted client-side cookie (no server-side session store). See the
//!   three-branch logic in [`AuthzCasdoorPlugin::execute_interactive`].
//!
//! ## Redirect wiring (interactive mode)
//!
//! A `302` produced by this node (login redirect, post-callback redirect, or
//! logout) is returned as an [`Err`] carrying the prepared response with code
//! `CASDOOR_REDIRECT`, following the same early-exit convention as the
//! `fault-injection`/`mocking` nodes. **Wire the node's `error` edge to
//! `client.in`** so the redirect reaches the browser; the `success` edge
//! carries authenticated requests on to the upstream.

use async_trait::async_trait;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use base64::Engine;
use bytes::Bytes;
use ring::rand::{SecureRandom, SystemRandom};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use crate::context::{Context, GatewayError};
use crate::outbound::{OutboundClient, OutboundError, OutboundRequest};
use crate::plugins::resources::PluginResources;
use crate::plugins::util::cookie_session::{
    build_set_cookie, delete_cookie, path_covers, read_cookie, CookieAttrs, CookieSealer, SameSite,
};
use crate::plugins::{Plugin, PluginExecutionError, PluginOutput, PluginResult};

/// Session payload sealed into the Casdoor session cookie (interactive mode).
#[derive(Debug, Clone, Serialize, Deserialize)]
struct CasdoorSession {
    /// The Casdoor access token minted at the callback.
    access_token: String,
    /// The client id this session was issued under (guards cross-config reuse).
    client_id: String,
    /// Decoded access-token claims, when the token is a JWT.
    #[serde(default)]
    claims: Option<serde_json::Value>,
}

/// Transient login-flow payload sealed into the short-lived flow cookie.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct CasdoorFlow {
    /// Anti-CSRF `state` echoed back on the callback.
    state: String,
    /// URI to return the browser to after login completes.
    original_uri: String,
}

/// Validates a Casdoor access token, and in interactive mode runs the SSO flow.
pub struct AuthzCasdoorPlugin {
    /// Casdoor server base URL, without a trailing slash.
    endpoint_addr: String,
    /// Casdoor application client id.
    client_id: String,
    /// Casdoor application client secret.
    client_secret: String,
    /// `Authorization: Basic ...` header value built from the client credentials.
    basic_auth: String,
    /// TLS certificate verification for the callout.
    ssl_verify: bool,
    /// Whole-call timeout for the callout.
    timeout: Duration,
    /// When set, interactive SSO login is enabled and this seals/opens cookies.
    sealer: Option<CookieSealer>,
    /// Full callback URL registered with Casdoor (the OAuth `redirect_uri`).
    callback_url: Option<String>,
    /// Path component of `callback_url`, matched against the request path.
    callback_path: Option<String>,
    /// OAuth `scope` requested at the authorize step (default `read`).
    scope: String,
    /// Session cookie name (interactive mode).
    cookie_name: String,
    /// Transient login-flow cookie name (interactive mode).
    flow_cookie_name: String,
    /// `Path` attribute of the session and flow cookies (interactive mode).
    /// Scope to a subpath (e.g. `/app_a`) for independent per-app sessions;
    /// must cover `callback_path`. Defaults to `/`.
    cookie_path: String,
    /// Session cookie lifetime in seconds (interactive mode).
    cookie_lifetime: u64,
    /// Optional logout path; a request to it clears the session cookie.
    logout_path: Option<String>,
    /// Randomness source for the anti-CSRF `state`.
    rng: SystemRandom,
    /// Shared pooled outbound HTTP client.
    outbound: Arc<OutboundClient>,
}

impl AuthzCasdoorPlugin {
    /// Builds the plugin from node config.
    ///
    /// Accepted keys:
    /// - `endpoint_addr` (string, **required**): Casdoor base URL (a trailing
    ///   `/` is trimmed).
    /// - `client_id` (string, **required**): Casdoor application client id.
    /// - `client_secret` (string, **required**): Casdoor application client
    ///   secret. Used for HTTP Basic auth on the introspection call (stateless)
    ///   and the code-exchange call (interactive).
    /// - `callback_url` (string): OAuth `redirect_uri`. **Required in interactive
    ///   mode**; accepted-but-unused in stateless mode.
    /// - `ssl_verify` (bool, default `true`): verify the endpoint's TLS certificate.
    /// - `timeout` (integer ms, default `3000`): callout timeout.
    ///
    /// Interactive-mode keys (a session secret ⇒ interactive login is enabled):
    /// - `session_secret` (string) or `session.secret` (string): signing/encryption
    ///   secret for the session and flow cookies. Setting it turns on the SSO flow.
    /// - `session.cookie.name` (string, default `"casdoor_session"`): session cookie name.
    /// - `session.cookie.path` (string, default `"/"`): session/flow cookie `Path`;
    ///   scope to a subpath (e.g. `/app_a`) for independent per-app sessions. Must
    ///   cover the `callback_url` path (rejected at load otherwise).
    /// - `session.cookie.lifetime` (u64 seconds, default `3600`): cookie lifetime.
    /// - `scope` (string, default `"read"`): OAuth scope requested at authorize.
    /// - `logout_path` (string, optional): request path that clears the session
    ///   cookie and redirects to `/`.
    ///
    /// ```yaml
    /// - id: authz
    ///   type: authz-casdoor
    ///   config:
    ///     endpoint_addr: https://casdoor.example.com
    ///     client_id: ${CASDOOR_CLIENT_ID}
    ///     client_secret: ${CASDOOR_CLIENT_SECRET}
    ///     callback_url: https://app.example.com/casdoor/callback
    ///     session_secret: ${CASDOOR_SESSION_SECRET}
    ///     scope: read
    /// ```
    pub fn from_config(
        config: &HashMap<String, serde_json::Value>,
        resources: &Arc<PluginResources>,
    ) -> Result<Self, String> {
        let endpoint_addr = config
            .get("endpoint_addr")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| "authz-casdoor requires 'endpoint_addr'".to_string())?
            .trim_end_matches('/')
            .to_string();

        let client_id = config
            .get("client_id")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| "authz-casdoor requires 'client_id'".to_string())?
            .to_string();

        let client_secret = config
            .get("client_secret")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| "authz-casdoor requires 'client_secret'".to_string())?
            .to_string();

        let ssl_verify = config
            .get("ssl_verify")
            .and_then(|v| v.as_bool())
            .unwrap_or(true);

        let timeout_ms = config
            .get("timeout")
            .and_then(|v| v.as_u64())
            .unwrap_or(3000);

        let callback_url = config
            .get("callback_url")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(|s| s.trim_end_matches('/').to_string());
        let callback_path = callback_url.as_deref().and_then(callback_path_of);

        // Interactive mode is enabled when a session secret is configured.
        let sealer = session_secret(config).map(|s| CookieSealer::new(&s));
        if sealer.is_some() && callback_path.is_none() {
            return Err(
                "authz-casdoor interactive mode (session_secret set) requires a 'callback_url' \
                 with a path component"
                    .to_string(),
            );
        }

        let scope = config
            .get("scope")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .unwrap_or("read")
            .to_string();
        let cookie_name =
            session_cookie_str(config, "name").unwrap_or_else(|| "casdoor_session".to_string());
        let flow_cookie_name = format!("{cookie_name}_flow");
        let cookie_path = session_cookie_str(config, "path").unwrap_or_else(|| "/".to_string());
        // The callback must receive the flow/session cookies, so the cookie path
        // has to cover the callback path (interactive mode only, where a callback
        // path is guaranteed present by the check above). Otherwise login loops.
        if let Some(cb) = callback_path.as_deref() {
            if sealer.is_some() && !path_covers(&cookie_path, cb) {
                return Err(format!(
                    "authz-casdoor: session.cookie.path '{}' does not cover the callback_url \
                     path '{}'; the session cookie would not reach the callback and login \
                     would loop. Set session.cookie.path to a prefix of the callback path.",
                    cookie_path, cb
                ));
            }
        }
        let cookie_lifetime = session_cookie_u64(config, "lifetime").unwrap_or(3_600);
        let logout_path = config
            .get("logout_path")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(String::from);

        Ok(Self {
            basic_auth: basic_auth_header(&client_id, &client_secret),
            endpoint_addr,
            client_id,
            client_secret,
            ssl_verify,
            timeout: Duration::from_millis(timeout_ms),
            sealer,
            callback_url,
            callback_path,
            scope,
            cookie_name,
            flow_cookie_name,
            cookie_path,
            cookie_lifetime,
            logout_path,
            rng: SystemRandom::new(),
            outbound: resources.outbound.clone(),
        })
    }

    /// Builds the 403 denial carrying the context.
    fn deny(ctx: Context, message: impl Into<String>) -> PluginResult {
        let mut ctx = ctx;
        ctx.response.status_code = 403;
        ctx.response.body = Bytes::from(r#"{"error":"access_denied"}"#);
        ctx.response.headers.insert(
            "content-type".to_string(),
            vec!["application/json".to_string()],
        );
        Err(PluginExecutionError {
            context: ctx,
            error: GatewayError {
                node_id: String::new(),
                code: "AUTHZ_CASDOOR_DENIED".to_string(),
                message: message.into(),
                metadata: HashMap::new(),
            },
        })
    }

    /// Builds a `302` early-exit carrying the prepared response. Wire the
    /// node's **error** edge to `client.in` so this reaches the browser.
    fn redirect(mut ctx: Context, location: String, set_cookies: Vec<String>) -> PluginResult {
        ctx.response.status_code = 302;
        ctx.response
            .headers
            .insert("location".to_string(), vec![location]);
        if !set_cookies.is_empty() {
            ctx.response
                .headers
                .insert("set-cookie".to_string(), set_cookies);
        }
        ctx.response.body = Bytes::new();
        Err(PluginExecutionError {
            context: ctx,
            error: GatewayError {
                node_id: String::new(),
                code: "CASDOOR_REDIRECT".to_string(),
                message: "authz-casdoor redirect".to_string(),
                metadata: HashMap::new(),
            },
        })
    }

    /// Cookie attributes: `HttpOnly`, `SameSite=Lax`, `Secure` only over HTTPS.
    fn cookie_attrs(&self, ctx: &Context, max_age: u64) -> CookieAttrs<'_> {
        CookieAttrs {
            path: &self.cookie_path,
            max_age: Some(max_age),
            http_only: true,
            secure: ctx.request.scheme == "https",
            same_site: SameSite::Lax,
        }
    }

    /// Reads and opens the session cookie, returning the sealed session.
    fn read_session(&self, ctx: &Context) -> Option<CasdoorSession> {
        let sealer = self.sealer.as_ref()?;
        let cookie_header = ctx.request.headers.get("cookie").and_then(|v| v.first())?;
        let raw = read_cookie(cookie_header, &self.cookie_name)?;
        let payload = sealer.open(raw).ok()?;
        serde_json::from_slice(&payload).ok()
    }

    /// Reads and opens the transient login-flow cookie.
    fn read_flow(&self, ctx: &Context) -> Option<CasdoorFlow> {
        let sealer = self.sealer.as_ref()?;
        let cookie_header = ctx.request.headers.get("cookie").and_then(|v| v.first())?;
        let raw = read_cookie(cookie_header, &self.flow_cookie_name)?;
        let payload = sealer.open(raw).ok()?;
        serde_json::from_slice(&payload).ok()
    }

    /// Attaches the authenticated identity from a session to the request.
    fn attach_session(&self, ctx: &mut Context, session: &CasdoorSession) {
        ctx.request.headers.insert(
            "authorization".to_string(),
            vec![format!("Bearer {}", session.access_token)],
        );
        if let Some(claims) = &session.claims {
            if let Some(sub) = claims.get("sub") {
                ctx.message.insert("user_id".to_string(), sub.clone());
            }
            ctx.message.insert("jwt_claims".to_string(), claims.clone());
        }
    }

    /// Exchanges an authorization `code` for a Casdoor access token.
    async fn fetch_access_token(&self, code: &str) -> Result<String, String> {
        let request = OutboundRequest {
            method: http::Method::POST,
            url: format!("{}/api/login/oauth/access_token", self.endpoint_addr),
            headers: vec![(
                "content-type".to_string(),
                "application/x-www-form-urlencoded".to_string(),
            )],
            body: Bytes::from(access_token_body(
                code,
                &self.client_id,
                &self.client_secret,
            )),
            timeout: self.timeout,
            ssl_verify: self.ssl_verify,
        };
        let resp = self
            .outbound
            .request(request)
            .await
            .map_err(|e| format!("Casdoor token exchange failed: {e}"))?;
        if resp.status != 200 {
            return Err(format!(
                "Casdoor token endpoint returned status {}",
                resp.status
            ));
        }
        parse_access_token(&resp.body)
    }

    /// Interactive SSO flow: callback → valid session → begin login.
    async fn execute_interactive(&self, mut ctx: Context) -> PluginResult {
        let sealer = self
            .sealer
            .as_ref()
            .expect("execute_interactive only called when a sealer is configured");

        // 0. Logout: clear the session cookie and bounce to "/".
        if let Some(ref logout_path) = self.logout_path {
            if &ctx.request.path == logout_path {
                let del = delete_cookie(&self.cookie_name, &self.cookie_path);
                return Self::redirect(ctx, "/".to_string(), vec![del]);
            }
        }

        // 1. Callback: request path matches callback_url path and carries
        //    code+state. Validate state, exchange the code, seal a session.
        if self.is_callback(&ctx) {
            return self.handle_callback(ctx, sealer).await;
        }

        // 2. Valid session cookie (for this client_id) → authenticate from it.
        if let Some(session) = self.read_session(&ctx) {
            if session.client_id == self.client_id {
                self.attach_session(&mut ctx, &session);
                return Ok(PluginOutput {
                    context: ctx,
                    named_outputs: HashMap::new(),
                });
            }
        }

        // 3. No session, not a callback → begin login at Casdoor.
        self.begin_login(ctx, sealer)
    }

    /// True when the request is the OAuth callback (path + `code` + `state`).
    fn is_callback(&self, ctx: &Context) -> bool {
        self.callback_path.as_deref() == Some(ctx.request.path.as_str())
            && ctx.request.query_params.contains_key("code")
            && ctx.request.query_params.contains_key("state")
    }

    /// Handles the OAuth callback: verify state, exchange code, seal a session,
    /// and redirect to the original URI.
    async fn handle_callback(&self, ctx: Context, sealer: &CookieSealer) -> PluginResult {
        let flow = match self.read_flow(&ctx) {
            Some(f) => f,
            None => return Self::deny(ctx, "missing or invalid login-flow cookie"),
        };
        let state = query_first(&ctx, "state").unwrap_or_default();
        if state != flow.state {
            return Self::deny(ctx, "OAuth state mismatch");
        }
        let code = match query_first(&ctx, "code") {
            Some(c) if !c.is_empty() => c,
            _ => return Self::deny(ctx, "missing authorization code"),
        };

        let access_token = match self.fetch_access_token(&code).await {
            Ok(t) => t,
            Err(e) => return Self::deny(ctx, e),
        };

        let claims = decode_jwt_claims(&access_token);
        let session = CasdoorSession {
            access_token,
            client_id: self.client_id.clone(),
            claims,
        };
        let payload = serde_json::to_vec(&session).unwrap_or_default();
        let sealed = sealer.seal(&payload, Duration::from_secs(self.cookie_lifetime));
        let set_session = build_set_cookie(
            &self.cookie_name,
            &sealed,
            &self.cookie_attrs(&ctx, self.cookie_lifetime),
        );
        let del_flow = delete_cookie(&self.flow_cookie_name, &self.cookie_path);
        Self::redirect(ctx, flow.original_uri, vec![set_session, del_flow])
    }

    /// Begins interactive login: mint a `state`, stash it plus the original URI
    /// in a short-lived flow cookie, and redirect to Casdoor's authorize URL.
    fn begin_login(&self, ctx: Context, sealer: &CookieSealer) -> PluginResult {
        let state = random_state(&self.rng);
        let original_uri = reconstruct_uri(&ctx);
        let flow = CasdoorFlow {
            state: state.clone(),
            original_uri,
        };
        let payload = serde_json::to_vec(&flow).unwrap_or_default();
        // Short-lived: the flow cookie only needs to survive the round trip.
        let sealed = sealer.seal(&payload, Duration::from_secs(300));
        let set_flow = build_set_cookie(
            &self.flow_cookie_name,
            &sealed,
            &self.cookie_attrs(&ctx, 300),
        );

        let callback = self.callback_url.as_deref().unwrap_or("");
        let authorize = build_authorize_url(
            &self.endpoint_addr,
            &self.client_id,
            callback,
            &state,
            &self.scope,
        );
        Self::redirect(ctx, authorize, vec![set_flow])
    }

    /// Stateless bearer-token validation via introspection (default behavior).
    async fn execute_stateless(&self, ctx: Context) -> PluginResult {
        let token = match extract_token(&ctx) {
            Some(t) => t,
            None => return Self::deny(ctx, "missing Casdoor access token"),
        };

        let request = OutboundRequest {
            method: http::Method::POST,
            url: introspect_url(&self.endpoint_addr),
            headers: vec![
                (
                    "content-type".to_string(),
                    "application/x-www-form-urlencoded".to_string(),
                ),
                ("authorization".to_string(), self.basic_auth.clone()),
            ],
            body: Bytes::from(introspect_body(&token)),
            timeout: self.timeout,
            ssl_verify: self.ssl_verify,
        };

        match self.outbound.request(request).await {
            Ok(resp) if resp.status == 200 && token_is_active(&resp.body) => Ok(PluginOutput {
                context: ctx,
                named_outputs: HashMap::new(),
            }),
            Ok(resp) => Self::deny(
                ctx,
                format!(
                    "Casdoor token inactive or rejected (status {})",
                    resp.status
                ),
            ),
            Err(e) => {
                let detail = match &e {
                    OutboundError::Timeout(d) => format!("Casdoor request timed out after {d:?}"),
                    OutboundError::InvalidRequest(m) => format!("invalid Casdoor request: {m}"),
                    OutboundError::Transport(m) => format!("Casdoor request failed: {m}"),
                };
                Self::deny(ctx, detail)
            }
        }
    }
}

/// Reads the session secret from `session_secret` or nested `session.secret`.
fn session_secret(config: &HashMap<String, serde_json::Value>) -> Option<String> {
    config
        .get("session_secret")
        .and_then(|v| v.as_str())
        .or_else(|| {
            config
                .get("session")
                .and_then(|s| s.get("secret"))
                .and_then(|v| v.as_str())
        })
        .filter(|s| !s.is_empty())
        .map(String::from)
}

/// Reads a string field from nested `session.cookie.<key>`, falling back to the
/// flat `session_cookie_<key>` form (used by the UI schema).
fn session_cookie_str(config: &HashMap<String, serde_json::Value>, key: &str) -> Option<String> {
    config
        .get("session")
        .and_then(|s| s.get("cookie"))
        .and_then(|c| c.get(key))
        .or_else(|| config.get(&format!("session_cookie_{key}")))
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(String::from)
}

/// Reads a u64 field from nested `session.cookie.<key>`, falling back to the
/// flat `session_cookie_<key>` form (used by the UI schema).
fn session_cookie_u64(config: &HashMap<String, serde_json::Value>, key: &str) -> Option<u64> {
    config
        .get("session")
        .and_then(|s| s.get("cookie"))
        .and_then(|c| c.get(key))
        .or_else(|| config.get(&format!("session_cookie_{key}")))
        .and_then(|v| v.as_u64())
}

/// Extracts the path component of a callback URL (`https://h/p?x` → `/p`).
/// Mirrors the APISIX regex `.+//[^/]+(/.*)`.
fn callback_path_of(url: &str) -> Option<String> {
    let after_scheme = url.split_once("://").map(|(_, rest)| rest).unwrap_or(url);
    let slash = after_scheme.find('/')?;
    let path = &after_scheme[slash..];
    // Strip query/fragment; the request path never carries them.
    let path = path.split(['?', '#']).next().unwrap_or(path);
    if path.is_empty() {
        None
    } else {
        Some(path.to_string())
    }
}

/// Builds the `Authorization: Basic <base64(client_id:client_secret)>` value.
fn basic_auth_header(client_id: &str, client_secret: &str) -> String {
    let raw = format!("{client_id}:{client_secret}");
    format!("Basic {}", STANDARD.encode(raw.as_bytes()))
}

/// The Casdoor OAuth token-introspection endpoint for a base URL.
fn introspect_url(endpoint_addr: &str) -> String {
    format!("{endpoint_addr}/api/login/oauth/introspect")
}

/// Builds the Casdoor authorize URL that begins the login handshake.
fn build_authorize_url(
    endpoint_addr: &str,
    client_id: &str,
    callback_url: &str,
    state: &str,
    scope: &str,
) -> String {
    format!(
        "{}/login/oauth/authorize?response_type=code&client_id={}&redirect_uri={}&state={}&scope={}",
        endpoint_addr,
        form_encode(client_id),
        form_encode(callback_url),
        form_encode(state),
        form_encode(scope),
    )
}

/// Encodes the code-exchange request body.
fn access_token_body(code: &str, client_id: &str, client_secret: &str) -> String {
    format!(
        "grant_type=authorization_code&code={}&client_id={}&client_secret={}",
        form_encode(code),
        form_encode(client_id),
        form_encode(client_secret),
    )
}

/// Parses the token endpoint's JSON, returning the `access_token` when the
/// reply is a valid, unexpired grant (`expires_in > 0`, per Casdoor).
fn parse_access_token(body: &[u8]) -> Result<String, String> {
    let data: serde_json::Value =
        serde_json::from_slice(body).map_err(|e| format!("failed to parse Casdoor token: {e}"))?;
    let token = data
        .get("access_token")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "Casdoor token response missing access_token".to_string())?;
    // Casdoor signals an invalid token with expires_in <= 0.
    if let Some(expires) = data.get("expires_in") {
        let secs = expires
            .as_i64()
            .or_else(|| expires.as_str().and_then(|s| s.parse().ok()));
        if matches!(secs, Some(n) if n <= 0) {
            return Err("Casdoor returned an expired/invalid access_token".to_string());
        }
    }
    Ok(token.to_string())
}

/// Decodes a JWT's claim set (middle segment) without verifying the signature.
/// The token came directly from Casdoor's token endpoint over TLS, so it is
/// trusted here; the claims are only used to surface identity to the upstream.
fn decode_jwt_claims(token: &str) -> Option<serde_json::Value> {
    let mut parts = token.split('.');
    let _header = parts.next()?;
    let payload = parts.next()?;
    let bytes = URL_SAFE_NO_PAD.decode(payload).ok()?;
    let value: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    if value.is_object() {
        Some(value)
    } else {
        None
    }
}

/// Generates a random anti-CSRF `state` (128 bits, hex-encoded).
fn random_state(rng: &SystemRandom) -> String {
    let mut bytes = [0u8; 16];
    rng.fill(&mut bytes).expect("system RNG must produce state");
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Reconstructs the request URI (`path` plus a best-effort query string) so the
/// browser can be returned there after login.
fn reconstruct_uri(ctx: &Context) -> String {
    let mut uri = ctx.request.path.clone();
    if !ctx.request.query_params.is_empty() {
        let mut pairs: Vec<String> = Vec::new();
        for (k, values) in &ctx.request.query_params {
            for v in values {
                if v.is_empty() {
                    pairs.push(k.clone());
                } else {
                    pairs.push(format!("{k}={v}"));
                }
            }
        }
        uri.push('?');
        uri.push_str(&pairs.join("&"));
    }
    uri
}

/// The first value of a query parameter.
fn query_first(ctx: &Context, key: &str) -> Option<String> {
    ctx.request
        .query_params
        .get(key)
        .and_then(|v| v.first())
        .cloned()
}

/// Extracts the raw bearer token (without the `Bearer ` prefix) from the
/// `Authorization` header.
fn extract_token(ctx: &Context) -> Option<String> {
    let raw = ctx
        .request
        .headers
        .get("authorization")
        .and_then(|v| v.first())?
        .as_str();
    let stripped = raw
        .strip_prefix("Bearer ")
        .or_else(|| raw.strip_prefix("bearer "))
        .unwrap_or(raw);
    let token = stripped.trim();
    if token.is_empty() {
        None
    } else {
        Some(token.to_string())
    }
}

/// Encodes the introspection request body (`token` + `token_type_hint`).
fn introspect_body(token: &str) -> String {
    format!("token={}&token_type_hint=access_token", form_encode(token))
}

/// Percent-encodes a value for `application/x-www-form-urlencoded` bodies.
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

/// Parses an RFC 7662 introspection response, returning the `active` flag.
/// A missing/false/non-boolean `active` is treated as inactive.
fn token_is_active(body: &[u8]) -> bool {
    serde_json::from_slice::<serde_json::Value>(body)
        .ok()
        .and_then(|v| v.get("active").and_then(|a| a.as_bool()))
        .unwrap_or(false)
}

#[async_trait]
impl Plugin for AuthzCasdoorPlugin {
    fn plugin_type(&self) -> &str {
        "authz-casdoor"
    }

    async fn execute(
        &self,
        ctx: Context,
        _named_inputs: &HashMap<String, serde_json::Value>,
    ) -> PluginResult {
        if self.sealer.is_some() {
            self.execute_interactive(ctx).await
        } else {
            self.execute_stateless(ctx).await
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

    fn ctx(path: &str, query: HashMap<String, Vec<String>>) -> Context {
        Context {
            request: GatewayRequest {
                method: "GET".to_string(),
                path: path.to_string(),
                host: "app.example.com".to_string(),
                scheme: "https".to_string(),
                headers: HashMap::new(),
                query_params: query,
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

    fn stateless_cfg() -> HashMap<String, serde_json::Value> {
        let mut config = HashMap::new();
        config.insert(
            "endpoint_addr".to_string(),
            serde_json::json!("https://casdoor.example.com/"),
        );
        config.insert("client_id".to_string(), serde_json::json!("id"));
        config.insert("client_secret".to_string(), serde_json::json!("secret"));
        config
    }

    fn interactive_cfg() -> HashMap<String, serde_json::Value> {
        let mut config = stateless_cfg();
        config.insert(
            "callback_url".to_string(),
            serde_json::json!("https://app.example.com/casdoor/callback"),
        );
        config.insert("session_secret".to_string(), serde_json::json!("s3cr3t"));
        config
    }

    #[test]
    fn test_basic_auth_header() {
        // base64("id:secret") = aWQ6c2VjcmV0
        assert_eq!(basic_auth_header("id", "secret"), "Basic aWQ6c2VjcmV0");
    }

    #[test]
    fn test_introspect_url_and_body() {
        assert_eq!(
            introspect_url("https://casdoor.example.com"),
            "https://casdoor.example.com/api/login/oauth/introspect"
        );
        assert_eq!(
            introspect_body("abc.def"),
            "token=abc.def&token_type_hint=access_token"
        );
    }

    #[test]
    fn test_extract_token() {
        assert_eq!(
            extract_token(&ctx_with_auth(Some("Bearer abc"))).as_deref(),
            Some("abc")
        );
        assert_eq!(
            extract_token(&ctx_with_auth(Some("bearer abc"))).as_deref(),
            Some("abc")
        );
        // raw token without prefix is accepted
        assert_eq!(
            extract_token(&ctx_with_auth(Some("abc"))).as_deref(),
            Some("abc")
        );
        assert_eq!(extract_token(&ctx_with_auth(None)), None);
        assert_eq!(extract_token(&ctx_with_auth(Some("Bearer "))), None);
    }

    #[test]
    fn test_token_is_active() {
        assert!(token_is_active(br#"{"active": true, "sub": "u1"}"#));
        assert!(!token_is_active(br#"{"active": false}"#));
        assert!(!token_is_active(br#"{"sub": "u1"}"#));
        assert!(!token_is_active(b"not json"));
    }

    #[tokio::test]
    async fn test_missing_token_denied() {
        let plugin =
            AuthzCasdoorPlugin::from_config(&stateless_cfg(), &PluginResources::empty()).unwrap();
        // trailing slash trimmed
        assert_eq!(plugin.endpoint_addr, "https://casdoor.example.com");
        // stateless by default
        assert!(plugin.sealer.is_none());
        let err = plugin
            .execute(ctx_with_auth(None), &HashMap::new())
            .await
            .unwrap_err();
        assert_eq!(err.error.code, "AUTHZ_CASDOOR_DENIED");
        assert_eq!(err.context.response.status_code, 403);
    }

    #[test]
    fn test_requires_endpoint_and_credentials() {
        assert!(
            AuthzCasdoorPlugin::from_config(&HashMap::new(), &PluginResources::empty()).is_err()
        );
        let mut config = HashMap::new();
        config.insert(
            "endpoint_addr".to_string(),
            serde_json::json!("https://casdoor"),
        );
        config.insert("client_id".to_string(), serde_json::json!("id"));
        // missing client_secret
        assert!(AuthzCasdoorPlugin::from_config(&config, &PluginResources::empty()).is_err());
    }

    #[test]
    fn test_interactive_requires_callback_url() {
        // session_secret set but no callback_url ⇒ rejected at load.
        let mut config = stateless_cfg();
        config.insert("session_secret".to_string(), serde_json::json!("s3cr3t"));
        assert!(AuthzCasdoorPlugin::from_config(&config, &PluginResources::empty()).is_err());
    }

    #[test]
    fn test_interactive_config_defaults() {
        let p =
            AuthzCasdoorPlugin::from_config(&interactive_cfg(), &PluginResources::empty()).unwrap();
        assert!(p.sealer.is_some());
        assert_eq!(p.cookie_name, "casdoor_session");
        assert_eq!(p.flow_cookie_name, "casdoor_session_flow");
        assert_eq!(p.cookie_lifetime, 3600);
        assert_eq!(p.cookie_path, "/");
        assert_eq!(p.scope, "read");
        assert_eq!(p.callback_path.as_deref(), Some("/casdoor/callback"));
    }

    #[test]
    fn test_session_cookie_path_configurable_and_validated() {
        // A path that covers the callback (/casdoor/callback) is accepted...
        let mut config = interactive_cfg();
        config.insert(
            "session".to_string(),
            serde_json::json!({ "secret": "s3cr3t", "cookie": { "path": "/casdoor" } }),
        );
        let p = AuthzCasdoorPlugin::from_config(&config, &PluginResources::empty()).unwrap();
        assert_eq!(p.cookie_path, "/casdoor");

        // ...the flat UI key works too...
        let mut config = interactive_cfg();
        config.insert("session_cookie_path".to_string(), serde_json::json!("/"));
        assert!(AuthzCasdoorPlugin::from_config(&config, &PluginResources::empty()).is_ok());

        // ...but a path that does NOT cover the callback is rejected at load.
        let mut config = interactive_cfg();
        config.insert(
            "session_cookie_path".to_string(),
            serde_json::json!("/other"),
        );
        let err = AuthzCasdoorPlugin::from_config(&config, &PluginResources::empty())
            .err()
            .unwrap();
        assert!(
            err.contains("session.cookie.path"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn test_callback_path_of() {
        assert_eq!(
            callback_path_of("https://app.example.com/casdoor/callback").as_deref(),
            Some("/casdoor/callback")
        );
        assert_eq!(callback_path_of("http://h/cb?x=1").as_deref(), Some("/cb"));
        // no path component
        assert_eq!(callback_path_of("https://app.example.com"), None);
    }

    #[test]
    fn test_build_authorize_url() {
        let url = build_authorize_url(
            "https://casdoor.example.com",
            "my-client",
            "https://app.example.com/casdoor/callback",
            "abcd1234",
            "read",
        );
        assert_eq!(
            url,
            "https://casdoor.example.com/login/oauth/authorize?response_type=code\
&client_id=my-client\
&redirect_uri=https%3A%2F%2Fapp.example.com%2Fcasdoor%2Fcallback\
&state=abcd1234&scope=read"
        );
    }

    #[test]
    fn test_access_token_body_and_parse() {
        assert_eq!(
            access_token_body("the code", "cid", "csecret"),
            "grant_type=authorization_code&code=the+code&client_id=cid&client_secret=csecret"
        );
        assert_eq!(
            parse_access_token(br#"{"access_token":"tok","expires_in":3600}"#).unwrap(),
            "tok"
        );
        // expires_in <= 0 ⇒ invalid
        assert!(parse_access_token(br#"{"access_token":"tok","expires_in":0}"#).is_err());
        // missing access_token ⇒ error
        assert!(parse_access_token(br#"{"error":"bad"}"#).is_err());
    }

    #[test]
    fn test_session_seal_open_round_trip() {
        let sealer = CookieSealer::new("s3cr3t");
        let session = CasdoorSession {
            access_token: "tok-123".into(),
            client_id: "id".into(),
            claims: Some(serde_json::json!({ "sub": "u1", "name": "Alice" })),
        };
        let payload = serde_json::to_vec(&session).unwrap();
        let cookie = sealer.seal(&payload, Duration::from_secs(3600));
        let opened = sealer.open(&cookie).unwrap();
        let back: CasdoorSession = serde_json::from_slice(&opened).unwrap();
        assert_eq!(back.access_token, "tok-123");
        assert_eq!(back.client_id, "id");
        assert_eq!(back.claims.unwrap().get("sub").unwrap(), "u1");
    }

    #[test]
    fn test_decode_jwt_claims() {
        // header.payload.sig where payload = {"sub":"u1"}
        let payload = URL_SAFE_NO_PAD.encode(br#"{"sub":"u1","name":"Bob"}"#);
        let token = format!("aGVhZGVy.{payload}.c2ln");
        let claims = decode_jwt_claims(&token).unwrap();
        assert_eq!(claims.get("sub").unwrap(), "u1");
        // opaque (non-JWT) token ⇒ None
        assert!(decode_jwt_claims("opaque-token").is_none());
    }

    #[tokio::test]
    async fn test_interactive_begin_login_redirects() {
        let p =
            AuthzCasdoorPlugin::from_config(&interactive_cfg(), &PluginResources::empty()).unwrap();
        let err = p
            .execute(ctx("/protected", HashMap::new()), &HashMap::new())
            .await
            .unwrap_err();
        assert_eq!(err.error.code, "CASDOOR_REDIRECT");
        assert_eq!(err.context.response.status_code, 302);
        let location = &err.context.response.headers.get("location").unwrap()[0];
        assert!(
            location.starts_with("https://casdoor.example.com/login/oauth/authorize?"),
            "{location}"
        );
        assert!(location.contains("response_type=code"));
        // A sealed flow cookie is set.
        let set = &err.context.response.headers.get("set-cookie").unwrap()[0];
        assert!(set.starts_with("casdoor_session_flow="), "{set}");
    }

    #[tokio::test]
    async fn test_interactive_valid_session_passes() {
        let p =
            AuthzCasdoorPlugin::from_config(&interactive_cfg(), &PluginResources::empty()).unwrap();
        let sealer = CookieSealer::new("s3cr3t");
        let session = CasdoorSession {
            access_token: "tok-xyz".into(),
            client_id: "id".into(),
            claims: Some(serde_json::json!({ "sub": "u1" })),
        };
        let sealed = sealer.seal(
            &serde_json::to_vec(&session).unwrap(),
            Duration::from_secs(3600),
        );

        let mut c = ctx("/protected", HashMap::new());
        c.request.headers.insert(
            "cookie".to_string(),
            vec![format!("casdoor_session={}", sealed)],
        );

        let out = p.execute(c, &HashMap::new()).await.unwrap();
        assert_eq!(
            out.context.request.headers.get("authorization").unwrap()[0],
            "Bearer tok-xyz"
        );
        assert_eq!(out.context.message.get("user_id").unwrap(), "u1");
    }

    #[tokio::test]
    async fn test_interactive_callback_bad_state_denied() {
        let p =
            AuthzCasdoorPlugin::from_config(&interactive_cfg(), &PluginResources::empty()).unwrap();
        let sealer = CookieSealer::new("s3cr3t");
        let flow = CasdoorFlow {
            state: "expected".into(),
            original_uri: "/home".into(),
        };
        let sealed = sealer.seal(
            &serde_json::to_vec(&flow).unwrap(),
            Duration::from_secs(300),
        );

        let mut query = HashMap::new();
        query.insert("code".to_string(), vec!["c".to_string()]);
        query.insert("state".to_string(), vec!["WRONG".to_string()]);
        let mut c = ctx("/casdoor/callback", query);
        c.request.headers.insert(
            "cookie".to_string(),
            vec![format!("casdoor_session_flow={}", sealed)],
        );

        let err = p.execute(c, &HashMap::new()).await.unwrap_err();
        assert_eq!(err.error.code, "AUTHZ_CASDOOR_DENIED");
        assert_eq!(err.context.response.status_code, 403);
    }

    #[test]
    fn test_reconstruct_uri() {
        let mut query = HashMap::new();
        query.insert("a".to_string(), vec!["1".to_string()]);
        assert_eq!(reconstruct_uri(&ctx("/p", query)), "/p?a=1");
        assert_eq!(reconstruct_uri(&ctx("/p", HashMap::new())), "/p");
    }
}
