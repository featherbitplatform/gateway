//! DingTalk authentication plugin (`dingtalk-auth`).
//!
//! Validates a DingTalk authorization *code* by exchanging it, through
//! DingTalk's OAuth API, for the calling user's identity, then attaches that
//! identity to the request for downstream nodes. A request whose code is
//! missing or that DingTalk actively rejects is denied with a `401` on the
//! `denied` port; a DingTalk callout that fails outright (network error,
//! non-200, unparseable body) is a genuine infrastructure failure and stays
//! on the `error` port.
//!
//! # Ported subset / deviations from APISIX
//!
//! APISIX's `dingtalk-auth` is a *session* plugin: on the first request it
//! reads a code, calls DingTalk, then stores the resolved userinfo in an
//! encrypted `dingtalk_session` cookie so later requests skip the callout, and
//! it 302-redirects to `redirect_uri` when no code and no session are present.
//! That session machinery is now restored on an opt-in basis, sharing the
//! same [cookie](crate::plugins::util::cookie_session) /
//! [server-store](crate::plugins::util::server_session) primitives as
//! `cas-auth`/`openid-connect`/`authz-casdoor`:
//!
//! - **Stateless (default)** — when no `session.secret` is configured the
//!   node behaves exactly as before: every request must carry a code, which
//!   is validated against DingTalk on each request. No cookie is read or
//!   set.
//! - **Session (opt-in)** — set `session.secret` (or `session_secret`) to
//!   turn the flow back on: strip any client-supplied `x-userinfo`, then read
//!   the `dingtalk_session` cookie (cookie mode: the userinfo JSON is sealed
//!   directly in the cookie; redis mode via `session.storage: redis` +
//!   `session.store: <name>`: the cookie carries a bare id, the sealed
//!   userinfo lives server-side). A valid session attaches identity straight
//!   from the stored payload — no DingTalk callout. An undecodable payload
//!   (corrupt/stale format) is destroyed and treated as no session rather
//!   than failing the request. No session and no code 302-redirects to the
//!   required `redirect_uri` (the `redirect` port). A code present runs the
//!   existing token+userinfo callouts unchanged, then establishes a new
//!   session (payload = the userinfo `result` JSON; subject = `userid` else
//!   `unionid` else empty; ttl = `session.cookie.lifetime`, APISIX's
//!   `cookie_expires_in`, default `86400`) and 302-redirects (the `redirect`
//!   port) to the current URL with the `code` query parameter stripped,
//!   carrying the session `Set-Cookie` — the graph wires `success` straight
//!   to `upstream.in`, which replaces `ctx.response.headers` wholesale, so a
//!   Set-Cookie attached on that path would never reach the browser. The
//!   browser's follow-up request then hits the session-read fast path above
//!   and attaches identity. A session-store outage is a `503`
//!   (`SESSION_STORE_ERROR`) on the `error` port, never a silent
//!   re-login. The app-level access token is still cached in-process (7000s
//!   TTL, matching APISIX's `lrucache`).
//!
//! Remaining deviation: `secret_fallbacks` (APISIX's multi-secret key
//! rotation) is **not** supported — the sealer is a single `session.secret`,
//! same as every other session plugin in this codebase.

use async_trait::async_trait;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use base64::Engine;
use bytes::Bytes;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

use crate::context::{Context, GatewayError};
use crate::outbound::{OutboundRequest, OutboundResponse};
use crate::plugins::resources::PluginResources;
use crate::plugins::util::cookie_session::{read_cookie, CookieAttrs, CookieSealer, SameSite};
use crate::plugins::util::server_session::{self, SessionBackend};
use crate::plugins::{Plugin, PluginExecutionError, PluginOutput, PluginResult};
use crate::sessions::StoreError;

const DEFAULT_USERINFO_URL: &str = "https://oapi.dingtalk.com/topapi/v2/user/getuserinfo";
const DEFAULT_TOKEN_URL: &str = "https://api.dingtalk.com/v1.0/oauth2/accessToken";
/// DingTalk access tokens live 7200s; cache slightly shorter to avoid using a
/// token that expires mid-flight (matches APISIX's cache TTL).
const ACCESS_TOKEN_TTL: Duration = Duration::from_secs(7000);

/// Outcome of resolving a DingTalk code into userinfo. `Unauthorized` is a
/// deliberate denial (exits `denied`, `401`); `Upstream` is a genuine callout
/// failure (exits `error`).
#[derive(Debug)]
enum DingtalkError {
    /// DingTalk rejected the code / access token (auth failure).
    Unauthorized(String),
    /// The callout itself failed (network, non-200, unparseable body).
    Upstream(String),
}

impl DingtalkError {
    fn message(&self) -> &str {
        match self {
            DingtalkError::Unauthorized(m) | DingtalkError::Upstream(m) => m,
        }
    }
}

/// Session-mode settings (present when `session.secret` is configured).
struct DingtalkSession {
    sealer: CookieSealer,
    backend: SessionBackend,
    cookie_name: String,
    cookie_path: String,
    cookie_lifetime: u64,
    redirect_uri: String,
}

/// Authenticates requests by resolving a DingTalk authorization code to a
/// DingTalk user via the OAuth `accessToken` + `getuserinfo` APIs.
pub struct DingtalkAuthPlugin {
    app_key: String,
    app_secret: String,
    /// Lowercased header the code is read from first.
    code_header: String,
    /// Query parameter the code falls back to.
    code_query: String,
    token_url: String,
    userinfo_url: String,
    set_userinfo_header: bool,
    timeout: Duration,
    ssl_verify: bool,
    resources: Arc<PluginResources>,
    /// In-process cache of the app-level access token: `(token, fetched_at)`.
    token_cache: Mutex<Option<(String, Instant)>>,
    /// Session-mode settings; `None` keeps the stateless token-validation
    /// behavior (the pre-existing, backward-compatible default).
    session: Option<DingtalkSession>,
}

impl DingtalkAuthPlugin {
    /// Builds the plugin from node config.
    ///
    /// Accepted keys:
    /// - `app_key` (string, required): DingTalk application key.
    /// - `app_secret` (string, required): DingTalk application secret.
    /// - `code_header` (string, default `"X-DingTalk-Code"`): header the
    ///   authorization code is read from first (matched case-insensitively).
    /// - `code_query` (string, default `"code"`): query parameter the code
    ///   falls back to when the header is absent.
    /// - `access_token_url` (string, default DingTalk's `oauth2/accessToken`).
    /// - `userinfo_url` (string, default DingTalk's `v2/user/getuserinfo`).
    /// - `set_userinfo_header` (bool, default `true`): when true the resolved
    ///   userinfo JSON is base64-encoded into the `X-Userinfo` request header
    ///   for the upstream.
    /// - `timeout` (integer ms, default `6000`): per-callout timeout.
    /// - `ssl_verify` (bool, default `true`): verify DingTalk's TLS certificate.
    ///
    /// Session-mode keys (present ⇒ session mode is enabled — see the module
    /// docs):
    /// - `session_secret` (string) or `session.secret` (string): signing/
    ///   encryption secret for the session cookie. Setting it turns on the
    ///   session flow.
    /// - `session.cookie.name` (string, default `"dingtalk_session"`).
    /// - `session.cookie.path` (string, default `"/"`).
    /// - `session.cookie.lifetime` (u64 seconds, default `86400`; APISIX's
    ///   `cookie_expires_in`).
    /// - `session.storage` / `session.store`: server-side session backend
    ///   (see [`server_session::parse_backend`]).
    /// - `redirect_uri` (string, **required in session mode**): where to
    ///   302 a browser that has neither a valid session nor a code.
    ///
    /// `secret_fallbacks` (APISIX multi-secret rotation) is not accepted —
    /// see the module docs.
    ///
    /// ```yaml
    /// type: dingtalk-auth
    /// config:
    ///   app_key: ${DINGTALK_APP_KEY}
    ///   app_secret: ${DINGTALK_APP_SECRET}
    ///   code_header: X-DingTalk-Code
    ///   session:
    ///     secret: ${DINGTALK_SESSION_SECRET}
    ///   redirect_uri: https://login.example.com/start
    /// ```
    pub fn from_config(
        config: &HashMap<String, serde_json::Value>,
        resources: &Arc<PluginResources>,
    ) -> Result<Self, String> {
        let app_key = require_string(config, "app_key")?;
        let app_secret = require_string(config, "app_secret")?;

        let code_header = config
            .get("code_header")
            .and_then(|v| v.as_str())
            .unwrap_or("X-DingTalk-Code")
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
                    .unwrap_or_else(|| "dingtalk_session".to_string());
                let cookie_path =
                    session_cookie_str(config, "path").unwrap_or_else(|| "/".to_string());
                let cookie_lifetime = session_cookie_u64(config, "lifetime").unwrap_or(86_400);
                let backend = server_session::parse_backend(config, resources, "dingtalk-auth")?;
                Some(DingtalkSession {
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
            app_key,
            app_secret,
            code_header,
            code_query,
            token_url,
            userinfo_url,
            set_userinfo_header,
            timeout,
            ssl_verify,
            resources: resources.clone(),
            token_cache: Mutex::new(None),
            session,
        })
    }

    /// Reads the authorization code from the configured header, falling back to
    /// the query parameter.
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

    /// Returns a valid access token, using the in-process cache when fresh and
    /// fetching a new one from DingTalk otherwise.
    async fn access_token(&self) -> Result<String, DingtalkError> {
        {
            let cache = self.token_cache.lock().await;
            if let Some((token, fetched_at)) = cache.as_ref() {
                if fetched_at.elapsed() < ACCESS_TOKEN_TTL {
                    return Ok(token.clone());
                }
            }
        }

        let body = serde_json::json!({
            "appKey": self.app_key,
            "appSecret": self.app_secret,
        });
        let req = OutboundRequest {
            method: http::Method::POST,
            url: self.token_url.clone(),
            headers: vec![("content-type".to_string(), "application/json".to_string())],
            body: Bytes::from(serde_json::to_vec(&body).unwrap_or_default()),
            timeout: self.timeout,
            ssl_verify: self.ssl_verify,
            tls: None,
        };
        let resp =
            self.resources.outbound.request(req).await.map_err(|e| {
                DingtalkError::Upstream(format!("access token callout failed: {}", e))
            })?;
        let token = parse_access_token(&resp)?;

        let mut cache = self.token_cache.lock().await;
        *cache = Some((token.clone(), Instant::now()));
        Ok(token)
    }

    /// Exchanges the code for DingTalk userinfo using `access_token`.
    async fn fetch_userinfo(
        &self,
        access_token: &str,
        code: &str,
    ) -> Result<serde_json::Value, DingtalkError> {
        let url = append_query(&self.userinfo_url, "access_token", access_token);
        let body = serde_json::json!({ "code": code });
        let req = OutboundRequest {
            method: http::Method::POST,
            url,
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
            .map_err(|e| DingtalkError::Upstream(format!("userinfo callout failed: {}", e)))?;
        parse_userinfo(&resp)
    }

    /// Builds the `401` rejection and exits on the `denied` port. Reserved
    /// for a deliberate denial — a missing code, or DingTalk actively
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

    /// Builds a genuine infrastructure-failure `Err` for a DingTalk callout
    /// that failed outright (network error, non-200, unparseable body) —
    /// unlike `reject`, the node could not do its job rather than DingTalk
    /// deliberately refusing the code.
    fn upstream_error(ctx: Context, message: &str) -> PluginResult {
        let mut ctx = ctx;
        ctx.response.status_code = 502;
        Err(PluginExecutionError {
            context: ctx,
            error: GatewayError {
                node_id: String::new(),
                code: "DINGTALK_UPSTREAM_ERROR".to_string(),
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
    fn session_attrs<'a>(session: &'a DingtalkSession, ctx: &Context) -> CookieAttrs<'a> {
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
    /// `Ok(Some(userinfo))` = a valid session; `Ok(None)` = no session (no
    /// cookie, unopenable/expired/tampered value, or a payload that failed to
    /// decode as JSON — in which case the session was also destroyed so a
    /// stale entry does not linger); `Err` = store outage (503 via
    /// [`DingtalkAuthPlugin::store_error`]), never a silent re-login.
    ///
    /// When the payload was undecodable, the returned delete-cookie
    /// `Set-Cookie` value is included so the caller can forward it on
    /// whatever response it ultimately builds.
    async fn read_session(
        &self,
        ctx: &Context,
        session: &DingtalkSession,
    ) -> Result<(Option<serde_json::Value>, Option<String>), StoreError> {
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
        match serde_json::from_slice::<serde_json::Value>(&bytes) {
            Ok(userinfo) => Ok((Some(userinfo), None)),
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
    async fn execute_session(&self, mut ctx: Context, session: &DingtalkSession) -> PluginResult {
        let cleared_cookie = match self.read_session(&ctx, session).await {
            Ok((Some(userinfo), _)) => {
                attach_identity(&mut ctx, &userinfo, self.set_userinfo_header);
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

        let access_token = match self.access_token().await {
            Ok(t) => t,
            Err(DingtalkError::Unauthorized(m)) => return Self::reject(ctx, &m),
            Err(e @ DingtalkError::Upstream(_)) => return Self::upstream_error(ctx, e.message()),
        };

        let userinfo = match self.fetch_userinfo(&access_token, &code).await {
            Ok(u) => u,
            Err(DingtalkError::Unauthorized(m)) => return Self::reject(ctx, &m),
            Err(e @ DingtalkError::Upstream(_)) => return Self::upstream_error(ctx, e.message()),
        };

        let subject = userinfo
            .get("userid")
            .or_else(|| userinfo.get("unionid"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let ttl = Duration::from_secs(session.cookie_lifetime);
        let meta = server_session::meta_now(&ctx, "dingtalk-auth", subject, ttl);
        let payload = serde_json::to_vec(&userinfo).unwrap_or_default();
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

        // Do NOT attach identity + succeed on this request: the graph wires
        // `success` straight to `upstream.in`, and `upstream` replaces
        // `ctx.response.headers` wholesale, so a Set-Cookie attached here
        // would never reach the browser. Instead 302-redirect to the
        // code-stripped URL carrying the cookie; the browser's follow-up
        // request then hits the session-read fast path above.
        let target = redirect_target(&ctx, &self.code_query);
        Self::redirect(ctx, &target, vec![set_cookie])
    }
}

/// Rebuilds the current request's path+query with the `code` query
/// parameter stripped, for the post-establish redirect (so the browser's
/// follow-up GET doesn't resubmit the one-time code). If the code was read
/// from the header rather than the query string, there is nothing to strip
/// and the query comes back unchanged. Mirrors
/// `authz_casdoor.rs::reconstruct_uri`.
fn redirect_target(ctx: &Context, code_query: &str) -> String {
    let mut uri = ctx.request.path.clone();
    let mut pairs: Vec<String> = Vec::new();
    for (k, values) in &ctx.request.query_params {
        if k == code_query {
            continue;
        }
        for v in values {
            if v.is_empty() {
                pairs.push(k.clone());
            } else {
                pairs.push(format!("{k}={v}"));
            }
        }
    }
    if !pairs.is_empty() {
        uri.push('?');
        uri.push_str(&pairs.join("&"));
    }
    uri
}

/// Extracts a required string config key.
fn require_string(
    config: &HashMap<String, serde_json::Value>,
    key: &str,
) -> Result<String, String> {
    config
        .get(key)
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(String::from)
        .ok_or_else(|| format!("dingtalk-auth plugin requires '{}'", key))
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

/// Appends `key=value` to `url`, choosing `?` or `&` as needed.
fn append_query(url: &str, key: &str, value: &str) -> String {
    let sep = if url.contains('?') { '&' } else { '?' };
    format!("{}{}{}={}", url, sep, key, urlencode(value))
}

/// Minimal percent-encoding for query values (access tokens are URL-safe-ish
/// but may contain `+` / `=`).
fn urlencode(value: &str) -> String {
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

/// Parses the `accessToken` from DingTalk's token-endpoint response.
fn parse_access_token(resp: &OutboundResponse) -> Result<String, DingtalkError> {
    if resp.status != 200 {
        return Err(DingtalkError::Upstream(format!(
            "unexpected token response status: {}",
            resp.status
        )));
    }
    let data: serde_json::Value = serde_json::from_slice(&resp.body)
        .map_err(|e| DingtalkError::Upstream(format!("failed to decode token response: {}", e)))?;
    data.get("accessToken")
        .and_then(|v| v.as_str())
        .map(String::from)
        .ok_or_else(|| DingtalkError::Upstream("token response missing accessToken".to_string()))
}

/// Parses DingTalk's `getuserinfo` response, returning the `result` object on
/// `errcode == 0` and an [`DingtalkError::Unauthorized`] otherwise.
fn parse_userinfo(resp: &OutboundResponse) -> Result<serde_json::Value, DingtalkError> {
    if resp.status != 200 {
        return Err(DingtalkError::Upstream(format!(
            "unexpected userinfo response status: {}",
            resp.status
        )));
    }
    let data: serde_json::Value = serde_json::from_slice(&resp.body).map_err(|e| {
        DingtalkError::Upstream(format!("failed to decode userinfo response: {}", e))
    })?;
    let errcode = data.get("errcode").and_then(|v| v.as_i64()).unwrap_or(-1);
    if errcode != 0 {
        let errmsg = data
            .get("errmsg")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");
        return Err(DingtalkError::Unauthorized(format!(
            "dingtalk rejected code (errcode {}): {}",
            errcode, errmsg
        )));
    }
    data.get("result")
        .cloned()
        .ok_or_else(|| DingtalkError::Upstream("userinfo response missing result".to_string()))
}

/// Copies the resolved identity into `context.message` and optionally the
/// `X-Userinfo` request header.
fn attach_identity(ctx: &mut Context, userinfo: &serde_json::Value, set_header: bool) {
    ctx.message
        .insert("dingtalk_userinfo".to_string(), userinfo.clone());
    if let Some(uid) = userinfo
        .get("userid")
        .or_else(|| userinfo.get("unionid"))
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
impl Plugin for DingtalkAuthPlugin {
    fn plugin_type(&self) -> &str {
        "dingtalk-auth"
    }

    async fn execute(&self, mut ctx: Context) -> PluginResult {
        // Never let a client-supplied X-Userinfo bleed through to the upstream.
        ctx.request.headers.remove("x-userinfo");

        if let Some(session) = &self.session {
            return self.execute_session(ctx, session).await;
        }

        let code = match self.extract_code(&ctx) {
            Some(c) => c,
            None => return Self::reject(ctx, "Missing DingTalk authorization code"),
        };

        let access_token = match self.access_token().await {
            Ok(t) => t,
            Err(DingtalkError::Unauthorized(m)) => return Self::reject(ctx, &m),
            Err(e @ DingtalkError::Upstream(_)) => return Self::upstream_error(ctx, e.message()),
        };

        let userinfo = match self.fetch_userinfo(&access_token, &code).await {
            Ok(u) => u,
            Err(DingtalkError::Unauthorized(m)) => return Self::reject(ctx, &m),
            Err(e @ DingtalkError::Upstream(_)) => return Self::upstream_error(ctx, e.message()),
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

    fn cfg(pairs: &[(&str, &str)]) -> HashMap<String, serde_json::Value> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), serde_json::Value::String(v.to_string())))
            .collect()
    }

    #[test]
    fn test_requires_app_key_and_secret() {
        assert!(
            DingtalkAuthPlugin::from_config(&HashMap::new(), &PluginResources::empty()).is_err()
        );
        let only_key = cfg(&[("app_key", "k")]);
        assert!(DingtalkAuthPlugin::from_config(&only_key, &PluginResources::empty()).is_err());
        let both = cfg(&[("app_key", "k"), ("app_secret", "s")]);
        assert!(DingtalkAuthPlugin::from_config(&both, &PluginResources::empty()).is_ok());
    }

    #[test]
    fn test_extract_code_header_then_query() {
        let cfg = cfg(&[("app_key", "k"), ("app_secret", "s")]);
        let plugin = DingtalkAuthPlugin::from_config(&cfg, &PluginResources::empty()).unwrap();

        let mut ctx = base_ctx();
        assert_eq!(plugin.extract_code(&ctx), None);

        ctx.request
            .query_params
            .insert("code".to_string(), vec!["from-query".to_string()]);
        assert_eq!(plugin.extract_code(&ctx), Some("from-query".to_string()));

        // header wins over query
        ctx.request.headers.insert(
            "x-dingtalk-code".to_string(),
            vec!["from-header".to_string()],
        );
        assert_eq!(plugin.extract_code(&ctx), Some("from-header".to_string()));
    }

    #[test]
    fn test_parse_access_token() {
        let ok = resp(
            200,
            serde_json::json!({ "accessToken": "abc", "expireIn": 7200 }),
        );
        assert_eq!(parse_access_token(&ok).unwrap(), "abc");

        let missing = resp(200, serde_json::json!({ "expireIn": 7200 }));
        assert!(matches!(
            parse_access_token(&missing),
            Err(DingtalkError::Upstream(_))
        ));

        let bad_status = resp(500, serde_json::json!({}));
        assert!(matches!(
            parse_access_token(&bad_status),
            Err(DingtalkError::Upstream(_))
        ));
    }

    #[test]
    fn test_parse_userinfo_success_and_auth_error() {
        let ok = resp(
            200,
            serde_json::json!({ "errcode": 0, "result": { "userid": "u1", "name": "Alice" } }),
        );
        let result = parse_userinfo(&ok).unwrap();
        assert_eq!(result.get("userid").unwrap(), "u1");

        // errcode != 0 → unauthorized (invalid code)
        let denied = resp(
            200,
            serde_json::json!({ "errcode": 40078, "errmsg": "invalid code" }),
        );
        assert!(matches!(
            parse_userinfo(&denied),
            Err(DingtalkError::Unauthorized(_))
        ));
    }

    #[test]
    fn test_attach_identity_sets_message_and_header() {
        let mut ctx = base_ctx();
        let userinfo = serde_json::json!({ "userid": "u1", "name": "Alice" });
        attach_identity(&mut ctx, &userinfo, true);
        assert_eq!(ctx.message.get("user_id").unwrap(), "u1");
        assert!(ctx.message.contains_key("dingtalk_userinfo"));
        let header = ctx
            .request
            .headers
            .get("x-userinfo")
            .unwrap()
            .first()
            .unwrap();
        let decoded = BASE64_STANDARD.decode(header).unwrap();
        let round: serde_json::Value = serde_json::from_slice(&decoded).unwrap();
        assert_eq!(round.get("name").unwrap(), "Alice");
    }

    #[test]
    fn test_append_query() {
        assert_eq!(
            append_query("http://x/y", "access_token", "a b"),
            "http://x/y?access_token=a%20b"
        );
        assert_eq!(
            append_query("http://x/y?z=1", "access_token", "tok"),
            "http://x/y?z=1&access_token=tok"
        );
    }

    #[tokio::test]
    async fn test_missing_code_rejected_401() {
        let cfg = cfg(&[("app_key", "k"), ("app_secret", "s")]);
        let plugin = DingtalkAuthPlugin::from_config(&cfg, &PluginResources::empty()).unwrap();
        let out = plugin.execute(base_ctx()).await.unwrap();
        assert_eq!(out.port, Some("denied"));
        assert_eq!(out.context.response.status_code, 401);
    }

    #[tokio::test]
    async fn test_upstream_callout_failure_stays_on_error_port() {
        // A DingTalk callout that fails outright (here: nothing listening on
        // the port) is a genuine infra failure and must stay a raw `Err`,
        // unlike the deliberate `Unauthorized`/missing-code denials above.
        let mut cfg = cfg(&[("app_key", "k"), ("app_secret", "s")]);
        cfg.insert(
            "access_token_url".to_string(),
            serde_json::json!("http://127.0.0.1:1"),
        );
        cfg.insert("timeout".to_string(), serde_json::json!(200));
        let plugin = DingtalkAuthPlugin::from_config(&cfg, &PluginResources::empty()).unwrap();

        let mut ctx = base_ctx();
        ctx.request
            .query_params
            .insert("code".to_string(), vec!["some-code".to_string()]);
        let err = plugin.execute(ctx).await.unwrap_err();
        assert_eq!(err.error.code, "DINGTALK_UPSTREAM_ERROR");
        assert!(err.context.response.status_code >= 500);
    }

    #[test]
    fn test_stateless_mode_unchanged() {
        // No session key → from_config succeeds without redirect_uri, and
        // the existing test_missing_code_rejected_401 behavior (above) is
        // untouched by this test file's changes.
        let cfg = cfg(&[("app_key", "k"), ("app_secret", "s")]);
        let plugin = DingtalkAuthPlugin::from_config(&cfg, &PluginResources::empty()).unwrap();
        assert!(plugin.session.is_none());
    }

    #[test]
    fn test_session_mode_requires_redirect_uri() {
        // session.secret set but no redirect_uri → config error naming it.
        let mut cfg = cfg(&[("app_key", "k"), ("app_secret", "s")]);
        cfg.insert(
            "session".to_string(),
            serde_json::json!({ "secret": "s3cr3t" }),
        );
        let err = DingtalkAuthPlugin::from_config(&cfg, &PluginResources::empty())
            .err()
            .unwrap();
        assert!(err.contains("redirect_uri"), "{err}");
    }

    #[tokio::test]
    async fn test_session_mode_no_code_redirects() {
        let plugin = DingtalkAuthPlugin::from_config(
            &cfg(&[
                ("app_key", "k"),
                ("app_secret", "s"),
                ("redirect_uri", "https://login.example.com/start"),
            ])
            .into_iter()
            .chain([(
                "session".to_string(),
                serde_json::json!({"secret": "s3cr3t"}),
            )])
            .collect(),
            &PluginResources::empty(),
        )
        .unwrap();
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
        // Cookie mode: hand-seal the userinfo JSON, present it, assert
        // success + identity attached WITHOUT any HTTP callout (endpoints
        // point at 127.0.0.1:1 — reaching them would error).
        let mut config = cfg(&[
            ("app_key", "k"),
            ("app_secret", "s"),
            ("access_token_url", "http://127.0.0.1:1"),
            ("userinfo_url", "http://127.0.0.1:1"),
            ("redirect_uri", "https://login.example.com/start"),
        ]);
        config.insert("timeout".to_string(), serde_json::json!(200));
        config.insert(
            "session".to_string(),
            serde_json::json!({ "secret": "s3cr3t" }),
        );
        let plugin = DingtalkAuthPlugin::from_config(&config, &PluginResources::empty()).unwrap();

        let sealer = CookieSealer::new("s3cr3t");
        let payload = serde_json::to_vec(&serde_json::json!({ "userid": "u1" })).unwrap();
        let sealed = sealer.seal(&payload, Duration::from_secs(86_400));

        let mut ctx = base_ctx();
        ctx.request.headers.insert(
            "cookie".to_string(),
            vec![format!("dingtalk_session={}", sealed)],
        );

        let out = plugin.execute(ctx).await.unwrap();
        assert!(out.port.is_none());
        assert_eq!(out.context.message.get("user_id").unwrap(), "u1");
    }

    /// One-shot mock HTTP server: accepts a single connection and replies with
    /// the given JSON body.
    async fn spawn_json_server(body: serde_json::Value) -> u16 {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            if let Ok((mut stream, _)) = listener.accept().await {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                let mut buf = [0u8; 4096];
                let _ = stream.read(&mut buf).await;
                let body = body.to_string();
                let _ = stream
                    .write_all(
                        format!(
                            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{}",
                            body.len(),
                            body
                        )
                        .as_bytes(),
                    )
                    .await;
                let _ = stream.shutdown().await;
            }
        });
        port
    }

    /// The critical regression test for the Set-Cookie-swallowed-by-upstream
    /// bug: on a successful code exchange in session mode, the node must NOT
    /// exit on `success` (that edge feeds `upstream.in`, which replaces
    /// `ctx.response.headers` wholesale, dropping the Set-Cookie and causing
    /// an infinite login loop). It must instead 302-redirect to the
    /// code-stripped URL carrying the session cookie, and a follow-up
    /// request presenting that cookie must take the session-read fast path.
    #[tokio::test]
    async fn test_session_mode_code_exchange_redirects_with_cookie() {
        let token_port = spawn_json_server(serde_json::json!({
            "accessToken": "app-token",
            "expireIn": 7200
        }))
        .await;
        let userinfo_port = spawn_json_server(serde_json::json!({
            "errcode": 0,
            "result": { "userid": "u1", "name": "Alice" }
        }))
        .await;

        let token_url = format!("http://127.0.0.1:{}", token_port);
        let userinfo_url = format!("http://127.0.0.1:{}", userinfo_port);
        let mut config = cfg(&[
            ("app_key", "k"),
            ("app_secret", "s"),
            ("access_token_url", token_url.as_str()),
            ("userinfo_url", userinfo_url.as_str()),
            ("redirect_uri", "https://login.example.com/start"),
        ]);
        config.insert("timeout".to_string(), serde_json::json!(2000));
        config.insert(
            "session".to_string(),
            serde_json::json!({ "secret": "s3cr3t" }),
        );
        let plugin = DingtalkAuthPlugin::from_config(&config, &PluginResources::empty()).unwrap();

        let mut ctx = base_ctx();
        ctx.request.path = "/callback".to_string();
        ctx.request
            .query_params
            .insert("code".to_string(), vec!["one-time-code".to_string()]);
        ctx.request
            .query_params
            .insert("foo".to_string(), vec!["bar".to_string()]);

        let out = plugin.execute(ctx).await.unwrap();
        assert_eq!(out.port, Some("redirect"));
        assert_eq!(out.context.response.status_code, 302);
        assert_eq!(
            out.context.response.headers["location"],
            vec!["/callback?foo=bar".to_string()]
        );
        // No identity should be attached on this response — it's a bare
        // redirect, not a success.
        assert!(!out.context.message.contains_key("user_id"));
        let set_cookie = out.context.response.headers["set-cookie"][0].clone();
        assert!(set_cookie.starts_with("dingtalk_session="), "{set_cookie}");

        // Follow-up request presenting the cookie the redirect just set must
        // take the fast path: success, identity attached, no callout.
        let cookie_value = set_cookie.split(';').next().unwrap().to_string();
        let mut ctx2 = base_ctx();
        ctx2.request
            .headers
            .insert("cookie".to_string(), vec![cookie_value]);
        let out2 = plugin.execute(ctx2).await.unwrap();
        assert!(out2.port.is_none());
        assert_eq!(out2.context.message.get("user_id").unwrap(), "u1");
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
    /// without any DingTalk callout, and a store outage is a 503 on the
    /// error port — never a silent re-login.
    #[cfg(feature = "redis-store")]
    #[tokio::test]
    async fn test_redis_session_read_and_store_outage_503() {
        use crate::sessions::SessionStore as _;

        let (resources, fake) = resources_with_fake_store();
        let mut config = cfg(&[
            ("app_key", "k"),
            ("app_secret", "s"),
            ("access_token_url", "http://127.0.0.1:1"),
            ("userinfo_url", "http://127.0.0.1:1"),
            ("redirect_uri", "https://login.example.com/start"),
        ]);
        config.insert("timeout".to_string(), serde_json::json!(200));
        config.insert(
            "session".to_string(),
            serde_json::json!({ "secret": "s3cr3t", "storage": "redis", "store": "s1" }),
        );
        let plugin = DingtalkAuthPlugin::from_config(&config, &resources).unwrap();

        // Hand-put sealed userinfo JSON under an id, the way a successful
        // callback would have established it.
        let sealer = CookieSealer::new("s3cr3t");
        let payload = serde_json::to_vec(&serde_json::json!({ "userid": "u1" })).unwrap();
        let sealed = sealer.seal(&payload, Duration::from_secs(86_400));
        let id = crate::sessions::SessionId::random();
        let meta = crate::sessions::SessionMeta {
            id: String::new(),
            subject: "u1".to_string(),
            plugin: "dingtalk-auth".to_string(),
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
            vec![format!("dingtalk_session={}", id.as_str())],
        );
        let out = plugin.execute(ctx).await.unwrap();
        assert!(out.port.is_none());
        assert_eq!(out.context.message.get("user_id").unwrap(), "u1");

        // Outage: same request, failing store.
        fake.fail.store(true, std::sync::atomic::Ordering::Relaxed);
        let mut ctx = base_ctx();
        ctx.request.headers.insert(
            "cookie".to_string(),
            vec![format!("dingtalk_session={}", id.as_str())],
        );
        let err = plugin.execute(ctx).await.unwrap_err();
        assert_eq!(err.error.code, "SESSION_STORE_ERROR");
        assert_eq!(err.context.response.status_code, 503);
    }
}
