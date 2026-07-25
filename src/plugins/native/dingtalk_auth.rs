//! DingTalk authentication plugin (`dingtalk-auth`).
//!
//! Validates a DingTalk authorization *code* by exchanging it, through
//! DingTalk's OAuth API, for the calling user's identity, then attaches that
//! identity to the request for downstream nodes. A request whose code cannot
//! be resolved to a DingTalk user is rejected with a `401`.
//!
//! # Ported subset / deviations from APISIX
//!
//! APISIX's `dingtalk-auth` is a *session* plugin: on the first request it
//! reads a code, calls DingTalk, then stores the resolved userinfo in an
//! encrypted `dingtalk_session` cookie so later requests skip the callout, and
//! it 302-redirects to `redirect_uri` when no code and no session are present.
//! featherbit is stateless with no session store, so this port implements the
//! **token-validation subset**: every request must carry a code, which is
//! validated against DingTalk on each request. Consequently the session /
//! cookie / redirect machinery is dropped, along with the config keys that only
//! served it (`secret`, `secret_fallbacks`, `redirect_uri`, `cookie_expires_in`).
//! The app-level access token *is* cached in-process (7000s TTL, matching
//! APISIX's `lrucache`) so only the userinfo call happens per request.

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
use crate::plugins::{Plugin, PluginExecutionError, PluginOutput, PluginResult};

const DEFAULT_USERINFO_URL: &str = "https://oapi.dingtalk.com/topapi/v2/user/getuserinfo";
const DEFAULT_TOKEN_URL: &str = "https://api.dingtalk.com/v1.0/oauth2/accessToken";
/// DingTalk access tokens live 7200s; cache slightly shorter to avoid using a
/// token that expires mid-flight (matches APISIX's cache TTL).
const ACCESS_TOKEN_TTL: Duration = Duration::from_secs(7000);

/// Outcome of resolving a DingTalk code into userinfo. Every failure maps to a
/// `401` (`DINGTALK_AUTH_FAILED`); the variants exist to keep the reason legible.
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
    /// Session-only APISIX keys (`secret`, `secret_fallbacks`, `redirect_uri`,
    /// `cookie_expires_in`) are not accepted — see the module docs.
    ///
    /// ```yaml
    /// type: dingtalk-auth
    /// config:
    ///   app_key: ${DINGTALK_APP_KEY}
    ///   app_secret: ${DINGTALK_APP_SECRET}
    ///   code_header: X-DingTalk-Code
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
        };
        let resp = self
            .resources
            .outbound
            .request(req)
            .await
            .map_err(|e| DingtalkError::Upstream(format!("userinfo callout failed: {}", e)))?;
        parse_userinfo(&resp)
    }

    /// Builds the `401` rejection carrying the context so the graph engine
    /// routes through the error port.
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
        Err(PluginExecutionError {
            context: ctx,
            error: GatewayError {
                node_id: String::new(),
                code: "DINGTALK_AUTH_FAILED".to_string(),
                message: message.to_string(),
                metadata: HashMap::new(),
            },
        })
    }
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

    async fn execute(
        &self,
        mut ctx: Context,
        _named_inputs: &HashMap<String, serde_json::Value>,
    ) -> PluginResult {
        // Never let a client-supplied X-Userinfo bleed through to the upstream.
        ctx.request.headers.remove("x-userinfo");

        let code = match self.extract_code(&ctx) {
            Some(c) => c,
            None => return Self::reject(ctx, "Missing DingTalk authorization code"),
        };

        let access_token = match self.access_token().await {
            Ok(t) => t,
            Err(e) => return Self::reject(ctx, e.message()),
        };

        let userinfo = match self.fetch_userinfo(&access_token, &code).await {
            Ok(u) => u,
            Err(e) => return Self::reject(ctx, e.message()),
        };

        attach_identity(&mut ctx, &userinfo, self.set_userinfo_header);
        Ok(PluginOutput {
            context: ctx,
            named_outputs: HashMap::new(),
        })
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
        let out = plugin.execute(base_ctx(), &HashMap::new()).await;
        let err = out.unwrap_err();
        assert_eq!(err.context.response.status_code, 401);
        assert_eq!(err.error.code, "DINGTALK_AUTH_FAILED");
    }
}
