//! Feishu / Lark authentication plugin (`feishu-auth`).
//!
//! Validates a Feishu authorization *code* by exchanging it, through Feishu's
//! OAuth v2 token endpoint, for a user access token, then calls Feishu's
//! userinfo endpoint to resolve the calling user's identity and attaches it to
//! the request. A code that cannot be resolved is rejected with a `401`.
//!
//! # Ported subset / deviations from APISIX
//!
//! APISIX's `feishu-auth` is a *session* plugin: it caches the exchanged access
//! token and resolved userinfo in an encrypted `feishu_session` cookie so later
//! requests skip the callouts, and it 302-redirects to `redirect_uri` when no
//! code/session is present. featherbit is stateless with no session store, so
//! this port implements the **token-validation subset**: every request must
//! carry a code, which is exchanged and validated on each request. The session
//! / cookie / redirect machinery is dropped, along with the keys that only
//! served it (`secret`, `secret_fallbacks`, `redirect_uri`, `cookie_expires_in`).
//! `auth_redirect_uri` is retained because it is part of the `authorization_code`
//! token-exchange body, not the interactive redirect.

use async_trait::async_trait;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use base64::Engine;
use bytes::Bytes;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use crate::context::{Context, GatewayError};
use crate::outbound::{OutboundRequest, OutboundResponse};
use crate::plugins::resources::PluginResources;
use crate::plugins::{Plugin, PluginExecutionError, PluginOutput, PluginResult};

const DEFAULT_TOKEN_URL: &str = "https://open.feishu.cn/open-apis/authen/v2/oauth/token";
const DEFAULT_USERINFO_URL: &str = "https://open.feishu.cn/open-apis/authen/v1/user_info";

/// Outcome of resolving a Feishu code. Every failure maps to a `401`
/// (`FEISHU_AUTH_FAILED`); variants exist to keep the reason legible.
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
    /// Session-only APISIX keys (`secret`, `secret_fallbacks`, `redirect_uri`,
    /// `cookie_expires_in`) are not accepted — see the module docs.
    ///
    /// ```yaml
    /// type: feishu-auth
    /// config:
    ///   app_id: ${FEISHU_APP_ID}
    ///   app_secret: ${FEISHU_APP_SECRET}
    ///   auth_redirect_uri: https://app.example.com/callback
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

    /// Exchanges `code` for a Feishu user access token.
    async fn fetch_access_token(&self, code: &str) -> Result<String, FeishuError> {
        let body = self.token_request_body(code);
        let req = OutboundRequest {
            method: http::Method::POST,
            url: self.token_url.clone(),
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
        };
        let resp = self
            .resources
            .outbound
            .request(req)
            .await
            .map_err(|e| FeishuError::Upstream(format!("userinfo callout failed: {}", e)))?;
        parse_userinfo(&resp)
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
        Err(PluginExecutionError {
            context: ctx,
            error: GatewayError {
                node_id: String::new(),
                code: "FEISHU_AUTH_FAILED".to_string(),
                message: message.to_string(),
                metadata: HashMap::new(),
            },
        })
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

/// Parses the user access token from Feishu's v2 token response.
fn parse_access_token(resp: &OutboundResponse) -> Result<String, FeishuError> {
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
    data.get("access_token")
        .and_then(|v| v.as_str())
        .map(String::from)
        .ok_or_else(|| FeishuError::Unauthorized("token response missing access_token".to_string()))
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

    async fn execute(
        &self,
        mut ctx: Context,
        _named_inputs: &HashMap<String, serde_json::Value>,
    ) -> PluginResult {
        ctx.request.headers.remove("x-userinfo");

        let code = match self.extract_code(&ctx) {
            Some(c) => c,
            None => return Self::reject(ctx, "Missing Feishu authorization code"),
        };

        let access_token = match self.fetch_access_token(&code).await {
            Ok(t) => t,
            Err(e) => return Self::reject(ctx, e.message()),
        };

        let userinfo = match self.fetch_userinfo(&access_token).await {
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
        assert_eq!(parse_access_token(&ok).unwrap(), "tok");

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
        let err = plugin
            .execute(base_ctx(), &HashMap::new())
            .await
            .unwrap_err();
        assert_eq!(err.context.response.status_code, 401);
        assert_eq!(err.error.code, "FEISHU_AUTH_FAILED");
    }
}
