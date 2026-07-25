//! OpenID Connect authentication plugin (`openid-connect`).
//!
//! Two modes, selected by `bearer_only`:
//!
//! - **Resource-server / bearer mode** (`bearer_only: true`, the default):
//!   validates an OAuth2 / OIDC **access token** presented as a `Bearer` token
//!   in the `Authorization` header and, on success, exposes the token claims to
//!   downstream nodes via `context.message`. Validation is either local JWT
//!   verification against the provider's JWKS (by `kid`, cached with a TTL,
//!   refetched once on an unknown `kid`) or RFC 7662 token introspection.
//!
//! - **Interactive login** (`bearer_only: false`): the full **Authorization
//!   Code flow with PKCE**. An unauthenticated browser is redirected to the
//!   identity provider; the provider redirects back to `redirect_uri` with a
//!   code; the plugin exchanges it for tokens, validates the `id_token`, and
//!   seals the resulting claims into an **encrypted client-side session
//!   cookie** (see [`crate::plugins::util::cookie_session`]). Subsequent
//!   requests carrying a valid session cookie are let through with the claims
//!   attached. No server-side session store is needed, so this works across a
//!   horizontally-scaled deployment as long as every instance shares
//!   `session.secret`.
//!
//! # Flow wiring (interactive mode)
//!
//! In interactive mode the node exits through its **error port** whenever it
//! needs the browser to move (the 302 to the IdP, the post-callback 302 back to
//! the original URL, or a `401`); the prepared response already sits on the
//! context. Wire the node's `error` edge to `client.in`. Only a request that
//! arrives with a valid session cookie continues out the `success` port toward
//! the upstream. The node must be on a route whose match rule also covers the
//! `redirect_uri` path so the callback reaches it.
//!
//! # Deviations from APISIX
//!
//! - **No server-side session revocation.** Sessions live entirely in the
//!   encrypted cookie, so a session cannot be invalidated before its
//!   `session.cookie.lifetime` expiry without a shared denylist (a future
//!   feature). Use short lifetimes. This is the standard client-side-cookie
//!   trade-off APISIX shares when configured for cookie sessions.
//! - **No token refresh** in this version: when the session cookie expires the
//!   user re-authenticates (a fresh, fast redirect round-trip if the IdP
//!   session is still valid).
//! - Only the Authorization Code grant is implemented (the OIDC gateway case);
//!   implicit/hybrid flows are not.

use async_trait::async_trait;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use bytes::Bytes;
use jsonwebtoken::{decode, decode_header, Algorithm, DecodingKey, Validation};
use ring::digest::{digest, SHA256};
use ring::rand::{SecureRandom, SystemRandom};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

use crate::context::{Context, GatewayError};
use crate::outbound::{OutboundRequest, OutboundResponse};
use crate::plugins::resources::PluginResources;
use crate::plugins::util::cookie_session::{
    build_set_cookie, delete_cookie, path_covers, read_cookie, CookieAttrs, CookieSealer, SameSite,
};
use crate::plugins::{Plugin, PluginExecutionError, PluginOutput, PluginResult};

/// Transient state carried in the short-lived flow cookie across the redirect
/// to the IdP and back to the callback (CSRF `state`, replay `nonce`, PKCE
/// `verifier`, and where to send the browser after login).
#[derive(Serialize, Deserialize)]
struct FlowState {
    state: String,
    nonce: String,
    verifier: String,
    original_uri: String,
}

/// The sealed session payload: the validated identity, kept small.
#[derive(Serialize, Deserialize)]
struct SessionData {
    claims: serde_json::Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    access_token: Option<String>,
}

/// Interactive-mode configuration, present only when `bearer_only: false`.
struct Interactive {
    sealer: CookieSealer,
    authorization_endpoint_cfg: Option<String>,
    token_endpoint_cfg: Option<String>,
    redirect_uri: String,
    /// Path portion of `redirect_uri`, matched to detect the callback.
    redirect_path: String,
    scope: String,
    session_cookie: String,
    flow_cookie: String,
    /// `Path` attribute for the session and flow cookies. Scoping this to the
    /// app's subpath (e.g. `/app_a`) lets two openid-connect nodes on distinct
    /// subpaths hold independent sessions in the browser. Defaults to `/`.
    cookie_path: String,
    session_lifetime: Duration,
    logout_path: Option<String>,
    post_logout_redirect_uri: String,
    authz_endpoint_resolved: Mutex<Option<String>>,
    token_endpoint_resolved: Mutex<Option<String>>,
}

/// A single JSON Web Key from a provider's JWKS document.
#[derive(Debug, Clone, Deserialize)]
// Mirrors the JWK spec; some members are deserialized for completeness but not
// consulted during verification.
#[allow(dead_code)]
struct Jwk {
    kty: String,
    kid: Option<String>,
    alg: Option<String>,
    // RSA
    n: Option<String>,
    e: Option<String>,
    // EC
    x: Option<String>,
    y: Option<String>,
    crv: Option<String>,
}

/// A JWKS document (`{ "keys": [ ... ] }`).
#[derive(Debug, Clone, Deserialize)]
struct JwkSet {
    keys: Vec<Jwk>,
}

/// Cached JWKS with the time it was fetched, for TTL-based expiry.
struct CachedJwks {
    keys: Vec<Jwk>,
    fetched_at: Instant,
}

/// Authenticates requests by validating a bearer access token via JWKS
/// signature verification or token introspection.
pub struct OpenidConnectPlugin {
    /// Well-known discovery URL; resolves `jwks_uri` when not given directly.
    discovery: Option<String>,
    /// Explicit JWKS endpoint (takes precedence over discovery).
    jwks_uri_cfg: Option<String>,
    /// Introspection endpoint; used only when no JWKS source is configured.
    introspection_endpoint: Option<String>,
    client_id: Option<String>,
    client_secret: Option<String>,
    ssl_verify: bool,
    timeout: Duration,
    /// Signature algorithms the token is allowed to be signed with.
    allowed_algs: Vec<Algorithm>,
    /// Issuers accepted for the `iss` claim; empty = do not validate issuer.
    valid_issuers: Vec<String>,
    audience_claim: String,
    audience_required: bool,
    audience_match_client_id: bool,
    set_userinfo_header: bool,
    set_access_token_header: bool,
    access_token_in_authorization_header: bool,
    /// True when a JWKS source (discovery/jwks_uri) is configured.
    use_jwks: bool,
    jwk_ttl: Duration,
    resources: Arc<PluginResources>,
    /// Lazily-resolved JWKS URI (from discovery), cached for the process.
    jwks_uri_resolved: Mutex<Option<String>>,
    jwks_cache: Mutex<Option<CachedJwks>>,
    /// Interactive login flow; `None` in bearer-only mode.
    interactive: Option<Interactive>,
}

impl OpenidConnectPlugin {
    /// Builds the plugin from node config (bearer-only subset).
    ///
    /// Accepted keys:
    /// - `discovery` (string): OIDC discovery URL
    ///   (`.../.well-known/openid-configuration`); used to resolve `jwks_uri`.
    /// - `jwks_uri` (string): explicit JWKS endpoint; takes precedence over
    ///   `discovery` for signature verification.
    /// - `introspection_endpoint` (string): RFC 7662 introspection endpoint;
    ///   used only when no JWKS source is configured.
    /// - `client_id` (string): OAuth client id (introspection auth / audience).
    /// - `client_secret` (string): OAuth client secret (introspection auth).
    /// - `bearer_only` (bool, default `true`): **must be true**. `false` is
    ///   rejected at load — interactive login is not supported (see module docs).
    /// - `token_signing_alg_values_expected` (string or array): permitted
    ///   signature algorithms (e.g. `RS256`, `ES256`). Defaults to
    ///   `RS256, RS384, RS512, ES256, ES384`.
    /// - `claim_validator.issuer.valid_issuers` (array): accepted `iss` values.
    /// - `claim_validator.audience.{claim,required,match_with_client_id}`:
    ///   audience validation (claim defaults to `aud`).
    /// - `set_userinfo_header` (bool, default `true`): base64-encode the claims
    ///   into the `X-Userinfo` request header for the upstream.
    /// - `set_access_token_header` (bool, default `true`) /
    ///   `access_token_in_authorization_header` (bool, default `false`):
    ///   forward the validated access token as `X-Access-Token` (or leave it in
    ///   `Authorization`).
    /// - `ssl_verify` (bool, default `true`), `timeout` (integer seconds,
    ///   default `3`).
    ///
    /// Rejected at load: `bearer_only: false`, and configs with neither a JWKS
    /// source (`discovery`/`jwks_uri`) nor an `introspection_endpoint`.
    ///
    /// ```yaml
    /// type: openid-connect
    /// config:
    ///   discovery: https://idp.example.com/.well-known/openid-configuration
    ///   bearer_only: true
    ///   client_id: my-api
    ///   token_signing_alg_values_expected: RS256
    ///   claim_validator:
    ///     issuer:
    ///       valid_issuers: ["https://idp.example.com/"]
    ///     audience:
    ///       required: true
    ///       match_with_client_id: true
    /// ```
    pub fn from_config(
        config: &HashMap<String, serde_json::Value>,
        resources: &Arc<PluginResources>,
    ) -> Result<Self, String> {
        let bearer_only = config
            .get("bearer_only")
            .and_then(|v| v.as_bool())
            .unwrap_or(true);

        let discovery = string_opt(config, "discovery");
        let jwks_uri_cfg = string_opt(config, "jwks_uri");
        let introspection_endpoint = string_opt(config, "introspection_endpoint");

        let use_jwks = discovery.is_some() || jwks_uri_cfg.is_some();
        if !use_jwks && introspection_endpoint.is_none() {
            return Err(
                "openid-connect: requires a JWKS source ('discovery' or 'jwks_uri') \
                 or an 'introspection_endpoint'"
                    .to_string(),
            );
        }

        let client_id = string_opt(config, "client_id");
        let client_secret = string_opt(config, "client_secret");

        // Introspection needs client credentials to authenticate the call.
        if !use_jwks && (client_id.is_none() || client_secret.is_none()) {
            return Err(
                "openid-connect: introspection requires 'client_id' and 'client_secret'"
                    .to_string(),
            );
        }

        // Interactive login (bearer_only: false) needs the Authorization Code
        // machinery: a session secret, client credentials, a redirect URI, and
        // token/authorization endpoints (via discovery or explicit config). The
        // id_token is validated with the same JWKS path as bearer mode.
        let interactive = if bearer_only {
            None
        } else {
            if !use_jwks {
                return Err("openid-connect: interactive login requires a JWKS source \
                            ('discovery' or 'jwks_uri') to validate the id_token"
                    .to_string());
            }
            if client_id.is_none() || client_secret.is_none() {
                return Err(
                    "openid-connect: interactive login requires 'client_id' and \
                            'client_secret'"
                        .to_string(),
                );
            }
            Some(build_interactive(config, &discovery)?)
        };

        let allowed_algs = parse_allowed_algs(config.get("token_signing_alg_values_expected"))?;

        let valid_issuers = read_valid_issuers(config);
        let (audience_claim, audience_required, audience_match_client_id) =
            read_audience_cfg(config);

        let set_userinfo_header = config
            .get("set_userinfo_header")
            .and_then(|v| v.as_bool())
            .unwrap_or(true);
        let set_access_token_header = config
            .get("set_access_token_header")
            .and_then(|v| v.as_bool())
            .unwrap_or(true);
        let access_token_in_authorization_header = config
            .get("access_token_in_authorization_header")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        let ssl_verify = config
            .get("ssl_verify")
            .and_then(|v| v.as_bool())
            .unwrap_or(true);
        let timeout = Duration::from_secs(
            config
                .get("timeout")
                .and_then(|v| v.as_u64())
                .unwrap_or(3)
                .max(1),
        );
        let jwk_ttl = Duration::from_secs(
            config
                .get("jwk_expires_in")
                .and_then(|v| v.as_u64())
                .unwrap_or(86400),
        );

        Ok(Self {
            discovery,
            jwks_uri_cfg,
            introspection_endpoint,
            client_id,
            client_secret,
            ssl_verify,
            timeout,
            allowed_algs,
            valid_issuers,
            audience_claim,
            audience_required,
            audience_match_client_id,
            set_userinfo_header,
            set_access_token_header,
            access_token_in_authorization_header,
            use_jwks,
            jwk_ttl,
            resources: resources.clone(),
            jwks_uri_resolved: Mutex::new(None),
            jwks_cache: Mutex::new(None),
            interactive,
        })
    }

    /// Resolves the JWKS URI, fetching the discovery document once if needed.
    async fn jwks_uri(&self) -> Result<String, String> {
        if let Some(uri) = &self.jwks_uri_cfg {
            return Ok(uri.clone());
        }
        {
            let cached = self.jwks_uri_resolved.lock().await;
            if let Some(uri) = cached.as_ref() {
                return Ok(uri.clone());
            }
        }
        let discovery = self
            .discovery
            .as_ref()
            .ok_or_else(|| "no discovery URL configured".to_string())?;
        let resp = self
            .get(discovery)
            .await
            .map_err(|e| format!("discovery fetch failed: {}", e))?;
        if resp.status != 200 {
            return Err(format!("discovery returned status {}", resp.status));
        }
        let doc: serde_json::Value = serde_json::from_slice(&resp.body)
            .map_err(|e| format!("failed to parse discovery doc: {}", e))?;
        let uri = doc
            .get("jwks_uri")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "discovery doc missing jwks_uri".to_string())?
            .to_string();
        *self.jwks_uri_resolved.lock().await = Some(uri.clone());
        Ok(uri)
    }

    /// Returns the current JWKS, fetching/refreshing when stale or when
    /// `force` is set (used on an unknown `kid`).
    async fn get_jwks(&self, force: bool) -> Result<Vec<Jwk>, String> {
        let mut cache = self.jwks_cache.lock().await;
        if !force {
            if let Some(c) = cache.as_ref() {
                if c.fetched_at.elapsed() < self.jwk_ttl {
                    return Ok(c.keys.clone());
                }
            }
        }
        let uri = self.jwks_uri().await?;
        let resp = self
            .get(&uri)
            .await
            .map_err(|e| format!("JWKS fetch failed: {}", e))?;
        if resp.status != 200 {
            return Err(format!("JWKS endpoint returned status {}", resp.status));
        }
        let set: JwkSet = serde_json::from_slice(&resp.body)
            .map_err(|e| format!("failed to parse JWKS: {}", e))?;
        *cache = Some(CachedJwks {
            keys: set.keys.clone(),
            fetched_at: Instant::now(),
        });
        Ok(set.keys)
    }

    /// Convenience GET through the shared outbound client.
    async fn get(&self, url: &str) -> Result<OutboundResponse, crate::outbound::OutboundError> {
        let req = OutboundRequest {
            method: http::Method::GET,
            url: url.to_string(),
            headers: Vec::new(),
            body: Bytes::new(),
            timeout: self.timeout,
            ssl_verify: self.ssl_verify,
        };
        self.resources.outbound.request(req).await
    }

    /// Validates the token via JWKS signature verification plus claim checks.
    async fn validate_via_jwks(
        &self,
        token: &str,
    ) -> Result<HashMap<String, serde_json::Value>, String> {
        let header = decode_header(token).map_err(|e| format!("invalid JWT header: {}", e))?;
        let kid = header.kid.clone();

        let keys = self.get_jwks(false).await?;
        let jwk = match select_jwk(&keys, kid.as_deref()) {
            Some(j) => j.clone(),
            None => {
                // Unknown kid: refetch once to pick up rotated keys.
                let keys = self.get_jwks(true).await?;
                select_jwk(&keys, kid.as_deref())
                    .cloned()
                    .ok_or_else(|| "no matching JWK for token kid".to_string())?
            }
        };

        let key = jwk_to_decoding_key(&jwk)?;
        // Narrow the permitted algorithms to the ones this key could possibly have
        // signed with. jsonwebtoken rejects the whole Validation if *any* listed
        // algorithm belongs to a different family than the key, so passing the
        // default list (RSA + EC) against an RSA key fails every token. See
        // `algs_for_key`.
        let algs = algs_for_key(&self.allowed_algs, &jwk.kty)?;
        let claims = decode_and_validate(token, &key, &algs)?;
        self.validate_claims(&claims)?;
        Ok(claims)
    }

    /// Validates the token via the introspection endpoint.
    async fn validate_via_introspection(
        &self,
        token: &str,
    ) -> Result<HashMap<String, serde_json::Value>, String> {
        let endpoint = self
            .introspection_endpoint
            .as_ref()
            .ok_or_else(|| "no introspection endpoint configured".to_string())?;
        let client_id = self.client_id.as_deref().unwrap_or("");
        let client_secret = self.client_secret.as_deref().unwrap_or("");
        let basic = BASE64_STANDARD.encode(format!("{}:{}", client_id, client_secret));

        let body = format!("token={}&token_type_hint=access_token", form_encode(token));
        let req = OutboundRequest {
            method: http::Method::POST,
            url: endpoint.clone(),
            headers: vec![
                (
                    "content-type".to_string(),
                    "application/x-www-form-urlencoded".to_string(),
                ),
                ("authorization".to_string(), format!("Basic {}", basic)),
                ("accept".to_string(), "application/json".to_string()),
            ],
            body: Bytes::from(body),
            timeout: self.timeout,
            ssl_verify: self.ssl_verify,
        };
        let resp = self
            .resources
            .outbound
            .request(req)
            .await
            .map_err(|e| format!("introspection callout failed: {}", e))?;
        if resp.status != 200 {
            return Err(format!("introspection returned status {}", resp.status));
        }
        let claims = parse_introspection(&resp.body)?;
        self.validate_claims(&claims)?;
        Ok(claims)
    }

    /// Applies the configured issuer and audience claim checks.
    fn validate_claims(&self, claims: &HashMap<String, serde_json::Value>) -> Result<(), String> {
        // Issuer
        if !self.valid_issuers.is_empty() {
            let iss = claims.get("iss").and_then(|v| v.as_str());
            match iss {
                Some(iss) if self.valid_issuers.iter().any(|v| v == iss) => {}
                _ => return Err("issuer not in valid_issuers".to_string()),
            }
        }

        // Audience
        let aud = claims.get(&self.audience_claim);
        if self.audience_required && aud.is_none() {
            return Err(format!(
                "required audience claim '{}' missing",
                self.audience_claim
            ));
        }
        if self.audience_match_client_id {
            if let Some(aud) = aud {
                let client_id = self.client_id.as_deref().unwrap_or("");
                if !audience_contains(aud, client_id) {
                    return Err("audience does not match client_id".to_string());
                }
            }
        }
        Ok(())
    }

    /// Writes the validated claims into `context.message` and the configured
    /// forwarding headers.
    fn attach(&self, ctx: &mut Context, claims: HashMap<String, serde_json::Value>, token: &str) {
        let claims_value = serde_json::to_value(&claims).unwrap_or_default();
        if let Some(sub) = claims.get("sub") {
            ctx.message.insert("user_id".to_string(), sub.clone());
        }
        ctx.message
            .insert("jwt_claims".to_string(), claims_value.clone());

        if self.set_userinfo_header {
            if let Ok(raw) = serde_json::to_vec(&claims_value) {
                ctx.request
                    .headers
                    .insert("x-userinfo".to_string(), vec![BASE64_STANDARD.encode(raw)]);
            }
        }
        if self.set_access_token_header {
            if self.access_token_in_authorization_header {
                ctx.request.headers.insert(
                    "authorization".to_string(),
                    vec![format!("Bearer {}", token)],
                );
            } else {
                ctx.request
                    .headers
                    .insert("x-access-token".to_string(), vec![token.to_string()]);
            }
        }
    }

    fn reject(ctx: Context, message: &str) -> PluginResult {
        let mut ctx = ctx;
        ctx.response.status_code = 401;
        ctx.response.body = Bytes::from(format!(
            r#"{{"error": "unauthorized", "message": "{}"}}"#,
            message.replace('"', "'")
        ));
        ctx.response.headers.insert(
            "content-type".to_string(),
            vec!["application/json".to_string()],
        );
        ctx.response.headers.insert(
            "www-authenticate".to_string(),
            vec!["Bearer error=\"invalid_token\"".to_string()],
        );
        Err(PluginExecutionError {
            context: ctx,
            error: GatewayError {
                node_id: String::new(),
                code: "OIDC_UNAUTHORIZED".to_string(),
                message: message.to_string(),
                metadata: HashMap::new(),
            },
        })
    }

    // ---- Interactive Authorization Code flow ------------------------------

    /// Drives the interactive flow: session check, callback handling, or a
    /// fresh redirect to the identity provider.
    async fn execute_interactive(&self, mut ctx: Context) -> PluginResult {
        let flow = self.interactive.as_ref().expect("interactive mode");

        // Logout: clear the session cookie and redirect.
        if let Some(logout_path) = &flow.logout_path {
            if ctx.request.path == *logout_path {
                let clear = delete_cookie(&flow.session_cookie, &flow.cookie_path);
                return redirect(ctx, &flow.post_logout_redirect_uri, vec![clear]);
            }
        }

        // Callback: the IdP has redirected back with code + state.
        if ctx.request.path == flow.redirect_path && ctx.request.query_params.contains_key("code") {
            return self.handle_callback(ctx).await;
        }

        // Existing valid session cookie → attach identity and continue.
        if let Some(session) = self.read_session(&ctx) {
            if let Some(sub) = session.claims.get("sub") {
                ctx.message.insert("user_id".to_string(), sub.clone());
            }
            ctx.message
                .insert("jwt_claims".to_string(), session.claims.clone());
            if self.set_userinfo_header {
                if let Ok(raw) = serde_json::to_vec(&session.claims) {
                    ctx.request
                        .headers
                        .insert("x-userinfo".to_string(), vec![BASE64_STANDARD.encode(raw)]);
                }
            }
            if let (true, Some(tok)) = (self.set_access_token_header, &session.access_token) {
                if self.access_token_in_authorization_header {
                    ctx.request
                        .headers
                        .insert("authorization".to_string(), vec![format!("Bearer {}", tok)]);
                } else {
                    ctx.request
                        .headers
                        .insert("x-access-token".to_string(), vec![tok.clone()]);
                }
            }
            return Ok(PluginOutput {
                context: ctx,
                named_outputs: HashMap::new(),
            });
        }

        // No session → begin the Authorization Code flow.
        self.begin_auth(ctx).await
    }

    /// Reads and opens the session cookie, if present and valid.
    fn read_session(&self, ctx: &Context) -> Option<SessionData> {
        let flow = self.interactive.as_ref()?;
        let cookie_header = ctx.request.headers.get("cookie")?.first()?;
        let raw = read_cookie(cookie_header, &flow.session_cookie)?;
        let bytes = flow.sealer.open(raw).ok()?;
        serde_json::from_slice(&bytes).ok()
    }

    /// Starts the flow: generate CSRF/nonce/PKCE, set the flow cookie, and
    /// redirect the browser to the IdP authorization endpoint.
    async fn begin_auth(&self, ctx: Context) -> PluginResult {
        let flow = self.interactive.as_ref().expect("interactive mode");
        let authz = match self.authorization_endpoint().await {
            Ok(u) => u,
            Err(e) => return Self::reject(ctx, &e),
        };

        let state = random_token();
        let nonce = random_token();
        let verifier = random_token();
        let challenge = pkce_challenge(&verifier);
        let original_uri = request_uri(&ctx);

        let flow_state = FlowState {
            state: state.clone(),
            nonce: nonce.clone(),
            verifier,
            original_uri,
        };
        let sealed = match serde_json::to_vec(&flow_state) {
            Ok(b) => flow.sealer.seal(&b, Duration::from_secs(300)),
            Err(e) => return Self::reject(ctx, &format!("flow cookie seal failed: {}", e)),
        };
        let set_flow = build_set_cookie(
            &flow.flow_cookie,
            &sealed,
            &CookieAttrs {
                path: &flow.cookie_path,
                max_age: Some(300),
                http_only: true,
                secure: request_is_https(&ctx),
                same_site: SameSite::Lax,
            },
        );

        let url = format!(
            "{}?response_type=code&client_id={}&redirect_uri={}&scope={}&state={}&nonce={}\
             &code_challenge={}&code_challenge_method=S256",
            authz,
            form_encode(self.client_id.as_deref().unwrap_or("")),
            form_encode(&flow.redirect_uri),
            form_encode(&flow.scope),
            form_encode(&state),
            form_encode(&nonce),
            form_encode(&challenge),
        );
        redirect(ctx, &url, vec![set_flow])
    }

    /// Handles the IdP redirect back: verify state, exchange the code, validate
    /// the id_token, seal a session cookie, and redirect to the original URL.
    async fn handle_callback(&self, ctx: Context) -> PluginResult {
        let flow = self.interactive.as_ref().expect("interactive mode");

        let code = first_query(&ctx, "code").unwrap_or_default();
        let state = first_query(&ctx, "state").unwrap_or_default();

        // Recover and validate the flow cookie (CSRF).
        let flow_state = match self.read_flow(&ctx) {
            Some(f) => f,
            None => return Self::reject(ctx, "missing or invalid login flow cookie"),
        };
        if flow_state.state != state || state.is_empty() {
            return Self::reject(ctx, "state mismatch (possible CSRF)");
        }

        // Exchange the authorization code for tokens.
        let token_endpoint = match self.token_endpoint().await {
            Ok(u) => u,
            Err(e) => return Self::reject(ctx, &e),
        };
        let tokens = match self
            .exchange_code(
                &token_endpoint,
                &code,
                &flow_state.verifier,
                &flow.redirect_uri,
            )
            .await
        {
            Ok(t) => t,
            Err(e) => return Self::reject(ctx, &e),
        };

        let id_token = match tokens.get("id_token").and_then(|v| v.as_str()) {
            Some(t) => t.to_string(),
            None => return Self::reject(ctx, "token response missing id_token"),
        };
        let access_token = tokens
            .get("access_token")
            .and_then(|v| v.as_str())
            .map(String::from);

        // Validate the id_token signature/claims via the JWKS path and check
        // the nonce binds it to this login attempt.
        let claims = match self.validate_via_jwks(&id_token).await {
            Ok(c) => c,
            Err(e) => return Self::reject(ctx, &format!("id_token validation failed: {}", e)),
        };
        if claims.get("nonce").and_then(|v| v.as_str()) != Some(flow_state.nonce.as_str()) {
            return Self::reject(ctx, "id_token nonce mismatch");
        }

        // Seal the session and redirect to where the user was going.
        let session = SessionData {
            claims: serde_json::to_value(&claims).unwrap_or_default(),
            access_token,
        };
        let sealed = match serde_json::to_vec(&session) {
            Ok(b) => flow.sealer.seal(&b, flow.session_lifetime),
            Err(e) => return Self::reject(ctx, &format!("session seal failed: {}", e)),
        };
        let set_session = build_set_cookie(
            &flow.session_cookie,
            &sealed,
            &CookieAttrs {
                path: &flow.cookie_path,
                max_age: Some(flow.session_lifetime.as_secs()),
                http_only: true,
                secure: request_is_https(&ctx),
                same_site: SameSite::Lax,
            },
        );
        let clear_flow = delete_cookie(&flow.flow_cookie, &flow.cookie_path);
        let target = if flow_state.original_uri.is_empty() {
            "/".to_string()
        } else {
            flow_state.original_uri.clone()
        };
        redirect(ctx, &target, vec![set_session, clear_flow])
    }

    /// Reads and opens the transient flow cookie.
    fn read_flow(&self, ctx: &Context) -> Option<FlowState> {
        let flow = self.interactive.as_ref()?;
        let cookie_header = ctx.request.headers.get("cookie")?.first()?;
        let raw = read_cookie(cookie_header, &flow.flow_cookie)?;
        let bytes = flow.sealer.open(raw).ok()?;
        serde_json::from_slice(&bytes).ok()
    }

    /// Exchanges an authorization code for tokens at the token endpoint.
    async fn exchange_code(
        &self,
        token_endpoint: &str,
        code: &str,
        verifier: &str,
        redirect_uri: &str,
    ) -> Result<serde_json::Value, String> {
        let client_id = self.client_id.as_deref().unwrap_or("");
        let client_secret = self.client_secret.as_deref().unwrap_or("");
        let basic = BASE64_STANDARD.encode(format!("{}:{}", client_id, client_secret));
        let body = format!(
            "grant_type=authorization_code&code={}&redirect_uri={}&code_verifier={}&client_id={}",
            form_encode(code),
            form_encode(redirect_uri),
            form_encode(verifier),
            form_encode(client_id),
        );
        let req = OutboundRequest {
            method: http::Method::POST,
            url: token_endpoint.to_string(),
            headers: vec![
                (
                    "content-type".to_string(),
                    "application/x-www-form-urlencoded".to_string(),
                ),
                ("authorization".to_string(), format!("Basic {}", basic)),
                ("accept".to_string(), "application/json".to_string()),
            ],
            body: Bytes::from(body),
            timeout: self.timeout,
            ssl_verify: self.ssl_verify,
        };
        let resp = self
            .resources
            .outbound
            .request(req)
            .await
            .map_err(|e| format!("token exchange callout failed: {}", e))?;
        if resp.status != 200 {
            return Err(format!("token endpoint returned status {}", resp.status));
        }
        serde_json::from_slice(&resp.body).map_err(|e| format!("invalid token response: {}", e))
    }

    /// Resolves the authorization endpoint (config or discovery).
    async fn authorization_endpoint(&self) -> Result<String, String> {
        let flow = self.interactive.as_ref().expect("interactive mode");
        if let Some(u) = &flow.authorization_endpoint_cfg {
            return Ok(u.clone());
        }
        self.discovery_field("authorization_endpoint", &flow.authz_endpoint_resolved)
            .await
    }

    /// Resolves the token endpoint (config or discovery).
    async fn token_endpoint(&self) -> Result<String, String> {
        let flow = self.interactive.as_ref().expect("interactive mode");
        if let Some(u) = &flow.token_endpoint_cfg {
            return Ok(u.clone());
        }
        self.discovery_field("token_endpoint", &flow.token_endpoint_resolved)
            .await
    }

    /// Reads a URL field from the discovery document, memoizing the result.
    async fn discovery_field(
        &self,
        field: &str,
        cache: &Mutex<Option<String>>,
    ) -> Result<String, String> {
        {
            if let Some(u) = cache.lock().await.as_ref() {
                return Ok(u.clone());
            }
        }
        let discovery = self
            .discovery
            .as_ref()
            .ok_or_else(|| format!("no discovery URL to resolve {}", field))?;
        let resp = self
            .get(discovery)
            .await
            .map_err(|e| format!("discovery fetch failed: {}", e))?;
        if resp.status != 200 {
            return Err(format!("discovery returned status {}", resp.status));
        }
        let doc: serde_json::Value = serde_json::from_slice(&resp.body)
            .map_err(|e| format!("failed to parse discovery doc: {}", e))?;
        let uri = doc
            .get(field)
            .and_then(|v| v.as_str())
            .ok_or_else(|| format!("discovery doc missing {}", field))?
            .to_string();
        *cache.lock().await = Some(uri.clone());
        Ok(uri)
    }
}

/// Builds the interactive-mode configuration from the plugin config.
fn build_interactive(
    config: &HashMap<String, serde_json::Value>,
    discovery: &Option<String>,
) -> Result<Interactive, String> {
    let secret = session_field(config, "secret")
        .or_else(|| string_opt(config, "session_secret"))
        .ok_or("openid-connect: interactive login requires 'session.secret'")?;

    let redirect_uri = string_opt(config, "redirect_uri")
        .ok_or("openid-connect: interactive login requires 'redirect_uri'")?;
    let redirect_path = url_path(&redirect_uri);

    let authorization_endpoint_cfg = string_opt(config, "authorization_endpoint");
    let token_endpoint_cfg = string_opt(config, "token_endpoint");
    if discovery.is_none() && (authorization_endpoint_cfg.is_none() || token_endpoint_cfg.is_none())
    {
        return Err(
            "openid-connect: interactive login requires 'discovery', or both \
                    'authorization_endpoint' and 'token_endpoint'"
                .to_string(),
        );
    }

    let scope = string_opt(config, "scope").unwrap_or_else(|| "openid".to_string());
    let session_cookie =
        session_cookie_field(config, "name").unwrap_or_else(|| "oidc_session".to_string());
    let cookie_path = session_cookie_field(config, "path").unwrap_or_else(|| "/".to_string());
    // The callback must be reachable with the session/flow cookies attached, so
    // the cookie path has to cover the redirect_uri path. Otherwise the browser
    // withholds the flow cookie on the callback and login loops forever — fail
    // fast at load instead of shipping a silently-broken route.
    if !path_covers(&cookie_path, &redirect_path) {
        return Err(format!(
            "openid-connect: session.cookie.path '{}' does not cover the redirect_uri \
             path '{}'; the session cookie would not be sent to the callback and login \
             would loop. Set session.cookie.path to a prefix of the callback path.",
            cookie_path, redirect_path
        ));
    }
    let session_lifetime = Duration::from_secs(
        config
            .get("session")
            .and_then(|s| s.get("cookie"))
            .and_then(|c| c.get("lifetime"))
            .or_else(|| config.get("session_cookie_lifetime"))
            .and_then(|v| v.as_u64())
            .unwrap_or(3600),
    );

    Ok(Interactive {
        sealer: CookieSealer::new(&secret),
        authorization_endpoint_cfg,
        token_endpoint_cfg,
        redirect_uri,
        redirect_path,
        scope,
        flow_cookie: format!("{}_flow", session_cookie),
        session_cookie,
        cookie_path,
        session_lifetime,
        logout_path: string_opt(config, "logout_path"),
        post_logout_redirect_uri: string_opt(config, "post_logout_redirect_uri")
            .unwrap_or_else(|| "/".to_string()),
        authz_endpoint_resolved: Mutex::new(None),
        token_endpoint_resolved: Mutex::new(None),
    })
}

/// Reads `session.<field>` as a string.
fn session_field(config: &HashMap<String, serde_json::Value>, field: &str) -> Option<String> {
    config
        .get("session")
        .and_then(|s| s.get(field))
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(String::from)
}

/// Reads a session cookie string field from nested `session.cookie.<field>`,
/// falling back to the flat `session_cookie_<field>` form the Web UI schema
/// emits (the SchemaForm is flat and cannot author nested maps).
fn session_cookie_field(
    config: &HashMap<String, serde_json::Value>,
    field: &str,
) -> Option<String> {
    config
        .get("session")
        .and_then(|s| s.get("cookie"))
        .and_then(|c| c.get(field))
        .or_else(|| config.get(&format!("session_cookie_{field}")))
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(String::from)
}

/// Extracts the path portion of a URL (everything from the first `/` after the
/// authority), defaulting to `/`.
fn url_path(url: &str) -> String {
    let after_scheme = url.split("://").nth(1).unwrap_or(url);
    match after_scheme.find('/') {
        Some(i) => {
            let path = &after_scheme[i..];
            path.split(['?', '#']).next().unwrap_or(path).to_string()
        }
        None => "/".to_string(),
    }
}

/// Rebuilds the request URI (path plus sorted query string) for the
/// post-login redirect target.
fn request_uri(ctx: &Context) -> String {
    let mut pairs: Vec<String> = Vec::new();
    for (k, values) in &ctx.request.query_params {
        for v in values {
            pairs.push(format!("{}={}", form_encode(k), form_encode(v)));
        }
    }
    pairs.sort();
    if pairs.is_empty() {
        ctx.request.path.clone()
    } else {
        format!("{}?{}", ctx.request.path, pairs.join("&"))
    }
}

/// First value of a query parameter.
fn first_query(ctx: &Context, name: &str) -> Option<String> {
    ctx.request
        .query_params
        .get(name)
        .and_then(|v| v.first())
        .cloned()
}

/// True when the request arrived over HTTPS (controls the cookie `Secure` flag).
fn request_is_https(ctx: &Context) -> bool {
    ctx.request.scheme.eq_ignore_ascii_case("https")
}

/// A URL-safe random token (32 bytes → base64url) for state/nonce/PKCE.
fn random_token() -> String {
    let mut bytes = [0u8; 32];
    SystemRandom::new()
        .fill(&mut bytes)
        .expect("system RNG must produce random bytes");
    URL_SAFE_NO_PAD.encode(bytes)
}

/// PKCE S256 challenge: base64url(SHA-256(verifier)).
fn pkce_challenge(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(digest(&SHA256, verifier.as_bytes()).as_ref())
}

/// Prepares a 302 redirect on the context and exits through the error port
/// (the node's error edge should be wired to `client.in`).
fn redirect(mut ctx: Context, location: &str, set_cookies: Vec<String>) -> PluginResult {
    ctx.response.status_code = 302;
    ctx.response.body = Bytes::new();
    ctx.response
        .headers
        .insert("location".to_string(), vec![location.to_string()]);
    if !set_cookies.is_empty() {
        ctx.response
            .headers
            .insert("set-cookie".to_string(), set_cookies);
    }
    Err(PluginExecutionError {
        context: ctx,
        error: GatewayError {
            node_id: String::new(),
            code: "OIDC_REDIRECT".to_string(),
            message: "redirecting for interactive login".to_string(),
            metadata: HashMap::new(),
        },
    })
}

/// Reads an optional non-empty string config value.
fn string_opt(config: &HashMap<String, serde_json::Value>, key: &str) -> Option<String> {
    config
        .get(key)
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(String::from)
}

/// Parses one algorithm name into a [`jsonwebtoken::Algorithm`] (asymmetric
/// only — OIDC JWKS keys are RSA/EC).
fn parse_alg(name: &str) -> Option<Algorithm> {
    match name {
        "RS256" => Some(Algorithm::RS256),
        "RS384" => Some(Algorithm::RS384),
        "RS512" => Some(Algorithm::RS512),
        "PS256" => Some(Algorithm::PS256),
        "PS384" => Some(Algorithm::PS384),
        "PS512" => Some(Algorithm::PS512),
        "ES256" => Some(Algorithm::ES256),
        "ES384" => Some(Algorithm::ES384),
        _ => None,
    }
}

/// Parses `token_signing_alg_values_expected` (string, comma/space list, or
/// array) into the allowed-algorithm set, defaulting to the common asymmetric
/// algorithms.
fn parse_allowed_algs(value: Option<&serde_json::Value>) -> Result<Vec<Algorithm>, String> {
    let default = || {
        vec![
            Algorithm::RS256,
            Algorithm::RS384,
            Algorithm::RS512,
            Algorithm::ES256,
            Algorithm::ES384,
        ]
    };
    let names: Vec<String> = match value {
        None => return Ok(default()),
        Some(serde_json::Value::String(s)) => s
            .split([',', ' '])
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(String::from)
            .collect(),
        Some(serde_json::Value::Array(a)) => a
            .iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect(),
        Some(_) => {
            return Err("token_signing_alg_values_expected must be a string or array".to_string())
        }
    };
    if names.is_empty() {
        return Ok(default());
    }
    let mut algs = Vec::new();
    for name in names {
        match parse_alg(&name) {
            Some(a) => algs.push(a),
            None => {
                return Err(format!(
                    "unsupported token signing algorithm '{}' \
                     (supported: RS256/384/512, PS256/384/512, ES256/384)",
                    name
                ))
            }
        }
    }
    Ok(algs)
}

/// Reads `claim_validator.issuer.valid_issuers`.
fn read_valid_issuers(config: &HashMap<String, serde_json::Value>) -> Vec<String> {
    config
        .get("claim_validator")
        .and_then(|v| v.get("issuer"))
        .and_then(|v| v.get("valid_issuers"))
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

/// Reads `claim_validator.audience.{claim,required,match_with_client_id}`.
fn read_audience_cfg(config: &HashMap<String, serde_json::Value>) -> (String, bool, bool) {
    let audience = config
        .get("claim_validator")
        .and_then(|v| v.get("audience"));
    let claim = audience
        .and_then(|a| a.get("claim"))
        .and_then(|v| v.as_str())
        .unwrap_or("aud")
        .to_string();
    let required = audience
        .and_then(|a| a.get("required"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let match_client = audience
        .and_then(|a| a.get("match_with_client_id"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    (claim, required, match_client)
}

/// Selects the JWK matching `kid`, or the sole key when no `kid` is present.
fn select_jwk<'a>(keys: &'a [Jwk], kid: Option<&str>) -> Option<&'a Jwk> {
    match kid {
        Some(kid) => keys.iter().find(|k| k.kid.as_deref() == Some(kid)),
        None => {
            if keys.len() == 1 {
                keys.first()
            } else {
                None
            }
        }
    }
}

/// Whether `alg` can be verified with a key of JWK type `kty`.
fn alg_matches_kty(alg: Algorithm, kty: &str) -> bool {
    match alg {
        Algorithm::RS256
        | Algorithm::RS384
        | Algorithm::RS512
        | Algorithm::PS256
        | Algorithm::PS384
        | Algorithm::PS512 => kty == "RSA",
        Algorithm::ES256 | Algorithm::ES384 => kty == "EC",
        Algorithm::HS256 | Algorithm::HS384 | Algorithm::HS512 => kty == "oct",
        Algorithm::EdDSA => kty == "OKP",
    }
}

/// Narrows the configured algorithms to those a `kty` key can verify.
///
/// This is not an optimization — it is required for correctness. `jsonwebtoken`
/// validates the *whole* algorithm list against the key family before it even
/// looks at the token:
///
/// ```ignore
/// for alg in &validation.algorithms {
///     if key.family != alg.family() { return Err(InvalidAlgorithm); }
/// }
/// ```
///
/// So a list spanning two families can never verify anything. The default
/// `token_signing_alg_values_expected` spans RSA *and* EC, which meant every
/// JWKS-verified token — bearer tokens and interactive `id_token`s alike — was
/// rejected with `InvalidAlgorithm` unless the operator happened to pin a single
/// family. Filtering per key keeps the permissive default working with whichever
/// key the IdP actually published.
fn algs_for_key(allowed: &[Algorithm], kty: &str) -> Result<Vec<Algorithm>, String> {
    let algs: Vec<Algorithm> = allowed
        .iter()
        .copied()
        .filter(|a| alg_matches_kty(*a, kty))
        .collect();
    if algs.is_empty() {
        return Err(format!(
            "no permitted signing algorithm can verify a '{}' key; \
             check token_signing_alg_values_expected",
            kty
        ));
    }
    Ok(algs)
}

/// Builds a [`DecodingKey`] from a JWK based on its key type.
fn jwk_to_decoding_key(jwk: &Jwk) -> Result<DecodingKey, String> {
    match jwk.kty.as_str() {
        "RSA" => {
            let n = jwk.n.as_deref().ok_or("RSA JWK missing 'n'")?;
            let e = jwk.e.as_deref().ok_or("RSA JWK missing 'e'")?;
            DecodingKey::from_rsa_components(n, e).map_err(|e| format!("invalid RSA JWK: {}", e))
        }
        "EC" => {
            let x = jwk.x.as_deref().ok_or("EC JWK missing 'x'")?;
            let y = jwk.y.as_deref().ok_or("EC JWK missing 'y'")?;
            DecodingKey::from_ec_components(x, y).map_err(|e| format!("invalid EC JWK: {}", e))
        }
        other => Err(format!("unsupported JWK key type '{}'", other)),
    }
}

/// Verifies the token signature (against `key`, restricted to `allowed_algs`)
/// and `exp`, returning the decoded claims. Issuer/audience are validated
/// separately by [`OpenidConnectPlugin::validate_claims`].
fn decode_and_validate(
    token: &str,
    key: &DecodingKey,
    allowed_algs: &[Algorithm],
) -> Result<HashMap<String, serde_json::Value>, String> {
    let first = allowed_algs.first().copied().unwrap_or(Algorithm::RS256);
    let mut validation = Validation::new(first);
    validation.algorithms = allowed_algs.to_vec();
    validation.validate_exp = true;
    // Issuer/audience handled manually to honor the plugin's flags precisely.
    validation.validate_aud = false;
    decode::<HashMap<String, serde_json::Value>>(token, key, &validation)
        .map(|data| data.claims)
        .map_err(|e| format!("token verification failed: {}", e))
}

/// Parses an RFC 7662 introspection response, requiring `active: true`.
fn parse_introspection(body: &[u8]) -> Result<HashMap<String, serde_json::Value>, String> {
    let value: serde_json::Value = serde_json::from_slice(body)
        .map_err(|e| format!("invalid introspection response: {}", e))?;
    let active = value
        .get("active")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    if !active {
        return Err("token is not active".to_string());
    }
    let map = value
        .as_object()
        .map(|m| m.clone().into_iter().collect())
        .unwrap_or_default();
    Ok(map)
}

/// True when `aud` equals `client_id` (string aud) or contains it (array aud).
fn audience_contains(aud: &serde_json::Value, client_id: &str) -> bool {
    match aud {
        serde_json::Value::String(s) => s == client_id,
        serde_json::Value::Array(arr) => arr.iter().any(|v| v.as_str() == Some(client_id)),
        _ => false,
    }
}

/// Extracts the bearer token from an `Authorization` header value.
fn parse_bearer(header_value: &str) -> Option<&str> {
    let mut parts = header_value.splitn(2, ' ');
    let scheme = parts.next()?;
    let token = parts.next()?.trim();
    if scheme.eq_ignore_ascii_case("bearer") && !token.is_empty() {
        Some(token)
    } else {
        None
    }
}

/// Percent-encodes a token for an `application/x-www-form-urlencoded` body.
fn form_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for b in value.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{:02X}", b)),
        }
    }
    out
}

#[async_trait]
impl Plugin for OpenidConnectPlugin {
    fn plugin_type(&self) -> &str {
        "openid-connect"
    }

    async fn execute(
        &self,
        mut ctx: Context,
        _named_inputs: &HashMap<String, serde_json::Value>,
    ) -> PluginResult {
        // Strip any client-supplied userinfo header before authentication.
        ctx.request.headers.remove("x-userinfo");

        // Interactive login (bearer_only: false) runs the cookie-session flow.
        if self.interactive.is_some() {
            return self.execute_interactive(ctx).await;
        }

        let token = ctx
            .request
            .headers
            .get("authorization")
            .and_then(|v| v.first())
            .and_then(|v| parse_bearer(v))
            .map(String::from);

        let token = match token {
            Some(t) => t,
            None => return Self::reject(ctx, "No bearer token found in request"),
        };

        let result = if self.use_jwks {
            self.validate_via_jwks(&token).await
        } else {
            self.validate_via_introspection(&token).await
        };

        match result {
            Ok(claims) => {
                self.attach(&mut ctx, claims, &token);
                Ok(PluginOutput {
                    context: ctx,
                    named_outputs: HashMap::new(),
                })
            }
            Err(e) => Self::reject(ctx, &e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jsonwebtoken::{encode, EncodingKey, Header};

    // Test RSA keypair (PKCS#8). The public modulus/exponent below are the same
    // key, expressed as JWK n/e, so a token signed with PRIV_PEM verifies
    // against the JWK — exercising the JWKS → DecodingKey path end to end.
    const PRIV_PEM: &str = "-----BEGIN PRIVATE KEY-----\n\
MIIEvAIBADANBgkqhkiG9w0BAQEFAASCBKYwggSiAgEAAoIBAQCvciOuri5uG88q\n\
rZ3T6qUhTYl7nWDHvVGBBsA8ku3xUfOW97PGpWbTe/Yq/3jovVxAQsAe/QoIMyUU\n\
HKCdDKAsIBO9j9OEPs3Le6cThFx+/9Z1U9cw4wCIa4TNtGBhyDgqbqKpOLNnXLI6\n\
WEcrykkoV5nUUH/47aS2i9BiqZn6H9eEL1VH82IX/x4fWNIEyXQAxKZtyULgznR4\n\
oUz2QPaY/cWtpK85B12scs1IpLnzEdjy69t28ZQnYZ7Nrvl+aFjSkvnqxhoNJ9Ut\n\
Lw2/3vld8t3Lh6B4vTM4vdJsue1dum6WnyEKEx/SDuCSDxWONfmdhu/B4XUghaQS\n\
1wBNiEhvAgMBAAECggEASXDcee8ktWfDsShK9F35MLcd0VaAICxiFUInr1OL8ePt\n\
tSjMIt+y6t0tnzMgwEAgATBP7sjabbNHFqOjIgqac84bpVKy5l1J1R9WQWe7NlhO\n\
w/9MCYVEgFaNmXQjklr3E+ALDA4VnzNg0eaJKE39kLsWxBbMcv27YMSm/t3i/B2s\n\
rwZbzBgxXXR5r7j/Tt+hRJmGHXe0zZvsNLzFNj4CsyngBiY9CIcexroGxd3yGEf7\n\
0PKHwbZKkH0CPr6QAc4f+tPgIfHB+8+29QPrUTR9e60Sc6dZNUjTr1EWIxyvFxVK\n\
dI3ekR5W26a81+yxc2MpRK8wZsv+mJ6okaeVs2+3jQKBgQDr0b3YX4RC9trW+RsE\n\
9wUXeLr3o9Vb0FTHf/8ALAZ9EWywEmF+sdA8fKs8+H+IyIzX6KGw/UbzqIi2aDuJ\n\
q63IPxKyyXr7nfVSUz8qWIGT/WoG/1d4rpFN2sbR/r/oue7uJnaXMIPswVT+zO8q\n\
5YieEPDwhteJ8bJUC16NWwddBQKBgQC+dcEmNm7MzxI/cuwubkojhayXw1ouACu4\n\
giGp3lJywzIAnV1CsJTGTpvHk31j+/L9oB2U/586+65MGklGJ2TGs0IQZs0iAy1H\n\
Oq3zzsLp0KiVizyqchgkIWP6KVpx5aPkpJSgPJGyJzuwofZwRzPK7IZr8c4MOtsy\n\
M8j8up8p4wKBgGbUxTYvIJuazX7kjXWyydOcX9tQ497vj6iXFflbOVEcYgq9WSpI\n\
G4fkzT7/FY3t9gzIcomdSG1D1qnD9gJojJU/e8XeufQywyEtD+RFR+vim3OFsPz9\n\
EnuipQQ5VDIFsjzDJP90tnJtM8UQVFKeWN6kgIxCIIcUkDC57HczdJiJAoGASPG4\n\
g/YdAXvdNUfChRXgdzJfI9DB3RRbqlLMqc5oLWPs5qdebIhMspawuwMV5xE7wz9r\n\
lQFB7sktvB/lKGU2B5PoHXgB4KDu2nTy4omxxPMRXhTxqyX/cPcI32qvJSgaWRtf\n\
gO8xrdWw2rltNRtQDsv/v5/glnaENPn4ZDLlepkCgYAqag5Uxj0ps6WNE/D6IEWA\n\
eTGicEEJPJQB9bGrElna7WyOjntnO5miRmpM1jH39R417czBURmvZHO2oTnqghZF\n\
c/7P2kweQNU7vtM/iLcm8EyFRw2lVB3J/XVTEcPU6ZeZHlVbGtiKx3gukkMBc4Ct\n\
CQTyrvDSz5J6MQhLtbNHnQ==\n\
-----END PRIVATE KEY-----\n";

    const JWK_N: &str = "r3Ijrq4ubhvPKq2d0-qlIU2Je51gx71RgQbAPJLt8VHzlvezxqVm03v2Kv946L1cQELAHv0KCDMlFBygnQygLCATvY_ThD7Ny3unE4Rcfv_WdVPXMOMAiGuEzbRgYcg4Km6iqTizZ1yyOlhHK8pJKFeZ1FB_-O2ktovQYqmZ-h_XhC9VR_NiF_8eH1jSBMl0AMSmbclC4M50eKFM9kD2mP3FraSvOQddrHLNSKS58xHY8uvbdvGUJ2Geza75fmhY0pL56sYaDSfVLS8Nv975XfLdy4egeL0zOL3SbLntXbpulp8hChMf0g7gkg8VjjX5nYbvweF1IIWkEtcATYhIbw";
    const JWK_E: &str = "AQAB";

    fn test_jwk(kid: &str) -> Jwk {
        Jwk {
            kty: "RSA".to_string(),
            kid: Some(kid.to_string()),
            alg: Some("RS256".to_string()),
            n: Some(JWK_N.to_string()),
            e: Some(JWK_E.to_string()),
            x: None,
            y: None,
            crv: None,
        }
    }

    fn sign(claims: serde_json::Value, kid: &str) -> String {
        let mut header = Header::new(Algorithm::RS256);
        header.kid = Some(kid.to_string());
        encode(
            &header,
            &claims,
            &EncodingKey::from_rsa_pem(PRIV_PEM.as_bytes()).unwrap(),
        )
        .unwrap()
    }

    fn cfg(pairs: &[(&str, serde_json::Value)]) -> HashMap<String, serde_json::Value> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect()
    }

    #[test]
    fn test_interactive_requires_secret_and_redirect() {
        // bearer_only:false without session.secret / redirect_uri is rejected.
        let missing = cfg(&[
            (
                "discovery",
                serde_json::json!("https://idp/.well-known/openid-configuration"),
            ),
            ("bearer_only", serde_json::json!(false)),
            ("client_id", serde_json::json!("app")),
            ("client_secret", serde_json::json!("s")),
        ]);
        assert!(OpenidConnectPlugin::from_config(&missing, &PluginResources::empty()).is_err());

        // Fully configured interactive mode builds.
        let ok = cfg(&[
            (
                "discovery",
                serde_json::json!("https://idp/.well-known/openid-configuration"),
            ),
            ("bearer_only", serde_json::json!(false)),
            ("client_id", serde_json::json!("app")),
            ("client_secret", serde_json::json!("s")),
            (
                "redirect_uri",
                serde_json::json!("https://app.example.com/oidc/callback"),
            ),
            (
                "session",
                serde_json::json!({ "secret": "cookie-signing-secret" }),
            ),
        ]);
        let plugin = OpenidConnectPlugin::from_config(&ok, &PluginResources::empty()).unwrap();
        let interactive = plugin.interactive.as_ref().unwrap();
        assert_eq!(interactive.redirect_path, "/oidc/callback");
        assert_eq!(interactive.session_cookie, "oidc_session");
        assert_eq!(interactive.flow_cookie, "oidc_session_flow");
        // Defaults: whole-origin cookie, one-hour lifetime.
        assert_eq!(interactive.cookie_path, "/");
        assert_eq!(interactive.session_lifetime, Duration::from_secs(3600));
    }

    /// Two nodes on distinct subpaths can carry independent, path-scoped sessions
    /// with their own names and lifetimes — the /app_a vs /app_b case.
    #[test]
    fn test_interactive_custom_session_cookie() {
        let c = cfg(&[
            (
                "discovery",
                serde_json::json!("https://idp/.well-known/openid-configuration"),
            ),
            ("bearer_only", serde_json::json!(false)),
            ("client_id", serde_json::json!("app")),
            ("client_secret", serde_json::json!("s")),
            (
                "redirect_uri",
                serde_json::json!("https://app.example.com/app_a/callback"),
            ),
            (
                "session",
                serde_json::json!({
                    "secret": "cookie-signing-secret",
                    "cookie": { "name": "a_session", "path": "/app_a", "lifetime": 900 }
                }),
            ),
        ]);
        let plugin = OpenidConnectPlugin::from_config(&c, &PluginResources::empty()).unwrap();
        let i = plugin.interactive.as_ref().unwrap();
        assert_eq!(i.session_cookie, "a_session");
        assert_eq!(i.flow_cookie, "a_session_flow");
        assert_eq!(i.cookie_path, "/app_a");
        assert_eq!(i.session_lifetime, Duration::from_secs(900));
    }

    /// The Web UI's flat `session_cookie_*` keys are honored just like the
    /// nested `session.cookie.*` form, so session properties edited in the UI
    /// take effect.
    #[test]
    fn test_interactive_flat_ui_session_keys() {
        let c = cfg(&[
            (
                "discovery",
                serde_json::json!("https://idp/.well-known/openid-configuration"),
            ),
            ("bearer_only", serde_json::json!(false)),
            ("client_id", serde_json::json!("app")),
            ("client_secret", serde_json::json!("s")),
            (
                "redirect_uri",
                serde_json::json!("https://app.example.com/app_a/callback"),
            ),
            ("session_secret", serde_json::json!("cookie-signing-secret")),
            ("session_cookie_name", serde_json::json!("a_session")),
            ("session_cookie_path", serde_json::json!("/app_a")),
            ("session_cookie_lifetime", serde_json::json!(1200)),
        ]);
        let plugin = OpenidConnectPlugin::from_config(&c, &PluginResources::empty()).unwrap();
        let i = plugin.interactive.as_ref().unwrap();
        assert_eq!(i.session_cookie, "a_session");
        assert_eq!(i.flow_cookie, "a_session_flow");
        assert_eq!(i.cookie_path, "/app_a");
        assert_eq!(i.session_lifetime, Duration::from_secs(1200));
    }

    /// A cookie path that does not cover the callback is rejected at load — it
    /// would starve the callback of the flow cookie and loop login forever.
    #[test]
    fn test_interactive_cookie_path_must_cover_callback() {
        let c = cfg(&[
            (
                "discovery",
                serde_json::json!("https://idp/.well-known/openid-configuration"),
            ),
            ("bearer_only", serde_json::json!(false)),
            ("client_id", serde_json::json!("app")),
            ("client_secret", serde_json::json!("s")),
            (
                "redirect_uri",
                serde_json::json!("https://app.example.com/app_a/callback"),
            ),
            (
                "session",
                serde_json::json!({
                    "secret": "s",
                    "cookie": { "path": "/app_b" }  // callback is under /app_a
                }),
            ),
        ]);
        // `.err().unwrap()` (not `unwrap_err()`): the Ok type isn't `Debug`.
        let err = OpenidConnectPlugin::from_config(&c, &PluginResources::empty())
            .err()
            .unwrap();
        assert!(
            err.contains("session.cookie.path"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn test_bearer_only_default_has_no_interactive() {
        let c = cfg(&[("jwks_uri", serde_json::json!("https://idp/jwks"))]);
        let plugin = OpenidConnectPlugin::from_config(&c, &PluginResources::empty()).unwrap();
        assert!(plugin.interactive.is_none());
    }

    #[test]
    fn test_url_path() {
        assert_eq!(
            url_path("https://app.example.com/oidc/callback"),
            "/oidc/callback"
        );
        assert_eq!(url_path("https://app.example.com/cb?x=1"), "/cb");
        assert_eq!(url_path("https://app.example.com"), "/");
    }

    #[test]
    fn test_pkce_challenge_is_stable_and_urlsafe() {
        // RFC 7636 test vector: verifier -> S256 challenge.
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        let challenge = pkce_challenge(verifier);
        assert_eq!(challenge, "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM");
        assert!(!challenge.contains('+') && !challenge.contains('/') && !challenge.contains('='));
    }

    #[test]
    fn test_flow_and_session_seal_round_trip() {
        let sealer = CookieSealer::new("k");
        let flow = FlowState {
            state: "st".into(),
            nonce: "nc".into(),
            verifier: "vf".into(),
            original_uri: "/dashboard?tab=1".into(),
        };
        let sealed = sealer.seal(
            &serde_json::to_vec(&flow).unwrap(),
            Duration::from_secs(300),
        );
        let back: FlowState = serde_json::from_slice(&sealer.open(&sealed).unwrap()).unwrap();
        assert_eq!(back.state, "st");
        assert_eq!(back.original_uri, "/dashboard?tab=1");

        let session = SessionData {
            claims: serde_json::json!({ "sub": "u1", "name": "Alice" }),
            access_token: Some("at".into()),
        };
        let sealed = sealer.seal(
            &serde_json::to_vec(&session).unwrap(),
            Duration::from_secs(3600),
        );
        let back: SessionData = serde_json::from_slice(&sealer.open(&sealed).unwrap()).unwrap();
        assert_eq!(back.claims.get("sub").unwrap(), "u1");
        assert_eq!(back.access_token.as_deref(), Some("at"));
    }

    #[test]
    fn test_rejects_no_validation_source() {
        let c = cfg(&[("client_id", serde_json::json!("x"))]);
        assert!(OpenidConnectPlugin::from_config(&c, &PluginResources::empty()).is_err());
    }

    #[test]
    fn test_accepts_jwks_and_introspection_configs() {
        let jwks = cfg(&[("jwks_uri", serde_json::json!("https://idp/jwks"))]);
        assert!(OpenidConnectPlugin::from_config(&jwks, &PluginResources::empty()).is_ok());

        let introspect = cfg(&[
            (
                "introspection_endpoint",
                serde_json::json!("https://idp/introspect"),
            ),
            ("client_id", serde_json::json!("id")),
            ("client_secret", serde_json::json!("secret")),
        ]);
        assert!(OpenidConnectPlugin::from_config(&introspect, &PluginResources::empty()).is_ok());
    }

    #[test]
    fn test_rejects_unknown_alg() {
        let c = cfg(&[
            ("jwks_uri", serde_json::json!("https://idp/jwks")),
            (
                "token_signing_alg_values_expected",
                serde_json::json!("HS256"),
            ),
        ]);
        assert!(OpenidConnectPlugin::from_config(&c, &PluginResources::empty()).is_err());
    }

    #[test]
    fn test_select_jwk_by_kid() {
        let keys = vec![test_jwk("k1"), test_jwk("k2")];
        assert_eq!(
            select_jwk(&keys, Some("k2")).unwrap().kid.as_deref(),
            Some("k2")
        );
        assert!(select_jwk(&keys, Some("nope")).is_none());
        // No kid with multiple keys is ambiguous.
        assert!(select_jwk(&keys, None).is_none());
        // No kid with a single key resolves.
        let one = vec![test_jwk("only")];
        assert!(select_jwk(&one, None).is_some());
    }

    #[test]
    fn test_jwk_to_decoding_key_rsa() {
        let jwk = test_jwk("k1");
        assert!(jwk_to_decoding_key(&jwk).is_ok());
        // Missing components fail.
        let mut bad = test_jwk("k1");
        bad.n = None;
        assert!(jwk_to_decoding_key(&bad).is_err());
    }

    /// Regression: the *default* algorithm list spans RSA and EC, and
    /// jsonwebtoken rejects a Validation whose list contains any algorithm from a
    /// different family than the key. Verifying an RS256 token with the defaults
    /// therefore failed with `InvalidAlgorithm` — openid-connect did not work at
    /// all out of the box. Every pre-existing test passed a single-family list
    /// explicitly, which is exactly why none of them caught it.
    #[test]
    fn test_default_algs_verify_an_rs256_token() {
        let defaults = parse_allowed_algs(None).unwrap();
        assert!(
            defaults.len() > 1,
            "the default list must span families to be a regression test"
        );

        let keys = vec![test_jwk("k1")];
        let token = sign(
            serde_json::json!({ "sub": "user-1", "exp": 9999999999u64 }),
            "k1",
        );
        let jwk = select_jwk(&keys, Some("k1")).unwrap();
        let key = jwk_to_decoding_key(jwk).unwrap();

        // What the plugin now does: narrow the list to the key's family first.
        let algs = algs_for_key(&defaults, &jwk.kty).unwrap();
        let claims = decode_and_validate(&token, &key, &algs)
            .expect("an RS256 token must verify under the default algorithm list");
        assert_eq!(claims.get("sub").unwrap(), "user-1");

        // Passing the unfiltered default list is what used to fail.
        assert!(
            decode_and_validate(&token, &key, &defaults).is_err(),
            "sanity: the unfiltered mixed-family list is rejected by jsonwebtoken"
        );
    }

    #[test]
    fn test_algs_for_key_filters_by_family() {
        let defaults = parse_allowed_algs(None).unwrap();

        let rsa = algs_for_key(&defaults, "RSA").unwrap();
        assert!(rsa.contains(&Algorithm::RS256));
        assert!(!rsa.contains(&Algorithm::ES256));

        let ec = algs_for_key(&defaults, "EC").unwrap();
        assert!(ec.contains(&Algorithm::ES256));
        assert!(!ec.contains(&Algorithm::RS256));

        // A key type nothing configured can verify is an error, not a silent pass.
        assert!(algs_for_key(&[Algorithm::RS256], "EC").is_err());
        assert!(algs_for_key(&defaults, "oct").is_err());
    }

    #[test]
    fn test_verify_signed_token_end_to_end() {
        let keys = vec![test_jwk("k1")];
        let token = sign(
            serde_json::json!({ "sub": "user-1", "iss": "https://idp/", "aud": "my-api", "exp": 9999999999u64 }),
            "k1",
        );
        let jwk = select_jwk(&keys, Some("k1")).unwrap();
        let key = jwk_to_decoding_key(jwk).unwrap();
        let claims = decode_and_validate(&token, &key, &[Algorithm::RS256]).unwrap();
        assert_eq!(claims.get("sub").unwrap(), "user-1");

        // Tampered signature (wrong kid selects the wrong key would fail; here
        // an expired token fails exp validation).
        let expired = sign(serde_json::json!({ "sub": "u", "exp": 100u64 }), "k1");
        assert!(decode_and_validate(&expired, &key, &[Algorithm::RS256]).is_err());

        // Algorithm not in the allowed set is rejected.
        assert!(decode_and_validate(&token, &key, &[Algorithm::ES256]).is_err());
    }

    #[test]
    fn test_validate_claims_issuer_and_audience() {
        let c = cfg(&[
            ("jwks_uri", serde_json::json!("https://idp/jwks")),
            ("client_id", serde_json::json!("my-api")),
            (
                "claim_validator",
                serde_json::json!({
                    "issuer": { "valid_issuers": ["https://idp/"] },
                    "audience": { "required": true, "match_with_client_id": true }
                }),
            ),
        ]);
        let plugin = OpenidConnectPlugin::from_config(&c, &PluginResources::empty()).unwrap();

        let good: HashMap<String, serde_json::Value> = serde_json::from_value(serde_json::json!({
            "iss": "https://idp/", "aud": ["my-api", "other"], "sub": "u"
        }))
        .unwrap();
        assert!(plugin.validate_claims(&good).is_ok());

        // Wrong issuer.
        let bad_iss: HashMap<String, serde_json::Value> =
            serde_json::from_value(serde_json::json!({
                "iss": "https://evil/", "aud": "my-api"
            }))
            .unwrap();
        assert!(plugin.validate_claims(&bad_iss).is_err());

        // Audience does not include client_id.
        let bad_aud: HashMap<String, serde_json::Value> =
            serde_json::from_value(serde_json::json!({
                "iss": "https://idp/", "aud": "someone-else"
            }))
            .unwrap();
        assert!(plugin.validate_claims(&bad_aud).is_err());

        // Missing required audience.
        let no_aud: HashMap<String, serde_json::Value> =
            serde_json::from_value(serde_json::json!({
                "iss": "https://idp/"
            }))
            .unwrap();
        assert!(plugin.validate_claims(&no_aud).is_err());
    }

    #[test]
    fn test_parse_introspection() {
        let active =
            serde_json::to_vec(&serde_json::json!({ "active": true, "sub": "u1" })).unwrap();
        let claims = parse_introspection(&active).unwrap();
        assert_eq!(claims.get("sub").unwrap(), "u1");

        let inactive = serde_json::to_vec(&serde_json::json!({ "active": false })).unwrap();
        assert!(parse_introspection(&inactive).is_err());
    }

    #[test]
    fn test_parse_bearer() {
        assert_eq!(parse_bearer("Bearer abc.def"), Some("abc.def"));
        assert_eq!(parse_bearer("bearer xyz"), Some("xyz"));
        assert_eq!(parse_bearer("Basic abc"), None);
        assert_eq!(parse_bearer("Bearer "), None);
        assert_eq!(parse_bearer("token"), None);
    }
}
