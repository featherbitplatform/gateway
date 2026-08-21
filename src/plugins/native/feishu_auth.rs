//! Feishu / Lark authentication plugin (`feishu-auth`).
//!
//! Validates a Feishu authorization *code* by exchanging it, through Feishu's
//! OAuth v2 token endpoint, for a user access token, then calls Feishu's
//! userinfo endpoint to resolve the calling user's identity and attaches it to
//! the request. A missing code, or a code/token Feishu actively rejects, is
//! denied with a `401` on the `denied` port; a Feishu callout that fails
//! outright (network error, non-200, unparseable body) is a genuine
//! infrastructure failure and stays on the `error` port.
//!
//! # Ported subset / deviations from APISIX
//!
//! APISIX's `feishu-auth` is a *session* plugin: on the first request it
//! reads a code, exchanges it for a user access token and userinfo, then
//! stores both in an encrypted `feishu_session` cookie so later requests skip
//! the callouts, and it 302-redirects to `redirect_uri` when no code and no
//! session are present. That session machinery is now restored on an opt-in
//! basis, sharing the same [cookie](crate::plugins::util::cookie_session) /
//! [server-store](crate::plugins::util::server_session) primitives as
//! `cas-auth`/`openid-connect`/`authz-casdoor`/`dingtalk-auth`:
//!
//! - **Stateless (default)** — when no `session.secret` is configured the
//!   node behaves exactly as before: every request must carry a code, which
//!   is exchanged and validated against Feishu on each request. No cookie is
//!   read or set.
//! - **Session (opt-in)** — set `session.secret` (or `session_secret`) to
//!   turn the flow back on: strip any client-supplied `x-userinfo`, then read
//!   the `feishu_session` cookie (cookie mode: the session payload is sealed
//!   directly in the cookie; redis mode via `session.storage: redis` +
//!   `session.store: <name>`: the cookie carries a bare id, the sealed
//!   payload lives server-side). A valid session attaches identity straight
//!   from the stored userinfo — no Feishu callout. An undecodable payload
//!   (corrupt/stale format) is destroyed and treated as no session rather
//!   than failing the request. No session and no code 302-redirects to the
//!   required `redirect_uri` (the `redirect` port, distinct from the
//!   always-required `auth_redirect_uri` token-exchange field below). A code
//!   present runs the existing token+userinfo callouts unchanged, then
//!   establishes a new session (payload = userinfo JSON plus the exchanged
//!   access token and its expiry; subject = `user_id` else `open_id` else
//!   `union_id` else empty; ttl = `session.cookie.lifetime`, APISIX's
//!   `cookie_expires_in`, default `86400`) and attaches identity with the
//!   `Set-Cookie` on the success response. A session-store outage is a `503`
//!   (`SESSION_STORE_ERROR`) on the `error` port, never a silent re-login.
//!
//! Remaining deviation: `secret_fallbacks` (APISIX's multi-secret key
//! rotation) is **not** supported — the sealer is a single `session.secret`,
//! same as every other session plugin in this codebase. `auth_redirect_uri`
//! is retained (and still always required) because it is part of the
//! `authorization_code` token-exchange body, not the interactive redirect.

use async_trait::async_trait;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use base64::Engine;
use bytes::Bytes;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::context::{Context, GatewayError};
use crate::outbound::{OutboundRequest, OutboundResponse};
use crate::plugins::resources::PluginResources;
use crate::plugins::util::cookie_session::{read_cookie, CookieAttrs, CookieSealer, SameSite};
use crate::plugins::util::server_session::{self, SessionBackend};
use crate::plugins::{Plugin, PluginExecutionError, PluginOutput, PluginResult};
use crate::sessions::StoreError;

const DEFAULT_TOKEN_URL: &str = "https://open.feishu.cn/open-apis/authen/v2/oauth/token";
const DEFAULT_USERINFO_URL: &str = "https://open.feishu.cn/open-apis/authen/v1/user_info";

/// Outcome of resolving a Feishu code. `Unauthorized` is a deliberate denial
/// (exits `denied`, `401`); `Upstream` is a genuine callout failure (exits
/// `error`).
#[derive(Debug)]
enum FeishuError {
    Unauthorized(String),
    Upstream(String),
}

impl FeishuError {
    fn message(&self) -> &str {
        match self {
            FeishuError::Unauthorized(m) | FeishuError::Upstream(m) => m,
        }
    }
}

/// Session payload: the resolved userinfo plus the exchanged access token
/// and its expiry, cached per APISIX's original so a future re-exchange
/// (after a userinfo failure) can reuse a still-valid token.
#[derive(serde::Serialize, serde::Deserialize)]
struct FeishuSessionData {
    userinfo: serde_json::Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    access_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    access_token_expires_at: Option<u64>,
}

/// Session-mode settings (present when `session.secret` is configured).
struct FeishuSession {
    sealer: CookieSealer,
    backend: SessionBackend,
    cookie_name: String,
    cookie_path: String,
    cookie_lifetime: u64,
    redirect_uri: String,
}

/// Authenticates requests by exchanging a Feishu authorization code for a user
/// access token, then resolving that token to a Feishu user.
pub struct FeishuAuthPlugin {
    app_id: String,
    app_secret: String,
    auth_redirect_uri: String,
    code_header: String,
    code_query: String,
    token_url: String,
    userinfo_url: String,
    set_userinfo_header: bool,
    timeout: Duration,
    ssl_verify: bool,
    resources: Arc<PluginResources>,
    /// Session-mode settings; `None` keeps the stateless token-validation
    /// behavior (the pre-existing, backward-compatible default).
    session: Option<FeishuSession>,
}

impl FeishuAuthPlugin {
    /// Builds the plugin from node config.
    ///
    /// Accepted keys:
    /// - `app_id` (string, required): Feishu application id.
    /// - `app_secret` (string, required): Feishu application secret.
    /// - `auth_redirect_uri` (string, required): the `redirect_uri` registered
    ///   with Feishu; sent in the `authorization_code` token-exchange body and
    ///   must match the one used to obtain the code.
    /// - `code_header` (string, default `"X-Feishu-Code"`): header the code is
    ///   read from first (matched case-insensitively).
    /// - `code_query` (string, default `"code"`): query parameter fallback.
    /// - `access_token_url` (string, default Feishu's `oauth/token`).
    /// - `userinfo_url` (string, default Feishu's `authen/v1/user_info`).
    /// - `set_userinfo_header` (bool, default `true`): base64-encode the
    ///   resolved userinfo into the `X-Userinfo` request header.
    /// - `timeout` (integer ms, default `6000`).
    /// - `ssl_verify` (bool, default `true`).
    ///
    /// Session-mode keys (present ⇒ session mode is enabled — see the module
    /// docs):
    /// - `session_secret` (string) or `session.secret` (string): signing/
    ///   encryption secret for the session cookie. Setting it turns on the
    ///   session flow.
    /// - `session.cookie.name` (string, default `"feishu_session"`).
    /// - `session.cookie.path` (string, default `"/"`).
    /// - `session.cookie.lifetime` (u64 seconds, default `86400`; APISIX's
    ///   `cookie_expires_in`).
    /// - `session.storage` / `session.store`: server-side session backend
    ///   (see [`server_session::parse_backend`]).
    /// - `redirect_uri` (string, **required in session mode**): where to
    ///   302 a browser that has neither a valid session nor a code. Distinct
    ///   from `auth_redirect_uri`, which stays required always.
    ///
    /// `secret_fallbacks` (APISIX multi-secret rotation) is not accepted —
    /// see the module docs.
    ///
    /// ```yaml
    /// type: feishu-auth
    /// config:
    ///   app_id: ${FEISHU_APP_ID}
    ///   app_secret: ${FEISHU_APP_SECRET}
    ///   auth_redirect_uri: https://app.example.com/callback
    ///   session:
    ///     secret: ${FEISHU_SESSION_SECRET}
    ///   redirect_uri: https://login.example.com/start
    /// ```
    pub fn from_config(
        config: &HashMap<String, serde_json::Value>,
        resources: &Arc<PluginResources>,
    ) -> Result<Self, String> {
        let app_id = require_string(config, "app_id")?;
        let app_secret = require_string(config, "app_secret")?;
        let auth_redirect_uri = require_string(config, "auth_redirect_uri")?;

        let code_header = config
            .get("code_header")
            .and_then(|v| v.as_str())
            .unwrap_or("X-Feishu-Code")
            .to_lowercase();
        let code_query = config
            .get("code_query")
            .and_then(|v| v.as_str())
            .unwrap_or("code")
            .to_string();
        let token_url = config
            .get("access_token_url")
            .and_then(|v| v.as_str())
            .unwrap_or(DEFAULT_TOKEN_URL)
            .to_string();
        let userinfo_url = config
            .get("userinfo_url")
            .and_then(|v| v.as_str())
            .unwrap_or(DEFAULT_USERINFO_URL)
            .to_string();
        let set_userinfo_header = config
            .get("set_userinfo_header")
            .and_then(|v| v.as_bool())
            .unwrap_or(true);
        let timeout = Duration::from_millis(
            config
                .get("timeout")
                .and_then(|v| v.as_u64())
                .unwrap_or(6000),
        );
        let ssl_verify = config
            .get("ssl_verify")
            .and_then(|v| v.as_bool())
            .unwrap_or(true);

        let session = match session_secret(config) {
            Some(secret) => {
                let redirect_uri = require_string(config, "redirect_uri")?;
                let sealer = CookieSealer::new(&secret);
                let cookie_name = session_cookie_str(config, "name")
                    .unwrap_or_else(|| "feishu_session".to_string());
                let cookie_path =
                    session_cookie_str(config, "path").unwrap_or_else(|| "/".to_string());
                let cookie_lifetime = session_cookie_u64(config, "lifetime").unwrap_or(86_400);
                let backend = server_session::parse_backend(config, resources, "feishu-auth")?;
                Some(FeishuSession {
                    sealer,
                    backend,
                    cookie_name,
                    cookie_path,
                    cookie_lifetime,
                    redirect_uri,
                })
            }
            None => None,
        };

        Ok(Self {
            app_id,
            app_secret,
            auth_redirect_uri,
            code_header,
            code_query,
            token_url,
            userinfo_url,
            set_userinfo_header,
            timeout,
            ssl_verify,
            resources: resources.clone(),
            session,
        })
    }

    fn extract_code(&self, ctx: &Context) -> Option<String> {
        if let Some(v) = ctx
            .request
            .headers
            .get(&self.code_header)
            .and_then(|v| v.first())
        {
            if !v.is_empty() {
                return Some(v.clone());
            }
        }
        ctx.request
            .query_params
            .get(&self.code_query)
            .and_then(|v| v.first())
            .filter(|v| !v.is_empty())
            .cloned()
    }

    /// Exchanges `code` for a Feishu user access token, returning the token
    /// and (when present) its `expires_in` seconds.
    async fn fetch_access_token(&self, code: &str) -> Result<(String, Option<u64>), FeishuError> {
        let body = self.token_request_body(code);
        let req = OutboundRequest {
            method: http::Method::POST,
            url: self.token_url.clone(),
            headers: vec![("content-type".to_string(), "application/json".to_string())],
            body: Bytes::from(serde_json::to_vec(&body).unwrap_or_default()),
            timeout: self.timeout,
            ssl_verify: self.ssl_verify,
            tls: None,
        };
        let resp = self
            .resources
            .outbound
            .request(req)
            .await
            .map_err(|e| FeishuError::Upstream(format!("token callout failed: {}", e)))?;
        parse_access_token(&resp)
    }

    /// Builds the `authorization_code` token-exchange body.
    fn token_request_body(&self, code: &str) -> serde_json::Value {
        serde_json::json!({
            "grant_type": "authorization_code",
            "client_id": self.app_id,
            "client_secret": self.app_secret,
            "redirect_uri": self.auth_redirect_uri,
            "code": code,
        })
    }

    /// Resolves the access token to Feishu userinfo.
    async fn fetch_userinfo(&self, access_token: &str) -> Result<serde_json::Value, FeishuError> {
        let req = OutboundRequest {
            method: http::Method::GET,
            url: self.userinfo_url.clone(),
            headers: vec![
                ("content-type".to_string(), "application/json".to_string()),
                (
                    "authorization".to_string(),
                    format!("Bearer {}", access_token),
                ),
            ],
            body: Bytes::new(),
            timeout: self.timeout,
            ssl_verify: self.ssl_verify,
            tls: None,
        };
        let resp = self
            .resources
            .outbound
            .request(req)
            .await
            .map_err(|e| FeishuError::Upstream(format!("userinfo callout failed: {}", e)))?;
        parse_userinfo(&resp)
    }

    /// Builds the `401` rejection and exits on the `denied` port. Reserved
    /// for a deliberate denial — a missing code, or Feishu actively
    /// rejecting the code/token.
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
        Ok(PluginOutput::on_port(ctx, "denied"))
    }

    /// Builds a genuine infrastructure-failure `Err` for a Feishu callout
    /// that failed outright (network error, non-200, unparseable body) —
    /// unlike `reject`, the node could not do its job rather than Feishu
    /// deliberately refusing the code.
    fn upstream_error(ctx: Context, message: &str) -> PluginResult {
        let mut ctx = ctx;
        ctx.response.status_code = 502;
        Err(PluginExecutionError {
            context: ctx,
            error: GatewayError {
                node_id: String::new(),
                code: "FEISHU_UPSTREAM_ERROR".to_string(),
                message: message.to_string(),
                metadata: HashMap::new(),
            },
        })
    }

    /// Session-store outage: 503 through the error port. Deliberately NOT
    /// 401 — a store outage is not "unauthenticated".
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

    /// Builds a `302` early-exit carrying the prepared response, and exits on
    /// the `redirect` port. Wire the node's `redirect` edge to `client.in` so
    /// this reaches the browser.
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

    /// Cookie attributes for the session cookie: `HttpOnly`, `SameSite=Lax`,
    /// and `Secure` only over HTTPS (so plain-HTTP dev works).
    fn session_attrs<'a>(session: &'a FeishuSession, ctx: &Context) -> CookieAttrs<'a> {
        CookieAttrs {
            path: &session.cookie_path,
            max_age: Some(session.cookie_lifetime),
            http_only: true,
            secure: ctx.request.scheme == "https",
            same_site: SameSite::Lax,
        }
    }

    /// Reads the session cookie via the configured backend.
    ///
    /// `Ok(Some(data))` = a valid session; `Ok(None)` = no session (no
    /// cookie, unopenable/expired/tampered value, or a payload that failed to
    /// decode as JSON — in which case the session was also destroyed so a
    /// stale entry does not linger); `Err` = store outage (503 via
    /// [`FeishuAuthPlugin::store_error`]), never a silent re-login.
    ///
    /// When the payload was undecodable, the returned delete-cookie
    /// `Set-Cookie` value is included so the caller can forward it on
    /// whatever response it ultimately builds.
    async fn read_session(
        &self,
        ctx: &Context,
        session: &FeishuSession,
    ) -> Result<(Option<FeishuSessionData>, Option<String>), StoreError> {
        let Some(cookie_header) = ctx.request.headers.get("cookie").and_then(|v| v.first()) else {
            return Ok((None, None));
        };
        let Some(raw) = read_cookie(cookie_header, &session.cookie_name) else {
            return Ok((None, None));
        };
        let raw = raw.to_string();
        let Some(bytes) = server_session::load(&session.backend, &session.sealer, &raw).await?
        else {
            return Ok((None, None));
        };
        match serde_json::from_slice::<FeishuSessionData>(&bytes) {
            Ok(data) => Ok((Some(data), None)),
            Err(_) => {
                // Undecodable payload: destroy the session and treat this
                // request as if there were no session at all.
                let cleared = server_session::destroy(
                    &session.backend,
                    Some(&raw),
                    &session.cookie_name,
                    &session.cookie_path,
                )
                .await?;
                Ok((None, Some(cleared)))
            }
        }
    }

    /// Session-mode flow: read session → callback (code) → begin login.
    async fn execute_session(&self, mut ctx: Context, session: &FeishuSession) -> PluginResult {
        let cleared_cookie = match self.read_session(&ctx, session).await {
            Ok((Some(data), _)) => {
                attach_identity(&mut ctx, &data.userinfo, self.set_userinfo_header);
                return Ok(PluginOutput::success(ctx));
            }
            Ok((None, cleared)) => cleared,
            Err(e) => return Err(Self::store_error(ctx, e)),
        };

        let code = match self.extract_code(&ctx) {
            Some(c) => c,
            None => {
                let set_cookies = cleared_cookie.into_iter().collect();
                return Self::redirect(ctx, &session.redirect_uri, set_cookies);
            }
        };

        let (access_token, expires_in) = match self.fetch_access_token(&code).await {
            Ok(t) => t,
            Err(FeishuError::Unauthorized(m)) => return Self::reject(ctx, &m),
            Err(e @ FeishuError::Upstream(_)) => return Self::upstream_error(ctx, e.message()),
        };

        let userinfo = match self.fetch_userinfo(&access_token).await {
            Ok(u) => u,
            Err(FeishuError::Unauthorized(m)) => return Self::reject(ctx, &m),
            Err(e @ FeishuError::Upstream(_)) => return Self::upstream_error(ctx, e.message()),
        };

        let subject = userinfo
            .get("user_id")
            .or_else(|| userinfo.get("open_id"))
            .or_else(|| userinfo.get("union_id"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let ttl = Duration::from_secs(session.cookie_lifetime);
        let meta = server_session::meta_now(&ctx, "feishu-auth", subject, ttl);
        // APISIX's skew: cache the app access token slightly short of its
        // real expiry.
        let access_token_expires_at = expires_in.map(|secs| now_unix() + secs.saturating_sub(60));
        let session_data = FeishuSessionData {
            userinfo: userinfo.clone(),
            access_token: Some(access_token),
            access_token_expires_at,
        };
        let payload = serde_json::to_vec(&session_data).unwrap_or_default();
        let attrs = Self::session_attrs(session, &ctx);
        let set_cookie = match server_session::establish(
            &session.backend,
            &session.sealer,
            &payload,
            ttl,
            meta,
            &session.cookie_name,
            &attrs,
        )
        .await
        {
            Ok(s) => s,
            Err(e) => return Err(Self::store_error(ctx, e)),
        };

        attach_identity(&mut ctx, &userinfo, self.set_userinfo_header);
        ctx.response
            .headers
            .insert("set-cookie".to_string(), vec![set_cookie]);
        Ok(PluginOutput::success(ctx))
    }
}

fn require_string(
    config: &HashMap<String, serde_json::Value>,
    key: &str,
) -> Result<String, String> {
    config
        .get(key)
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(String::from)
        .ok_or_else(|| format!("feishu-auth plugin requires '{}'", key))
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

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Parses the user access token from Feishu's v2 token response, returning
/// the token and (when present) its `expires_in` seconds.
fn parse_access_token(resp: &OutboundResponse) -> Result<(String, Option<u64>), FeishuError> {
    if resp.status != 200 {
        return Err(FeishuError::Upstream(format!(
            "unexpected token response status: {}",
            resp.status
        )));
    }
    let data: serde_json::Value = serde_json::from_slice(&resp.body)
        .map_err(|e| FeishuError::Upstream(format!("failed to decode token response: {}", e)))?;
    // Feishu returns `code: 0` on success for the v2 token endpoint; a non-zero
    // code (e.g. bad/expired authorization code) is an auth failure.
    if let Some(code) = data.get("code").and_then(|v| v.as_i64()) {
        if code != 0 {
            let msg = data
                .get("error_description")
                .and_then(|v| v.as_str())
                .or_else(|| data.get("msg").and_then(|v| v.as_str()))
                .unwrap_or("unknown");
            return Err(FeishuError::Unauthorized(format!(
                "feishu rejected code (code {}): {}",
                code, msg
            )));
        }
    }
    let token = data
        .get("access_token")
        .and_then(|v| v.as_str())
        .map(String::from)
        .ok_or_else(|| {
            FeishuError::Unauthorized("token response missing access_token".to_string())
        })?;
    let expires_in = data.get("expires_in").and_then(|v| v.as_u64());
    Ok((token, expires_in))
}

/// Parses Feishu's userinfo response, returning `data.data` on `code == 0`.
fn parse_userinfo(resp: &OutboundResponse) -> Result<serde_json::Value, FeishuError> {
    if resp.status != 200 {
        return Err(FeishuError::Upstream(format!(
            "unexpected userinfo response status: {}",
            resp.status
        )));
    }
    let data: serde_json::Value = serde_json::from_slice(&resp.body)
        .map_err(|e| FeishuError::Upstream(format!("failed to decode userinfo response: {}", e)))?;
    let code = data.get("code").and_then(|v| v.as_i64()).unwrap_or(-1);
    if code != 0 {
        let msg = data
            .get("msg")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");
        return Err(FeishuError::Unauthorized(format!(
            "feishu userinfo rejected token (code {}): {}",
            code, msg
        )));
    }
    data.get("data")
        .cloned()
        .ok_or_else(|| FeishuError::Upstream("userinfo response missing data".to_string()))
}

/// Copies the resolved identity into `context.message` and optionally the
/// `X-Userinfo` request header.
fn attach_identity(ctx: &mut Context, userinfo: &serde_json::Value, set_header: bool) {
    ctx.message
        .insert("feishu_userinfo".to_string(), userinfo.clone());
    if let Some(uid) = userinfo
        .get("user_id")
        .or_else(|| userinfo.get("open_id"))
        .or_else(|| userinfo.get("union_id"))
        .and_then(|v| v.as_str())
    {
        ctx.message.insert(
            "user_id".to_string(),
            serde_json::Value::String(uid.to_string()),
        );
    }
    if set_header {
        if let Ok(raw) = serde_json::to_vec(userinfo) {
            ctx.request
                .headers
                .insert("x-userinfo".to_string(), vec![BASE64_STANDARD.encode(raw)]);
        }
    }
}

#[async_trait]
impl Plugin for FeishuAuthPlugin {
    fn plugin_type(&self) -> &str {
        "feishu-auth"
    }

    async fn execute(&self, mut ctx: Context) -> PluginResult {
        // Never let a client-supplied X-Userinfo bleed through to the upstream.
        ctx.request.headers.remove("x-userinfo");

        if let Some(session) = &self.session {
            return self.execute_session(ctx, session).await;
        }

        let code = match self.extract_code(&ctx) {
            Some(c) => c,
            None => return Self::reject(ctx, "Missing Feishu authorization code"),
        };

        let (access_token, _expires_in) = match self.fetch_access_token(&code).await {
            Ok(t) => t,
            Err(FeishuError::Unauthorized(m)) => return Self::reject(ctx, &m),
            Err(e @ FeishuError::Upstream(_)) => return Self::upstream_error(ctx, e.message()),
        };

        let userinfo = match self.fetch_userinfo(&access_token).await {
            Ok(u) => u,
            Err(FeishuError::Unauthorized(m)) => return Self::reject(ctx, &m),
            Err(e @ FeishuError::Upstream(_)) => return Self::upstream_error(ctx, e.message()),
        };

        attach_identity(&mut ctx, &userinfo, self.set_userinfo_header);
        Ok(PluginOutput::success(ctx))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::{GatewayRequest, GatewayResponse, Protocol};

    fn resp(status: u16, body: serde_json::Value) -> OutboundResponse {
        OutboundResponse {
            status,
            headers: HashMap::new(),
            body: Bytes::from(serde_json::to_vec(&body).unwrap()),
        }
    }

    fn base_ctx() -> Context {
        Context {
            request: GatewayRequest {
                method: "GET".to_string(),
                path: "/".to_string(),
                host: "h".to_string(),
                scheme: "http".to_string(),
                headers: HashMap::new(),
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

    fn full_cfg() -> HashMap<String, serde_json::Value> {
        [
            ("app_id", "id"),
            ("app_secret", "secret"),
            ("auth_redirect_uri", "https://app/callback"),
        ]
        .iter()
        .map(|(k, v)| (k.to_string(), serde_json::Value::String(v.to_string())))
        .collect()
    }

    #[test]
    fn test_requires_id_secret_redirect() {
        assert!(FeishuAuthPlugin::from_config(&HashMap::new(), &PluginResources::empty()).is_err());
        // missing auth_redirect_uri
        let mut cfg: HashMap<String, serde_json::Value> = HashMap::new();
        cfg.insert("app_id".to_string(), serde_json::json!("id"));
        cfg.insert("app_secret".to_string(), serde_json::json!("secret"));
        assert!(FeishuAuthPlugin::from_config(&cfg, &PluginResources::empty()).is_err());
        assert!(FeishuAuthPlugin::from_config(&full_cfg(), &PluginResources::empty()).is_ok());
    }

    #[test]
    fn test_token_request_body_shape() {
        let plugin = FeishuAuthPlugin::from_config(&full_cfg(), &PluginResources::empty()).unwrap();
        let body = plugin.token_request_body("the-code");
        assert_eq!(body.get("grant_type").unwrap(), "authorization_code");
        assert_eq!(body.get("client_id").unwrap(), "id");
        assert_eq!(body.get("client_secret").unwrap(), "secret");
        assert_eq!(body.get("redirect_uri").unwrap(), "https://app/callback");
        assert_eq!(body.get("code").unwrap(), "the-code");
    }

    #[test]
    fn test_parse_access_token() {
        let ok = resp(
            200,
            serde_json::json!({ "code": 0, "access_token": "tok", "expires_in": 7200 }),
        );
        let (token, expires_in) = parse_access_token(&ok).unwrap();
        assert_eq!(token, "tok");
        assert_eq!(expires_in, Some(7200));

        // non-zero code → unauthorized
        let denied = resp(
            200,
            serde_json::json!({ "code": 20037, "error_description": "invalid code" }),
        );
        assert!(matches!(
            parse_access_token(&denied),
            Err(FeishuError::Unauthorized(_))
        ));

        let bad_status = resp(400, serde_json::json!({}));
        assert!(matches!(
            parse_access_token(&bad_status),
            Err(FeishuError::Upstream(_))
        ));
    }

    #[test]
    fn test_parse_userinfo() {
        let ok = resp(
            200,
            serde_json::json!({ "code": 0, "data": { "user_id": "u1", "name": "Bob" } }),
        );
        let data = parse_userinfo(&ok).unwrap();
        assert_eq!(data.get("user_id").unwrap(), "u1");

        let denied = resp(
            200,
            serde_json::json!({ "code": 99991663, "msg": "token invalid" }),
        );
        assert!(matches!(
            parse_userinfo(&denied),
            Err(FeishuError::Unauthorized(_))
        ));
    }

    #[test]
    fn test_attach_identity() {
        let mut ctx = base_ctx();
        let userinfo = serde_json::json!({ "user_id": "u1", "open_id": "ou_x", "name": "Bob" });
        attach_identity(&mut ctx, &userinfo, true);
        assert_eq!(ctx.message.get("user_id").unwrap(), "u1");
        assert!(ctx.request.headers.contains_key("x-userinfo"));
    }

    #[tokio::test]
    async fn test_missing_code_rejected_401() {
        let plugin = FeishuAuthPlugin::from_config(&full_cfg(), &PluginResources::empty()).unwrap();
        let out = plugin.execute(base_ctx()).await.unwrap();
        assert_eq!(out.port, Some("denied"));
        assert_eq!(out.context.response.status_code, 401);
    }

    #[tokio::test]
    async fn test_upstream_callout_failure_stays_on_error_port() {
        // A Feishu callout that fails outright (here: nothing listening on
        // the port) is a genuine infra failure and must stay a raw `Err`,
        // unlike the deliberate `Unauthorized`/missing-code denials above.
        let mut cfg = full_cfg();
        cfg.insert(
            "access_token_url".to_string(),
            serde_json::json!("http://127.0.0.1:1"),
        );
        cfg.insert("timeout".to_string(), serde_json::json!(200));
        let plugin = FeishuAuthPlugin::from_config(&cfg, &PluginResources::empty()).unwrap();

        let mut ctx = base_ctx();
        ctx.request
            .query_params
            .insert("code".to_string(), vec!["some-code".to_string()]);
        let err = plugin.execute(ctx).await.unwrap_err();
        assert_eq!(err.error.code, "FEISHU_UPSTREAM_ERROR");
        assert!(err.context.response.status_code >= 500);
    }

    #[test]
    fn test_stateless_mode_unchanged() {
        // No session key → from_config succeeds without redirect_uri, and
        // the existing behavior (above) is untouched by this file's changes.
        let plugin = FeishuAuthPlugin::from_config(&full_cfg(), &PluginResources::empty()).unwrap();
        assert!(plugin.session.is_none());
    }

    #[test]
    fn test_session_mode_requires_redirect_uri() {
        // session.secret set but no redirect_uri → config error naming it.
        let mut cfg = full_cfg();
        cfg.insert(
            "session".to_string(),
            serde_json::json!({ "secret": "s3cr3t" }),
        );
        let err = FeishuAuthPlugin::from_config(&cfg, &PluginResources::empty())
            .err()
            .unwrap();
        assert!(err.contains("redirect_uri"), "{err}");
    }

    #[tokio::test]
    async fn test_session_mode_no_code_redirects() {
        let mut cfg = full_cfg();
        cfg.insert(
            "redirect_uri".to_string(),
            serde_json::json!("https://login.example.com/start"),
        );
        cfg.insert(
            "session".to_string(),
            serde_json::json!({"secret": "s3cr3t"}),
        );
        let plugin = FeishuAuthPlugin::from_config(&cfg, &PluginResources::empty()).unwrap();
        let out = plugin.execute(base_ctx()).await.unwrap();
        assert_eq!(out.port, Some("redirect"));
        assert_eq!(out.context.response.status_code, 302);
        assert_eq!(
            out.context.response.headers["location"],
            vec!["https://login.example.com/start".to_string()]
        );
    }

    #[tokio::test]
    async fn test_session_mode_valid_cookie_skips_callout() {
        // Cookie mode: hand-seal the session payload, present it, assert
        // success + identity attached WITHOUT any HTTP callout (endpoints
        // point at 127.0.0.1:1 — reaching them would error).
        let mut config = full_cfg();
        config.insert(
            "access_token_url".to_string(),
            serde_json::json!("http://127.0.0.1:1"),
        );
        config.insert(
            "userinfo_url".to_string(),
            serde_json::json!("http://127.0.0.1:1"),
        );
        config.insert(
            "redirect_uri".to_string(),
            serde_json::json!("https://login.example.com/start"),
        );
        config.insert("timeout".to_string(), serde_json::json!(200));
        config.insert(
            "session".to_string(),
            serde_json::json!({ "secret": "s3cr3t" }),
        );
        let plugin = FeishuAuthPlugin::from_config(&config, &PluginResources::empty()).unwrap();

        let sealer = CookieSealer::new("s3cr3t");
        let session_data = FeishuSessionData {
            userinfo: serde_json::json!({ "user_id": "u1" }),
            access_token: Some("cached-token".to_string()),
            access_token_expires_at: Some(1_000_000),
        };
        let payload = serde_json::to_vec(&session_data).unwrap();
        let sealed = sealer.seal(&payload, Duration::from_secs(86_400));

        let mut ctx = base_ctx();
        ctx.request.headers.insert(
            "cookie".to_string(),
            vec![format!("feishu_session={}", sealed)],
        );

        let out = plugin.execute(ctx).await.unwrap();
        assert!(out.port.is_none());
        assert_eq!(out.context.message.get("user_id").unwrap(), "u1");
    }

    #[cfg(feature = "redis-store")]
    fn resources_with_fake_store() -> (Arc<PluginResources>, Arc<crate::sessions::FakeSessionStore>)
    {
        let fake = Arc::new(crate::sessions::FakeSessionStore::default());
        let resources = PluginResources::empty();
        resources.stores.store(Arc::new(
            crate::stores::StoreRegistry::with_fake_session_store("s1", fake.clone()),
        ));
        (resources, fake)
    }

    /// In redis mode a valid session cookie authenticates from the store
    /// without any Feishu callout, and a store outage is a 503 on the
    /// error port — never a silent re-login.
    #[cfg(feature = "redis-store")]
    #[tokio::test]
    async fn test_redis_session_read_and_store_outage_503() {
        use crate::sessions::SessionStore as _;

        let (resources, fake) = resources_with_fake_store();
        let mut config = full_cfg();
        config.insert(
            "access_token_url".to_string(),
            serde_json::json!("http://127.0.0.1:1"),
        );
        config.insert(
            "userinfo_url".to_string(),
            serde_json::json!("http://127.0.0.1:1"),
        );
        config.insert(
            "redirect_uri".to_string(),
            serde_json::json!("https://login.example.com/start"),
        );
        config.insert("timeout".to_string(), serde_json::json!(200));
        config.insert(
            "session".to_string(),
            serde_json::json!({ "secret": "s3cr3t", "storage": "redis", "store": "s1" }),
        );
        let plugin = FeishuAuthPlugin::from_config(&config, &resources).unwrap();

        // Hand-put sealed session data under an id, the way a successful
        // callback would have established it.
        let sealer = CookieSealer::new("s3cr3t");
        let session_data = FeishuSessionData {
            userinfo: serde_json::json!({ "user_id": "u1" }),
            access_token: None,
            access_token_expires_at: None,
        };
        let payload = serde_json::to_vec(&session_data).unwrap();
        let sealed = sealer.seal(&payload, Duration::from_secs(86_400));
        let id = crate::sessions::SessionId::random();
        let meta = crate::sessions::SessionMeta {
            id: String::new(),
            subject: "u1".to_string(),
            plugin: "feishu-auth".to_string(),
            policy: String::new(),
            route: String::new(),
            created_at: 0,
            expires_at: 0,
        };
        fake.put(&id, sealed.as_bytes(), Duration::from_secs(86_400), &meta)
            .await
            .unwrap();

        let mut ctx = base_ctx();
        ctx.request.headers.insert(
            "cookie".to_string(),
            vec![format!("feishu_session={}", id.as_str())],
        );
        let out = plugin.execute(ctx).await.unwrap();
        assert!(out.port.is_none());
        assert_eq!(out.context.message.get("user_id").unwrap(), "u1");

        // Outage: same request, failing store.
        fake.fail.store(true, std::sync::atomic::Ordering::Relaxed);
        let mut ctx = base_ctx();
        ctx.request.headers.insert(
            "cookie".to_string(),
            vec![format!("feishu_session={}", id.as_str())],
        );
        let err = plugin.execute(ctx).await.unwrap_err();
        assert_eq!(err.error.code, "SESSION_STORE_ERROR");
        assert_eq!(err.context.response.status_code, 503);
    }
}
