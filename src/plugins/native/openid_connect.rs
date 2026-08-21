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
//! In interactive mode the node exits through the dedicated **`redirect`**
//! port whenever the browser must move (the 302 to the IdP, the post-callback
//! 302 back to the original URL, or a logout redirect) — wire `redirect` to
//! `client.in`. Deliberate rejections (missing/invalid flow cookie, CSRF
//! `state` mismatch, an invalid `id_token`, or a nonce mismatch) exit through
//! **`denied`** — wire it to `client.in` too, or a custom denial handler.
//! Genuine provider failures (discovery, JWKS, or token-endpoint callouts that
//! transport-fail, return a non-2xx status, or hand back unparseable data)
//! exit through the ordinary **`error`** port, since the node could not do its
//! job. Only a request that arrives with a valid session cookie continues out
//! **`success`** toward the upstream. The node must be on a route whose match
//! rule also covers the `redirect_uri` path so the callback reaches it.
//!
//! # Deviations from APISIX
//!
//! - **No server-side session revocation.** Sessions live entirely in the
//!   encrypted cookie, so a session cannot be invalidated before its
//!   `session.cookie.lifetime` expiry without a shared denylist (a future
//!   feature). Use short lifetimes. This is the standard client-side-cookie
//!   trade-off APISIX shares when configured for cookie sessions.
//! - **Token refresh, redis mode only.** When `session.storage: redis`, the
//!   callback captures the token response's `refresh_token`/`expires_in`
//!   alongside the session; a read that finds the access token within 30s of
//!   `expires_at` transparently refreshes it at the token endpoint before
//!   attaching identity, coordinated across concurrent requests via the
//!   store's short-lived lock (`SessionStore::try_lock`/`unlock`) so only one
//!   request per session performs the callout — losers re-read the
//!   (usually already-refreshed) session instead of also calling the IdP.
//!   An id_token in the refresh response is re-validated and its claims
//!   replace the session's; an IdP-side refresh failure (unreachable,
//!   non-2xx, invalid id_token) is not a store outage, so it falls back to
//!   a fresh login rather than a 503. Set `session.refresh: false` to
//!   disable (default `true`). Cookie-mode sessions have no server-side
//!   coordination point for this, so they keep the original behavior: when
//!   the session cookie expires the user re-authenticates (a fresh, fast
//!   redirect round-trip if the IdP session is still valid).
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
use std::ops::ControlFlow;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::Mutex;

use crate::context::{Context, GatewayError};
use crate::outbound::{OutboundRequest, OutboundResponse};
use crate::plugins::resources::PluginResources;
use crate::plugins::util::cookie_session::{
    build_set_cookie, delete_cookie, path_covers, read_cookie, CookieAttrs, CookieSealer, SameSite,
};
use crate::plugins::util::server_session::{self, SessionBackend};
use crate::plugins::{Plugin, PluginExecutionError, PluginOutput, PluginResult};
use crate::sessions::{SessionId, StoreError};

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
///
/// `refresh_token`/`expires_at` are populated ONLY in redis mode (see
/// [`OpenidConnectPlugin::handle_callback`]) — cookie-mode sessions have no
/// server-side coordination point for a lock-guarded refresh, so those
/// fields always stay `None` there and no refresh is ever attempted.
#[derive(Serialize, Deserialize)]
struct SessionData {
    claims: serde_json::Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    access_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    refresh_token: Option<String>,
    /// Access-token expiry, epoch seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    expires_at: Option<u64>,
}

/// Interactive-mode configuration, present only when `bearer_only: false`.
struct Interactive {
    sealer: CookieSealer,
    backend: SessionBackend,
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
    /// `session.refresh` (default `true`): redis-mode only — whether a
    /// near-expiry access token is transparently refreshed before identity
    /// is attached. Ignored in cookie mode, which never refreshes.
    refresh_enabled: bool,
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

/// Distinguishes a genuine provider/infrastructure failure (discovery, JWKS,
/// or introspection endpoint unreachable, non-2xx, or unparseable) from the
/// presented token being deliberately invalid (bad signature, unknown `kid`,
/// wrong issuer/audience, expired, or inactive). `Infra` exits through the
/// node's `error` port — the node could not do its job; `Denied` exits
/// through `denied` — the node did its job and the token was rejected.
#[derive(Debug)]
enum TokenError {
    Infra(String),
    Denied(String),
}

/// Outcome of a redis-mode refresh attempt ([`OpenidConnectPlugin::do_refresh`]).
/// `ReAuth` (IdP unreachable/errored, or the refreshed id_token failing
/// validation) is not a store outage — the caller falls back to re-login,
/// never a 503. `Store` is a genuine session-store failure and maps to
/// [`OpenidConnectPlugin::store_error`] (503) same as everywhere else.
#[derive(Debug)]
enum RefreshFailure {
    ReAuth(String),
    Store(StoreError),
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
            Some(build_interactive(config, &discovery, resources)?)
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
    /// Every failure here is a genuine provider/infra problem, not the
    /// caller's token being bad, so it always classifies as [`TokenError::Infra`].
    async fn jwks_uri(&self) -> Result<String, TokenError> {
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
            .ok_or_else(|| TokenError::Infra("no discovery URL configured".to_string()))?;
        let resp = self
            .get(discovery)
            .await
            .map_err(|e| TokenError::Infra(format!("discovery fetch failed: {}", e)))?;
        if resp.status != 200 {
            return Err(TokenError::Infra(format!(
                "discovery returned status {}",
                resp.status
            )));
        }
        let doc: serde_json::Value = serde_json::from_slice(&resp.body)
            .map_err(|e| TokenError::Infra(format!("failed to parse discovery doc: {}", e)))?;
        let uri = doc
            .get("jwks_uri")
            .and_then(|v| v.as_str())
            .ok_or_else(|| TokenError::Infra("discovery doc missing jwks_uri".to_string()))?
            .to_string();
        *self.jwks_uri_resolved.lock().await = Some(uri.clone());
        Ok(uri)
    }

    /// Returns the current JWKS, fetching/refreshing when stale or when
    /// `force` is set (used on an unknown `kid`). Every failure here is a
    /// genuine provider/infra problem, so it always classifies as
    /// [`TokenError::Infra`].
    async fn get_jwks(&self, force: bool) -> Result<Vec<Jwk>, TokenError> {
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
            .map_err(|e| TokenError::Infra(format!("JWKS fetch failed: {}", e)))?;
        if resp.status != 200 {
            return Err(TokenError::Infra(format!(
                "JWKS endpoint returned status {}",
                resp.status
            )));
        }
        let set: JwkSet = serde_json::from_slice(&resp.body)
            .map_err(|e| TokenError::Infra(format!("failed to parse JWKS: {}", e)))?;
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
            tls: None,
        };
        self.resources.outbound.request(req).await
    }

    /// Validates the token via JWKS signature verification plus claim checks.
    /// Provider/JWKS callout trouble classifies as [`TokenError::Infra`]; the
    /// token itself being malformed, unsigned by a known key, unverifiable, or
    /// failing an issuer/audience check classifies as [`TokenError::Denied`].
    async fn validate_via_jwks(
        &self,
        token: &str,
    ) -> Result<HashMap<String, serde_json::Value>, TokenError> {
        let header = decode_header(token)
            .map_err(|e| TokenError::Denied(format!("invalid JWT header: {}", e)))?;
        let kid = header.kid.clone();

        let keys = self.get_jwks(false).await?;
        let jwk = match select_jwk(&keys, kid.as_deref()) {
            Some(j) => j.clone(),
            None => {
                // Unknown kid: refetch once to pick up rotated keys.
                let keys = self.get_jwks(true).await?;
                select_jwk(&keys, kid.as_deref()).cloned().ok_or_else(|| {
                    TokenError::Denied("no matching JWK for token kid".to_string())
                })?
            }
        };

        // A malformed JWK or an allowed-algorithm list that cannot verify this
        // key's family is a provider/config problem, not the caller's fault.
        let key = jwk_to_decoding_key(&jwk).map_err(TokenError::Infra)?;
        // Narrow the permitted algorithms to the ones this key could possibly have
        // signed with. jsonwebtoken rejects the whole Validation if *any* listed
        // algorithm belongs to a different family than the key, so passing the
        // default list (RSA + EC) against an RSA key fails every token. See
        // `algs_for_key`.
        let algs = algs_for_key(&self.allowed_algs, &jwk.kty).map_err(TokenError::Infra)?;
        let claims = decode_and_validate(token, &key, &algs).map_err(TokenError::Denied)?;
        self.validate_claims(&claims).map_err(TokenError::Denied)?;
        Ok(claims)
    }

    /// Validates the token via the introspection endpoint. Provider callout
    /// trouble classifies as [`TokenError::Infra`]; the introspection response
    /// itself declaring the token inactive, or failing claim checks,
    /// classifies as [`TokenError::Denied`].
    async fn validate_via_introspection(
        &self,
        token: &str,
    ) -> Result<HashMap<String, serde_json::Value>, TokenError> {
        let endpoint = self
            .introspection_endpoint
            .as_ref()
            .ok_or_else(|| TokenError::Infra("no introspection endpoint configured".to_string()))?;
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
            tls: None,
        };
        let resp = self
            .resources
            .outbound
            .request(req)
            .await
            .map_err(|e| TokenError::Infra(format!("introspection callout failed: {}", e)))?;
        if resp.status != 200 {
            return Err(TokenError::Infra(format!(
                "introspection returned status {}",
                resp.status
            )));
        }
        let claims = parse_introspection(&resp.body)?;
        self.validate_claims(&claims).map_err(TokenError::Denied)?;
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

    /// Builds a 401 rejection for a deliberate authentication failure (missing
    /// bearer token, invalid/unverifiable token, CSRF/nonce/session-flow
    /// failure) and exits on the `denied` port.
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
        Ok(PluginOutput::on_port(ctx, "denied"))
    }

    /// Builds a genuine infrastructure-failure `Err` (discovery, JWKS,
    /// introspection, or token-endpoint callout that transport-failed,
    /// returned a non-2xx status, or handed back unparseable data). Unlike
    /// `reject`, this exits through the `error` port because the node could
    /// not do its job, not because a presented credential was deliberately
    /// refused. The response shape mirrors `reject`'s so client-visible
    /// behavior over this path is unchanged by the port split.
    fn infra_error(ctx: Context, message: String) -> PluginExecutionError {
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
        PluginExecutionError {
            context: ctx,
            error: GatewayError {
                node_id: String::new(),
                code: "OIDC_PROVIDER_ERROR".to_string(),
                message,
                metadata: HashMap::new(),
            },
        }
    }

    /// Session-store outage: 503 through the error port. Deliberately NOT
    /// 401 — bouncing users to an IdP whose callback also cannot persist a
    /// session is a redirect loop disguised as an outage.
    fn store_error(mut ctx: Context, e: StoreError) -> PluginExecutionError {
        ctx.response.status_code = 503;
        ctx.response.body = Bytes::from(r#"{"error": "session store unavailable"}"#.as_bytes());
        ctx.response.headers.insert(
            "content-type".to_string(),
            vec!["application/json".to_string()],
        );
        PluginExecutionError {
            context: ctx,
            error: GatewayError {
                node_id: String::new(),
                code: "SESSION_STORE_ERROR".to_string(),
                message: e.to_string(),
                metadata: HashMap::new(),
            },
        }
    }

    // ---- Interactive Authorization Code flow ------------------------------

    /// Drives the interactive flow: session check, callback handling, or a
    /// fresh redirect to the identity provider.
    async fn execute_interactive(&self, ctx: Context) -> PluginResult {
        let flow = self.interactive.as_ref().expect("interactive mode");

        // Logout: clear the session (server-side, in store mode) and redirect.
        if let Some(logout_path) = &flow.logout_path {
            if ctx.request.path == *logout_path {
                let cookie_value = ctx
                    .request
                    .headers
                    .get("cookie")
                    .and_then(|v| v.first())
                    .and_then(|h| read_cookie(h, &flow.session_cookie))
                    .map(str::to_string);
                let clear = match server_session::destroy(
                    &flow.backend,
                    cookie_value.as_deref(),
                    &flow.session_cookie,
                    &flow.cookie_path,
                )
                .await
                {
                    Ok(c) => c,
                    Err(e) => return Err(Self::store_error(ctx, e)),
                };
                return redirect(ctx, &flow.post_logout_redirect_uri, vec![clear]);
            }
        }

        // Callback: the IdP has redirected back with code + state.
        if ctx.request.path == flow.redirect_path && ctx.request.query_params.contains_key("code") {
            return self.handle_callback(ctx).await;
        }

        // Existing valid session cookie → attach identity and continue. Kept
        // alongside the session read (rather than only inside `read_session`)
        // because the redis-mode refresh check below needs the raw id to
        // take the store's lock.
        let raw_cookie_value = ctx
            .request
            .headers
            .get("cookie")
            .and_then(|v| v.first())
            .and_then(|h| read_cookie(h, &flow.session_cookie))
            .map(str::to_string);
        let session = match self.read_session(&ctx).await {
            Ok(s) => s,
            Err(e) => return Err(Self::store_error(ctx, e)),
        };
        if let Some(session) = session {
            let (mut ctx, session) = match raw_cookie_value.as_deref() {
                Some(raw) => match self.refresh_if_needed(ctx, flow, raw, session).await {
                    ControlFlow::Continue(pair) => pair,
                    ControlFlow::Break(result) => return result,
                },
                // A session was loaded from a cookie value that somehow
                // isn't readable here — refresh needs the raw id, but
                // authentication itself does not, so just proceed.
                None => (ctx, session),
            };
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
            return Ok(PluginOutput::success(ctx));
        }

        // No session → begin the Authorization Code flow.
        self.begin_auth(ctx).await
    }

    /// Reads the session cookie via the configured backend. `Ok(None)` =
    /// unauthenticated; `Err` = store outage (503 via `store_error`).
    async fn read_session(&self, ctx: &Context) -> Result<Option<SessionData>, StoreError> {
        let Some(flow) = self.interactive.as_ref() else {
            return Ok(None);
        };
        let Some(cookie_header) = ctx.request.headers.get("cookie").and_then(|v| v.first()) else {
            return Ok(None);
        };
        let Some(raw) = read_cookie(cookie_header, &flow.session_cookie) else {
            return Ok(None);
        };
        let bytes = server_session::load(&flow.backend, &flow.sealer, raw).await?;
        Ok(bytes.and_then(|b| serde_json::from_slice(&b).ok()))
    }

    /// Pre-attach refresh check (redis mode only, gated on `refresh_enabled`
    /// and the access token being within 30s of `expires_at`): coordinates a
    /// lock-guarded refresh so concurrent requests for the same session don't
    /// all hit the IdP.
    ///
    /// `ControlFlow::Continue` carries the (possibly refreshed) session data
    /// for the caller to attach as normal; `ControlFlow::Break` is an
    /// immediate exit the caller must return directly — either a 503 store
    /// error, or a fallback to re-login when the IdP-side refresh itself
    /// failed. Nothing outside this function and [`Self::do_refresh`] ever
    /// touches the lock: every branch below unlocks exactly once before
    /// breaking or continuing.
    async fn refresh_if_needed(
        &self,
        ctx: Context,
        flow: &Interactive,
        raw: &str,
        session: SessionData,
    ) -> ControlFlow<PluginResult, (Context, SessionData)> {
        if !flow.refresh_enabled || !matches!(flow.backend, SessionBackend::Store { .. }) {
            return ControlFlow::Continue((ctx, session));
        }
        let (Some(expires_at), Some(refresh_token)) =
            (session.expires_at, session.refresh_token.clone())
        else {
            return ControlFlow::Continue((ctx, session));
        };
        if now_unix() + 30 < expires_at {
            return ControlFlow::Continue((ctx, session));
        }
        let Some(id) = SessionId::parse(raw) else {
            // Shouldn't happen (the session was loaded through this same raw
            // cookie value), but a malformed id is not grounds to fail the
            // request — just proceed with what we already have.
            return ControlFlow::Continue((ctx, session));
        };
        let SessionBackend::Store { store, .. } = &flow.backend else {
            unreachable!("matches! above guarantees Store");
        };

        let acquired = match store.try_lock(&id, Duration::from_secs(10)).await {
            Ok(b) => b,
            Err(e) => return ControlFlow::Break(Err(Self::store_error(ctx, e))),
        };
        if !acquired {
            // Loser: the winner is usually already refreshing/refreshed —
            // re-read once and proceed with whatever is there now.
            return match self.read_session(&ctx).await {
                Ok(Some(fresh)) => ControlFlow::Continue((ctx, fresh)),
                // The session vanished under us (e.g. evicted) — treat like
                // no session at all rather than authenticating on stale data.
                Ok(None) => ControlFlow::Break(self.begin_auth(ctx).await),
                Err(e) => ControlFlow::Break(Err(Self::store_error(ctx, e))),
            };
        }

        // Winner: every branch below unlocks exactly once.
        match self
            .do_refresh(&ctx, flow, &id, &refresh_token, &session)
            .await
        {
            Ok(updated) => match store.unlock(&id).await {
                Ok(()) => ControlFlow::Continue((ctx, updated)),
                Err(e) => ControlFlow::Break(Err(Self::store_error(ctx, e))),
            },
            Err(RefreshFailure::ReAuth(reason)) => {
                // An IdP-side refresh failure (unreachable, non-2xx, or an
                // invalid refreshed id_token) is not a store outage — unlock
                // (best effort; the lock has a 10s TTL regardless) and fall
                // back to a fresh login rather than a 503.
                tracing::debug!(
                    "openid-connect: redis-mode refresh failed, falling back to re-login: {reason}"
                );
                let _ = store.unlock(&id).await;
                ControlFlow::Break(self.begin_auth(ctx).await)
            }
            Err(RefreshFailure::Store(e)) => {
                let _ = store.unlock(&id).await;
                ControlFlow::Break(Err(Self::store_error(ctx, e)))
            }
        }
    }

    /// Performs the refresh callout, optional id_token re-validation, and
    /// session rewrite. Never touches the lock — [`Self::refresh_if_needed`]
    /// unlocks on every outcome of this call.
    async fn do_refresh(
        &self,
        ctx: &Context,
        flow: &Interactive,
        id: &SessionId,
        refresh_token: &str,
        old: &SessionData,
    ) -> Result<SessionData, RefreshFailure> {
        let tokens = self
            .refresh_tokens(refresh_token)
            .await
            .map_err(RefreshFailure::ReAuth)?;

        let claims = match tokens.get("id_token").and_then(|v| v.as_str()) {
            Some(id_token) => match self.validate_via_jwks(id_token).await {
                Ok(c) => serde_json::to_value(&c).unwrap_or_default(),
                Err(TokenError::Infra(e)) | Err(TokenError::Denied(e)) => {
                    return Err(RefreshFailure::ReAuth(format!(
                        "refreshed id_token validation failed: {e}"
                    )))
                }
            },
            None => old.claims.clone(),
        };
        let access_token = tokens
            .get("access_token")
            .and_then(|v| v.as_str())
            .map(String::from)
            .or_else(|| old.access_token.clone());
        // Keep the old refresh_token unless the IdP rotated it.
        let refresh_token = tokens
            .get("refresh_token")
            .and_then(|v| v.as_str())
            .map(String::from)
            .or_else(|| old.refresh_token.clone());
        let expires_at = tokens
            .get("expires_in")
            .and_then(|v| v.as_u64())
            .map(|secs| now_unix() + secs)
            .or(old.expires_at);

        let updated = SessionData {
            claims,
            access_token,
            refresh_token,
            expires_at,
        };
        let payload = serde_json::to_vec(&updated)
            .map_err(|e| RefreshFailure::ReAuth(format!("session serialize failed: {e}")))?;
        let subject = updated
            .claims
            .get("sub")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        // Refresh must not extend the session's absolute lifetime, but the
        // store doesn't expose the record's remaining TTL from here, so we
        // approximate the remaining lifetime with the configured
        // `session.cookie.lifetime` — an acceptable, documented
        // over-approximation rather than a true "remaining" value.
        let ttl = flow.session_lifetime;
        let meta = server_session::meta_now(ctx, "openid-connect", &subject, ttl);
        server_session::update(
            &flow.backend,
            &flow.sealer,
            id.as_str(),
            &payload,
            ttl,
            meta,
            &flow.session_cookie,
            &CookieAttrs {
                path: &flow.cookie_path,
                max_age: Some(ttl.as_secs()),
                http_only: true,
                secure: request_is_https(ctx),
                same_site: SameSite::Lax,
            },
        )
        .await
        .map_err(RefreshFailure::Store)?;

        Ok(updated)
    }

    /// Refreshes an access token at the token endpoint (RFC 6749 §6). Cloned
    /// from [`Self::exchange_code`]'s request shape, with the refresh-token
    /// grant body instead.
    async fn refresh_tokens(&self, refresh_token: &str) -> Result<serde_json::Value, String> {
        let token_endpoint = self.token_endpoint().await?;
        let client_id = self.client_id.as_deref().unwrap_or("");
        let client_secret = self.client_secret.as_deref().unwrap_or("");
        let body = format!(
            "grant_type=refresh_token&refresh_token={}&client_id={}&client_secret={}",
            form_encode(refresh_token),
            form_encode(client_id),
            form_encode(client_secret),
        );
        let req = OutboundRequest {
            method: http::Method::POST,
            url: token_endpoint,
            headers: vec![
                (
                    "content-type".to_string(),
                    "application/x-www-form-urlencoded".to_string(),
                ),
                ("accept".to_string(), "application/json".to_string()),
            ],
            body: Bytes::from(body),
            timeout: self.timeout,
            ssl_verify: self.ssl_verify,
            tls: None,
        };
        let resp = self
            .resources
            .outbound
            .request(req)
            .await
            .map_err(|e| format!("token refresh callout failed: {}", e))?;
        if resp.status != 200 {
            return Err(format!("token endpoint returned status {}", resp.status));
        }
        serde_json::from_slice(&resp.body).map_err(|e| format!("invalid token response: {}", e))
    }

    /// Starts the flow: generate CSRF/nonce/PKCE, set the flow cookie, and
    /// redirect the browser to the IdP authorization endpoint.
    async fn begin_auth(&self, ctx: Context) -> PluginResult {
        let flow = self.interactive.as_ref().expect("interactive mode");
        let authz = match self.authorization_endpoint().await {
            Ok(u) => u,
            Err(e) => return Err(Self::infra_error(ctx, e)),
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
            Err(e) => {
                return Err(Self::infra_error(
                    ctx,
                    format!("flow cookie seal failed: {}", e),
                ))
            }
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

        // Exchange the authorization code for tokens. A token-endpoint
        // callout that transport-fails, returns non-2xx, or hands back
        // unparseable JSON is a genuine provider failure, not a deliberate
        // rejection of the caller.
        let token_endpoint = match self.token_endpoint().await {
            Ok(u) => u,
            Err(e) => return Err(Self::infra_error(ctx, e)),
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
            Err(e) => return Err(Self::infra_error(ctx, e)),
        };

        let id_token = match tokens.get("id_token").and_then(|v| v.as_str()) {
            Some(t) => t.to_string(),
            None => {
                return Err(Self::infra_error(
                    ctx,
                    "token response missing id_token".to_string(),
                ))
            }
        };
        let access_token = tokens
            .get("access_token")
            .and_then(|v| v.as_str())
            .map(String::from);
        // Refresh material is captured only in redis mode with refresh
        // enabled: cookie-mode sessions have no server-side coordination
        // point for a lock-guarded refresh, so they keep the pre-Task-7
        // re-login-on-expiry behavior untouched.
        let (refresh_token, expires_at) =
            if matches!(flow.backend, SessionBackend::Store { .. }) && flow.refresh_enabled {
                let refresh_token = tokens
                    .get("refresh_token")
                    .and_then(|v| v.as_str())
                    .map(String::from);
                let expires_at = tokens
                    .get("expires_in")
                    .and_then(|v| v.as_u64())
                    .map(|secs| now_unix() + secs);
                (refresh_token, expires_at)
            } else {
                (None, None)
            };

        // Validate the id_token signature/claims via the JWKS path and check
        // the nonce binds it to this login attempt. A JWKS callout failure is
        // a genuine provider failure; an invalid/unverifiable id_token is a
        // deliberate rejection.
        let claims = match self.validate_via_jwks(&id_token).await {
            Ok(c) => c,
            Err(TokenError::Infra(e)) => {
                return Err(Self::infra_error(
                    ctx,
                    format!("id_token validation failed: {}", e),
                ))
            }
            Err(TokenError::Denied(e)) => {
                return Self::reject(ctx, &format!("id_token validation failed: {}", e))
            }
        };
        if claims.get("nonce").and_then(|v| v.as_str()) != Some(flow_state.nonce.as_str()) {
            return Self::reject(ctx, "id_token nonce mismatch");
        }

        // Seal the session and redirect to where the user was going.
        let session = SessionData {
            claims: serde_json::to_value(&claims).unwrap_or_default(),
            access_token,
            refresh_token,
            expires_at,
        };
        let payload = match serde_json::to_vec(&session) {
            Ok(b) => b,
            Err(e) => {
                return Err(Self::infra_error(
                    ctx,
                    format!("session serialize failed: {}", e),
                ))
            }
        };
        let subject = claims
            .get("sub")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let meta =
            server_session::meta_now(&ctx, "openid-connect", &subject, flow.session_lifetime);
        let set_session = match server_session::establish(
            &flow.backend,
            &flow.sealer,
            &payload,
            flow.session_lifetime,
            meta,
            &flow.session_cookie,
            &CookieAttrs {
                path: &flow.cookie_path,
                max_age: Some(flow.session_lifetime.as_secs()),
                http_only: true,
                secure: request_is_https(&ctx),
                same_site: SameSite::Lax,
            },
        )
        .await
        {
            Ok(s) => s,
            Err(e) => return Err(Self::store_error(ctx, e)),
        };
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
            tls: None,
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
    resources: &Arc<PluginResources>,
) -> Result<Interactive, String> {
    let secret = session_field(config, "secret")
        .or_else(|| string_opt(config, "session_secret"))
        .ok_or("openid-connect: interactive login requires 'session.secret'")?;
    let backend = server_session::parse_backend(config, resources, "openid-connect")?;

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
        backend,
        authorization_endpoint_cfg,
        token_endpoint_cfg,
        redirect_uri,
        redirect_path,
        scope,
        flow_cookie: format!("{}_flow", session_cookie),
        session_cookie,
        cookie_path,
        session_lifetime,
        refresh_enabled: session_refresh_enabled(config),
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

/// Reads `session.refresh` (nested), falling back to the flat
/// `session_refresh` key the Web UI schema would emit; default `true`.
/// Redis-mode only — cookie mode never attempts a refresh regardless.
fn session_refresh_enabled(config: &HashMap<String, serde_json::Value>) -> bool {
    config
        .get("session")
        .and_then(|s| s.get("refresh"))
        .or_else(|| config.get("session_refresh"))
        .and_then(|v| v.as_bool())
        .unwrap_or(true)
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

/// Current time, epoch seconds. Used for `expires_at` bookkeeping on the
/// redis-mode refresh path.
fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Prepares a 302 redirect on the context and exits through the dedicated
/// `redirect` port (wire the node's `redirect` edge to `client.in`).
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
    Ok(PluginOutput::on_port(ctx, "redirect"))
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

/// Parses an RFC 7662 introspection response, requiring `active: true`. An
/// unparseable response is a genuine provider failure ([`TokenError::Infra`]);
/// an inactive token is a deliberate rejection ([`TokenError::Denied`]).
fn parse_introspection(body: &[u8]) -> Result<HashMap<String, serde_json::Value>, TokenError> {
    let value: serde_json::Value = serde_json::from_slice(body)
        .map_err(|e| TokenError::Infra(format!("invalid introspection response: {}", e)))?;
    let active = value
        .get("active")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    if !active {
        return Err(TokenError::Denied("token is not active".to_string()));
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

    async fn execute(&self, mut ctx: Context) -> PluginResult {
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
                Ok(PluginOutput::success(ctx))
            }
            Err(TokenError::Denied(e)) => Self::reject(ctx, &e),
            Err(TokenError::Infra(e)) => Err(Self::infra_error(ctx, e)),
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
            refresh_token: None,
            expires_at: None,
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

    // ---- Port-split coverage: denied / redirect (Ok) vs error (Err) --------

    fn req_ctx(path: &str, query: HashMap<String, Vec<String>>) -> crate::context::Context {
        crate::context::Context::new(crate::context::GatewayRequest {
            method: "GET".into(),
            path: path.into(),
            host: "app.example.com".into(),
            scheme: "https".into(),
            headers: HashMap::new(),
            query_params: query,
            body: Bytes::new(),
            remote_addr: "1.2.3.4:5".into(),
            protocol: crate::context::Protocol::Http1,
        })
    }

    fn with_bearer(mut ctx: crate::context::Context, token: &str) -> crate::context::Context {
        ctx.request
            .headers
            .insert("authorization".to_string(), vec![format!("Bearer {token}")]);
        ctx
    }

    #[tokio::test]
    async fn test_bearer_missing_token_denied() {
        let c = cfg(&[("jwks_uri", serde_json::json!("https://idp/jwks"))]);
        let plugin = OpenidConnectPlugin::from_config(&c, &PluginResources::empty()).unwrap();

        let out = plugin.execute(req_ctx("/", HashMap::new())).await.unwrap();
        assert_eq!(out.port, Some("denied"));
        assert_eq!(out.context.response.status_code, 401);
        assert!(out
            .context
            .response
            .headers
            .contains_key("www-authenticate"));
    }

    /// Regression: before the port split, every failure (deliberate or
    /// infra) flowed through the same `Err`. A JWKS endpoint that is
    /// unreachable (nothing listening) is a genuine provider failure and
    /// must stay on `Err`, not be folded into `denied`.
    #[tokio::test]
    async fn test_bearer_jwks_unreachable_stays_on_error_port() {
        let c = cfg(&[
            ("jwks_uri", serde_json::json!("http://127.0.0.1:1/jwks")),
            ("timeout", serde_json::json!(1)),
        ]);
        let plugin = OpenidConnectPlugin::from_config(&c, &PluginResources::empty()).unwrap();

        // A structurally valid JWT so `decode_header` succeeds and the
        // failure comes from the (unreachable) JWKS callout, not header parsing.
        let token = sign(serde_json::json!({ "sub": "user-1" }), "k1");
        let err = plugin
            .execute(with_bearer(req_ctx("/", HashMap::new()), &token))
            .await
            .unwrap_err();
        assert_eq!(err.error.code, "OIDC_PROVIDER_ERROR");
        assert_eq!(err.context.response.status_code, 401);
    }

    /// Same regression, via the discovery path: an unreachable discovery
    /// document is also a genuine provider failure.
    #[tokio::test]
    async fn test_bearer_discovery_unreachable_stays_on_error_port() {
        let c = cfg(&[
            (
                "discovery",
                serde_json::json!("http://127.0.0.1:1/.well-known/openid-configuration"),
            ),
            ("timeout", serde_json::json!(1)),
        ]);
        let plugin = OpenidConnectPlugin::from_config(&c, &PluginResources::empty()).unwrap();

        let token = sign(serde_json::json!({ "sub": "user-1" }), "k1");
        let err = plugin
            .execute(with_bearer(req_ctx("/", HashMap::new()), &token))
            .await
            .unwrap_err();
        assert_eq!(err.error.code, "OIDC_PROVIDER_ERROR");
        assert_eq!(err.context.response.status_code, 401);
    }

    /// Same regression, via introspection: an unreachable introspection
    /// endpoint is a genuine provider failure, not a token denial.
    #[tokio::test]
    async fn test_bearer_introspection_unreachable_stays_on_error_port() {
        let c = cfg(&[
            (
                "introspection_endpoint",
                serde_json::json!("http://127.0.0.1:1/introspect"),
            ),
            ("client_id", serde_json::json!("id")),
            ("client_secret", serde_json::json!("secret")),
            ("timeout", serde_json::json!(1)),
        ]);
        let plugin = OpenidConnectPlugin::from_config(&c, &PluginResources::empty()).unwrap();

        let err = plugin
            .execute(with_bearer(req_ctx("/", HashMap::new()), "opaque-token"))
            .await
            .unwrap_err();
        assert_eq!(err.error.code, "OIDC_PROVIDER_ERROR");
        assert_eq!(err.context.response.status_code, 401);
    }

    /// An introspection response that reports the token inactive is a
    /// deliberate rejection and must stay `denied`, distinct from the
    /// callout-unreachable case above.
    #[test]
    fn test_parse_introspection_inactive_is_denied_not_infra() {
        let inactive = serde_json::to_vec(&serde_json::json!({ "active": false })).unwrap();
        match parse_introspection(&inactive) {
            Err(TokenError::Denied(_)) => {}
            other => panic!("expected Denied, got {other:?}"),
        }
        let bad_json = b"not json";
        match parse_introspection(bad_json) {
            Err(TokenError::Infra(_)) => {}
            other => panic!("expected Infra, got {other:?}"),
        }
    }

    fn interactive_explicit_cfg() -> HashMap<String, serde_json::Value> {
        cfg(&[
            (
                "authorization_endpoint",
                serde_json::json!("https://idp.example.com/authorize"),
            ),
            (
                "token_endpoint",
                serde_json::json!("http://127.0.0.1:1/token"),
            ),
            (
                "jwks_uri",
                serde_json::json!("https://idp.example.com/jwks"),
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
        ])
    }

    #[tokio::test]
    async fn test_interactive_begin_login_redirects() {
        let plugin = OpenidConnectPlugin::from_config(
            &interactive_explicit_cfg(),
            &PluginResources::empty(),
        )
        .unwrap();

        let out = plugin
            .execute(req_ctx("/dashboard", HashMap::new()))
            .await
            .unwrap();
        assert_eq!(out.port, Some("redirect"));
        assert_eq!(out.context.response.status_code, 302);
        let location = &out.context.response.headers.get("location").unwrap()[0];
        assert!(
            location.starts_with("https://idp.example.com/authorize?"),
            "{location}"
        );
        let set = &out.context.response.headers.get("set-cookie").unwrap()[0];
        assert!(set.starts_with("oidc_session_flow="), "{set}");
    }

    #[tokio::test]
    async fn test_interactive_logout_redirects() {
        let mut c = interactive_explicit_cfg();
        c.insert("logout_path".to_string(), serde_json::json!("/logout"));
        let plugin = OpenidConnectPlugin::from_config(&c, &PluginResources::empty()).unwrap();

        let out = plugin
            .execute(req_ctx("/logout", HashMap::new()))
            .await
            .unwrap();
        assert_eq!(out.port, Some("redirect"));
        assert_eq!(out.context.response.status_code, 302);
        assert_eq!(
            out.context.response.headers.get("location").unwrap()[0],
            "/"
        );
    }

    #[tokio::test]
    async fn test_interactive_callback_missing_flow_cookie_denied() {
        let plugin = OpenidConnectPlugin::from_config(
            &interactive_explicit_cfg(),
            &PluginResources::empty(),
        )
        .unwrap();

        let mut query = HashMap::new();
        query.insert("code".to_string(), vec!["c".to_string()]);
        query.insert("state".to_string(), vec!["st".to_string()]);
        let out = plugin
            .execute(req_ctx("/oidc/callback", query))
            .await
            .unwrap();
        assert_eq!(out.port, Some("denied"));
        assert_eq!(out.context.response.status_code, 401);
    }

    #[tokio::test]
    async fn test_interactive_callback_state_mismatch_denied() {
        let plugin = OpenidConnectPlugin::from_config(
            &interactive_explicit_cfg(),
            &PluginResources::empty(),
        )
        .unwrap();
        let flow = FlowState {
            state: "expected".into(),
            nonce: "nonce".into(),
            verifier: "verifier".into(),
            original_uri: "/dashboard".into(),
        };
        let sealer = CookieSealer::new("cookie-signing-secret");
        let sealed = sealer.seal(
            &serde_json::to_vec(&flow).unwrap(),
            Duration::from_secs(300),
        );

        let mut query = HashMap::new();
        query.insert("code".to_string(), vec!["c".to_string()]);
        query.insert("state".to_string(), vec!["WRONG".to_string()]);
        let mut c = req_ctx("/oidc/callback", query);
        c.request.headers.insert(
            "cookie".to_string(),
            vec![format!("oidc_session_flow={sealed}")],
        );

        let out = plugin.execute(c).await.unwrap();
        assert_eq!(out.port, Some("denied"));
        assert_eq!(out.context.response.status_code, 401);
    }

    /// The callback's code-exchange call to the token endpoint is a genuine
    /// provider callout; when it is unreachable that must stay `Err`, not be
    /// folded into `denied` alongside the CSRF/state checks above.
    #[tokio::test]
    async fn test_interactive_callback_token_endpoint_unreachable_stays_on_error_port() {
        let plugin = OpenidConnectPlugin::from_config(
            &interactive_explicit_cfg(),
            &PluginResources::empty(),
        )
        .unwrap();
        let flow = FlowState {
            state: "matching".into(),
            nonce: "nonce".into(),
            verifier: "verifier".into(),
            original_uri: "/dashboard".into(),
        };
        let sealer = CookieSealer::new("cookie-signing-secret");
        let sealed = sealer.seal(
            &serde_json::to_vec(&flow).unwrap(),
            Duration::from_secs(300),
        );

        let mut query = HashMap::new();
        query.insert("code".to_string(), vec!["c".to_string()]);
        query.insert("state".to_string(), vec!["matching".to_string()]);
        let mut c = req_ctx("/oidc/callback", query);
        c.request.headers.insert(
            "cookie".to_string(),
            vec![format!("oidc_session_flow={sealed}")],
        );

        let err = plugin.execute(c).await.unwrap_err();
        assert_eq!(err.error.code, "OIDC_PROVIDER_ERROR");
        assert_eq!(err.context.response.status_code, 401);
    }

    // ---- Redis session storage ---------------------------------------

    #[cfg(feature = "redis-store")]
    fn resources_with_fake_store() -> (
        std::sync::Arc<crate::plugins::resources::PluginResources>,
        std::sync::Arc<crate::sessions::FakeSessionStore>,
    ) {
        let fake = std::sync::Arc::new(crate::sessions::FakeSessionStore::default());
        let resources = crate::plugins::resources::PluginResources::empty();
        resources.stores.store(std::sync::Arc::new(
            crate::stores::StoreRegistry::with_fake_session_store("s1", fake.clone()),
        ));
        (resources, fake)
    }

    /// redis storage requires a store name; unknown stores fail at config.
    #[test]
    fn test_session_storage_redis_requires_store() {
        let mut cfg = interactive_explicit_cfg();
        cfg.insert(
            "session".to_string(),
            serde_json::json!({"secret": "cookie-signing-secret", "storage": "redis"}),
        );
        // `.err().unwrap()` (not `unwrap_err()`): the Ok type isn't `Debug`.
        let err = OpenidConnectPlugin::from_config(&cfg, &PluginResources::empty())
            .err()
            .unwrap();
        assert!(err.contains("requires 'session.store'"), "{err}");
    }

    /// In redis mode a valid id-cookie authenticates from the store, and a
    /// store outage is a 503 on the error port — never a silent re-login.
    #[cfg(feature = "redis-store")]
    #[tokio::test]
    async fn test_redis_session_read_and_store_outage_503() {
        use crate::sessions::SessionStore as _;

        let (resources, fake) = resources_with_fake_store();
        let mut cfg = interactive_explicit_cfg();
        cfg.insert(
            "session".to_string(),
            serde_json::json!({
                "secret": "cookie-signing-secret",
                "storage": "redis",
                "store": "s1"
            }),
        );
        let plugin = OpenidConnectPlugin::from_config(&cfg, &resources).unwrap();

        // Establish a session by hand: seal SessionData, put under an id.
        let sealer = CookieSealer::new("cookie-signing-secret");
        let data = serde_json::json!({"claims": {"sub": "u1"}});
        let sealed = sealer.seal(&serde_json::to_vec(&data).unwrap(), Duration::from_secs(60));
        let id = crate::sessions::SessionId::random();
        let meta = crate::sessions::SessionMeta {
            id: String::new(),
            subject: "u1".to_string(),
            plugin: "openid-connect".to_string(),
            policy: String::new(),
            route: String::new(),
            created_at: 0,
            expires_at: 0,
        };
        fake.put(&id, sealed.as_bytes(), Duration::from_secs(60), &meta)
            .await
            .unwrap();

        let mut ctx = req_ctx("/api", HashMap::new());
        ctx.request.headers.insert(
            "cookie".to_string(),
            vec![format!("oidc_session={}", id.as_str())],
        );
        let out = plugin.execute(ctx).await.unwrap();
        assert!(out.port.is_none(), "valid store session must pass");
        assert_eq!(out.context.message["user_id"], "u1");

        // Outage: same request, failing store.
        fake.fail.store(true, std::sync::atomic::Ordering::Relaxed);
        let mut ctx = req_ctx("/api", HashMap::new());
        ctx.request.headers.insert(
            "cookie".to_string(),
            vec![format!("oidc_session={}", id.as_str())],
        );
        let err = plugin.execute(ctx).await.unwrap_err();
        assert_eq!(err.error.code, "SESSION_STORE_ERROR");
        assert_eq!(err.context.response.status_code, 503);
    }

    /// Redis-mode refresh: expired access token + refresh_token triggers a
    /// locked refresh; the session is rewritten under the same id. The IdP
    /// being unreachable falls back to re-login (redirect), not 503.
    #[cfg(feature = "redis-store")]
    #[tokio::test]
    async fn test_redis_refresh_lock_and_fallback() {
        use crate::sessions::SessionStore as _;

        let (resources, fake) = resources_with_fake_store();
        let mut cfg = interactive_explicit_cfg(); // token_endpoint: http://127.0.0.1:1 (unreachable)
        cfg.insert(
            "session".to_string(),
            serde_json::json!({
                "secret": "cookie-signing-secret",
                "storage": "redis",
                "store": "s1"
            }),
        );
        let plugin = OpenidConnectPlugin::from_config(&cfg, &resources).unwrap();

        let sealer = CookieSealer::new("cookie-signing-secret");
        // Stale access token (expires_at in the past) + a refresh token.
        let data = serde_json::json!({
            "claims": {"sub": "u1"},
            "access_token": "old",
            "refresh_token": "rt",
            "expires_at": 1
        });
        let sealed = sealer.seal(
            &serde_json::to_vec(&data).unwrap(),
            Duration::from_secs(600),
        );
        let id = crate::sessions::SessionId::random();
        let meta = crate::sessions::SessionMeta {
            id: String::new(),
            subject: "u1".into(),
            plugin: "openid-connect".into(),
            policy: String::new(),
            route: String::new(),
            created_at: 0,
            expires_at: 0,
        };
        fake.put(&id, sealed.as_bytes(), Duration::from_secs(600), &meta)
            .await
            .unwrap();

        let mut ctx = req_ctx("/api", HashMap::new());
        ctx.request.headers.insert(
            "cookie".to_string(),
            vec![format!("oidc_session={}", id.as_str())],
        );
        // Token endpoint unreachable → refresh fails → fall back to re-login.
        let out = plugin.execute(ctx).await.unwrap();
        assert_eq!(out.port, Some("redirect"), "failed refresh re-enters login");
        // The lock was released (unlock on the failure path).
        assert!(fake.try_lock(&id, Duration::from_secs(1)).await.unwrap());
    }

    /// `session.refresh: false` disables the refresh check entirely: a stale
    /// access token + refresh_token pass through unchanged (no redirect, no
    /// lock taken) — the plugin behaves exactly as it did before Task 7.
    #[cfg(feature = "redis-store")]
    #[tokio::test]
    async fn test_redis_refresh_disabled_passes_stale_session_through() {
        use crate::sessions::SessionStore as _;

        let (resources, fake) = resources_with_fake_store();
        let mut cfg = interactive_explicit_cfg();
        cfg.insert(
            "session".to_string(),
            serde_json::json!({
                "secret": "cookie-signing-secret",
                "storage": "redis",
                "store": "s1",
                "refresh": false
            }),
        );
        let plugin = OpenidConnectPlugin::from_config(&cfg, &resources).unwrap();

        let sealer = CookieSealer::new("cookie-signing-secret");
        let data = serde_json::json!({
            "claims": {"sub": "u1"},
            "access_token": "old",
            "refresh_token": "rt",
            "expires_at": 1
        });
        let sealed = sealer.seal(
            &serde_json::to_vec(&data).unwrap(),
            Duration::from_secs(600),
        );
        let id = crate::sessions::SessionId::random();
        let meta = crate::sessions::SessionMeta {
            id: String::new(),
            subject: "u1".into(),
            plugin: "openid-connect".into(),
            policy: String::new(),
            route: String::new(),
            created_at: 0,
            expires_at: 0,
        };
        fake.put(&id, sealed.as_bytes(), Duration::from_secs(600), &meta)
            .await
            .unwrap();

        let mut ctx = req_ctx("/api", HashMap::new());
        ctx.request.headers.insert(
            "cookie".to_string(),
            vec![format!("oidc_session={}", id.as_str())],
        );
        let out = plugin.execute(ctx).await.unwrap();
        assert!(
            out.port.is_none(),
            "disabled refresh must pass through as-is"
        );
        assert_eq!(out.context.message["user_id"], "u1");
        // No lock was ever taken — try_lock succeeds trivially.
        assert!(fake.try_lock(&id, Duration::from_secs(1)).await.unwrap());
    }
}
