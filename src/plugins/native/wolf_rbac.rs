//! Wolf-RBAC authorization plugin (`wolf-rbac`) — token-check subset.
//!
//! Port of the request-time authorization core of APISIX's `wolf-rbac` plugin.
//! On each request it extracts the caller's wolf RBAC token, parses it, and asks
//! the wolf-server whether that token may perform the request's method on the
//! request's path. On allow it copies the returned user identity into request
//! headers and `context.message`; on deny it exits on the `denied` port.
//!
//! Only an actual authorization verdict is a denial: `200` allows, `401`/`403`
//! denies. A wolf-server callout that fails outright — unreachable, timed out,
//! or answering with any other status (`5xx`, or a `404` from a mistyped
//! `server` URL) — is a genuine infrastructure failure and stays on the
//! `error` port (see [`classify_access_check`]).
//!
//! Only the `_M.rewrite` authorization path is ported. The interactive
//! `/apisix/plugin/wolf-rbac/{login,change_pwd,user_info}` admin endpoints —
//! which proxy credential exchange to wolf-server and mint tokens — are a
//! session/login concern and are **not** implemented. See the Deviations in
//! `website/docs/reference/plugins/wolf-rbac.md`.

use async_trait::async_trait;
use bytes::Bytes;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use crate::context::{Context, GatewayError};
use crate::outbound::{OutboundClient, OutboundRequest};
use crate::plugins::resources::PluginResources;
use crate::plugins::{Plugin, PluginExecutionError, PluginOutput, PluginResult};

/// The rbac-token version prefix wolf uses (`V1#appid#wolf_token`).
const TOKEN_VERSION: &str = "V1";

/// Checks a wolf RBAC token against a wolf-server `access_check` endpoint.
pub struct WolfRbacPlugin {
    /// wolf-server base URL (e.g. `http://127.0.0.1:12180`).
    server: String,
    /// Expected application id; also used as a fallback when a token omits it.
    appid: String,
    /// Prefix prepended to the `UserId`/`Username`/`Nickname` response headers.
    header_prefix: String,
    /// Whether TLS certificates are verified on the callout.
    ssl_verify: bool,
    /// Whole-call deadline for the wolf-server callout.
    timeout: Duration,
    /// Denial status code (401).
    rejected_code: u16,
    client: Arc<OutboundClient>,
}

/// The subset of wolf-server's `userInfo` payload the plugin propagates.
#[derive(Debug, PartialEq)]
struct UserInfo {
    id: String,
    username: String,
    nickname: String,
}

impl WolfRbacPlugin {
    /// Builds the plugin from node config.
    ///
    /// Accepted keys:
    /// - `server` (string, default `"http://127.0.0.1:12180"`): wolf-server
    ///   base URL that `/wolf/rbac/access_check` is called on.
    /// - `appid` (string, default `"unset"`): application id used as the
    ///   `appID` request argument when a token carries none.
    /// - `header_prefix` (string, default `"X-"`): prefix for the identity
    ///   headers injected on an allowed request.
    /// - `ssl_verify` (bool, default `false`): verify wolf-server's TLS cert.
    /// - `timeout_ms` (u64, default `10000`): callout deadline.
    ///
    /// ```yaml
    /// type: wolf-rbac
    /// config:
    ///   server: http://wolf-server:12180
    ///   appid: restful
    ///   header_prefix: X-
    ///   ssl_verify: false
    /// ```
    pub fn from_config(
        config: &HashMap<String, serde_json::Value>,
        resources: &Arc<PluginResources>,
    ) -> Result<Self, String> {
        let server = config
            .get("server")
            .and_then(|v| v.as_str())
            .unwrap_or("http://127.0.0.1:12180")
            .trim_end_matches('/')
            .to_string();

        let appid = config
            .get("appid")
            .and_then(|v| v.as_str())
            .unwrap_or("unset")
            .to_string();

        let header_prefix = config
            .get("header_prefix")
            .and_then(|v| v.as_str())
            .unwrap_or("X-")
            .to_string();

        let ssl_verify = config
            .get("ssl_verify")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        let timeout = Duration::from_millis(
            config
                .get("timeout_ms")
                .and_then(|v| v.as_u64())
                .unwrap_or(10_000),
        );

        Ok(Self {
            server,
            appid,
            header_prefix,
            ssl_verify,
            timeout,
            rejected_code: 401,
            client: resources.outbound.clone(),
        })
    }

    /// Builds a denial and exits on the node's `denied` port.
    fn reject(&self, ctx: Context, message: &str) -> PluginResult {
        let mut ctx = ctx;
        ctx.response.status_code = self.rejected_code;
        ctx.response.body = Bytes::from(format!(
            r#"{{"error": "forbidden", "message": "{}"}}"#,
            message
        ));
        ctx.response.headers.insert(
            "content-type".to_string(),
            vec!["application/json".to_string()],
        );
        Ok(PluginOutput::on_port(ctx, "denied"))
    }

    /// Builds a genuine infrastructure-failure `Err` (wolf-server unreachable,
    /// timed out, or answering `access_check` with a status that is not an
    /// authorization verdict). Unlike [`WolfRbacPlugin::reject`], this exits
    /// through the `error` port because the node never obtained a verdict.
    fn callout_error(&self, ctx: Context, message: String) -> PluginExecutionError {
        let mut ctx = ctx;
        ctx.response.status_code = 500;
        PluginExecutionError {
            context: ctx,
            error: GatewayError {
                node_id: String::new(),
                code: "WOLF_RBAC_UPSTREAM_ERROR".to_string(),
                message,
                metadata: HashMap::new(),
            },
        }
    }
}

/// What a wolf-server `access_check` reply means.
#[derive(Debug, PartialEq, Eq)]
enum AccessCheck {
    /// `200` — wolf-server allowed the request.
    Allowed,
    /// `401`/`403` — wolf-server evaluated the token and refused it. A
    /// deliberate, client-facing decision → the `denied` port.
    Denied,
    /// Anything else — `5xx`, or a `4xx` that means the *request to
    /// wolf-server* was wrong rather than the caller's access (`404` from a
    /// wrong `server` base URL, `400`). No verdict → the `error` port.
    Unexpected,
}

/// Classifies a wolf-server `access_check` status.
///
/// The split matters: a `502` from a wolf-server behind a dead proxy, or a
/// `404` from a mistyped `server` URL, is not "this token may not pass" —
/// reporting it as a 401 hides a broken deployment behind a plausible denial.
fn classify_access_check(status: u16) -> AccessCheck {
    match status {
        200 => AccessCheck::Allowed,
        401 | 403 => AccessCheck::Denied,
        _ => AccessCheck::Unexpected,
    }
}

/// Extracts the rbac token from (in APISIX precedence order): the `rbac_token`
/// query argument, the `Authorization` header, the `X-RBAC-Token` header, then
/// the `x-rbac-token` cookie.
fn extract_rbac_token(
    headers: &HashMap<String, Vec<String>>,
    query: &HashMap<String, Vec<String>>,
) -> Option<String> {
    if let Some(v) = query.get("rbac_token").and_then(|v| v.first()) {
        return Some(v.clone());
    }
    if let Some(v) = headers.get("authorization").and_then(|v| v.first()) {
        return Some(v.clone());
    }
    if let Some(v) = headers.get("x-rbac-token").and_then(|v| v.first()) {
        return Some(v.clone());
    }
    // cookie: x-rbac-token=<value>
    if let Some(cookie) = headers.get("cookie").and_then(|v| v.first()) {
        for part in cookie.split(';') {
            let part = part.trim();
            if let Some(val) = part.strip_prefix("x-rbac-token=") {
                return Some(val.to_string());
            }
        }
    }
    None
}

/// Parses a `V1#<appid>#<wolf_token>` rbac token into `(appid, wolf_token)`.
/// Errors on the wrong version prefix or the wrong number of `#` segments.
fn parse_rbac_token(token: &str) -> Result<(String, String), &'static str> {
    let parts: Vec<&str> = token.splitn(3, '#').collect();
    if parts.len() != 3 || parts[0] != TOKEN_VERSION {
        return Err("invalid rbac token: version");
    }
    Ok((parts[1].to_string(), parts[2].to_string()))
}

/// Percent-encodes a query-argument value (RFC3986 unreserved chars kept).
fn percent_encode(value: &str) -> String {
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

/// Builds the `access_check` URL with the query arguments wolf-server expects.
fn build_access_check_url(
    server: &str,
    appid: &str,
    action: &str,
    res_name: &str,
    client_ip: &str,
) -> String {
    format!(
        "{}/wolf/rbac/access_check?appID={}&resName={}&action={}&clientIP={}",
        server,
        percent_encode(appid),
        percent_encode(res_name),
        percent_encode(action),
        percent_encode(client_ip),
    )
}

/// Extracts `data.userInfo.{id,username,nickname}` from a wolf-server response
/// body. `nickname` falls back to `username`; a missing `username` yields
/// `None` (no identity to propagate).
fn parse_user_info(body: &[u8]) -> Option<UserInfo> {
    let json: serde_json::Value = serde_json::from_slice(body).ok()?;
    let info = json.get("data")?.get("userInfo")?;
    let username = info.get("username").and_then(|v| v.as_str())?.to_string();
    let id = info
        .get("id")
        .map(|v| match v {
            serde_json::Value::String(s) => s.clone(),
            other => other.to_string(),
        })
        .unwrap_or_default();
    let nickname = info
        .get("nickname")
        .and_then(|v| v.as_str())
        .map(String::from)
        .unwrap_or_else(|| username.clone());
    Some(UserInfo {
        id,
        username,
        nickname,
    })
}

#[async_trait]
impl Plugin for WolfRbacPlugin {
    fn plugin_type(&self) -> &str {
        "wolf-rbac"
    }

    async fn execute(
        &self,
        mut ctx: Context,
    ) -> PluginResult {
        let token = match extract_rbac_token(&ctx.request.headers, &ctx.request.query_params) {
            Some(t) => t,
            None => return self.reject(ctx, "Missing rbac token in request"),
        };

        let (appid, wolf_token) = match parse_rbac_token(&token) {
            Ok(pair) => pair,
            Err(_) => return self.reject(ctx, "invalid rbac token: parse failed"),
        };
        // A token that carries no appid falls back to the configured one.
        let appid = if appid.is_empty() {
            self.appid.clone()
        } else {
            appid
        };

        let action = ctx.request.method.clone();
        let res_name = ctx.request.path.clone();
        let client_ip = ctx
            .request
            .remote_addr
            .rsplit_once(':')
            .map_or(ctx.request.remote_addr.as_str(), |(ip, _)| ip)
            .to_string();

        let url = build_access_check_url(&self.server, &appid, &action, &res_name, &client_ip);

        let outbound = OutboundRequest {
            method: http::Method::GET,
            url,
            headers: vec![
                ("x-rbac-token".to_string(), wolf_token),
                (
                    "content-type".to_string(),
                    "application/json; charset=utf-8".to_string(),
                ),
            ],
            body: Bytes::new(),
            timeout: self.timeout,
            ssl_verify: self.ssl_verify,
            tls: None,
        };

        let response = match self.client.request(outbound).await {
            Ok(resp) => resp,
            Err(e) => {
                // A genuine infrastructure failure (wolf-server unreachable),
                // not a deliberate denial: stays on the `error` port.
                return Err(
                    self.callout_error(ctx, format!("request to wolf-server failed: {}", e))
                );
            }
        };

        // Propagate the identity (when present) before the allow/deny decision,
        // matching APISIX which sets these headers regardless of status.
        if let Some(user) = parse_user_info(&response.body) {
            let set = |ctx: &mut Context, suffix: &str, value: &str| {
                let name = format!("{}{}", self.header_prefix, suffix).to_lowercase();
                ctx.request.headers.insert(name, vec![value.to_string()]);
            };
            set(&mut ctx, "UserId", &user.id);
            set(&mut ctx, "Username", &user.username);
            set(&mut ctx, "Nickname", &percent_encode(&user.nickname));
            ctx.message.insert(
                "user".to_string(),
                serde_json::Value::String(user.username.clone()),
            );
            ctx.message.insert(
                "wolf_rbac.user_id".to_string(),
                serde_json::Value::String(user.id.clone()),
            );
        }

        match classify_access_check(response.status) {
            AccessCheck::Allowed => Ok(PluginOutput::success(ctx)),
            AccessCheck::Denied => {
                let reason = serde_json::from_slice::<serde_json::Value>(&response.body)
                    .ok()
                    .and_then(|v| v.get("reason").and_then(|r| r.as_str()).map(String::from))
                    .unwrap_or_else(|| "access denied by wolf-server".to_string());
                self.reject(ctx, &reason)
            }
            // No verdict was obtained — a broken wolf-server or a wrong
            // `server` URL, not a decision about this caller.
            AccessCheck::Unexpected => Err(self.callout_error(
                ctx,
                format!(
                    "unexpected status {} from wolf-server access_check (expected 200/401/403)",
                    response.status
                ),
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_rbac_token() {
        assert_eq!(
            parse_rbac_token("V1#restful#abc.def.ghi"),
            Ok(("restful".to_string(), "abc.def.ghi".to_string()))
        );
        // wolf_token may itself contain '#'
        assert_eq!(
            parse_rbac_token("V1#app#tok#en"),
            Ok(("app".to_string(), "tok#en".to_string()))
        );
        assert!(parse_rbac_token("V2#app#tok").is_err());
        assert!(parse_rbac_token("garbage").is_err());
        assert!(parse_rbac_token("V1#onlytwo").is_err());
    }

    #[test]
    fn test_extract_rbac_token_precedence() {
        // query arg wins
        let mut headers = HashMap::new();
        headers.insert("authorization".to_string(), vec!["hdr".to_string()]);
        let mut query = HashMap::new();
        query.insert("rbac_token".to_string(), vec!["qry".to_string()]);
        assert_eq!(
            extract_rbac_token(&headers, &query),
            Some("qry".to_string())
        );

        // then Authorization header
        assert_eq!(
            extract_rbac_token(&headers, &HashMap::new()),
            Some("hdr".to_string())
        );

        // then X-RBAC-Token header
        let mut headers = HashMap::new();
        headers.insert("x-rbac-token".to_string(), vec!["xh".to_string()]);
        assert_eq!(
            extract_rbac_token(&headers, &HashMap::new()),
            Some("xh".to_string())
        );

        // then cookie
        let mut headers = HashMap::new();
        headers.insert(
            "cookie".to_string(),
            vec!["foo=bar; x-rbac-token=ck; baz=1".to_string()],
        );
        assert_eq!(
            extract_rbac_token(&headers, &HashMap::new()),
            Some("ck".to_string())
        );

        // nothing
        assert_eq!(extract_rbac_token(&HashMap::new(), &HashMap::new()), None);
    }

    #[test]
    fn test_build_access_check_url_encodes_args() {
        let url =
            build_access_check_url("http://wolf:12180", "restful", "GET", "/pet/1 2", "1.2.3.4");
        assert_eq!(
            url,
            "http://wolf:12180/wolf/rbac/access_check?appID=restful&resName=%2Fpet%2F1%202&action=GET&clientIP=1.2.3.4"
        );
    }

    #[test]
    fn test_parse_user_info() {
        let body =
            br#"{"ok":true,"data":{"userInfo":{"id":123,"username":"alice","nickname":"Al"}}}"#;
        assert_eq!(
            parse_user_info(body),
            Some(UserInfo {
                id: "123".to_string(),
                username: "alice".to_string(),
                nickname: "Al".to_string(),
            })
        );

        // nickname falls back to username
        let body = br#"{"data":{"userInfo":{"id":"7","username":"bob"}}}"#;
        assert_eq!(
            parse_user_info(body),
            Some(UserInfo {
                id: "7".to_string(),
                username: "bob".to_string(),
                nickname: "bob".to_string(),
            })
        );

        // no userInfo → None
        assert_eq!(parse_user_info(br#"{"ok":false,"reason":"denied"}"#), None);
        assert_eq!(parse_user_info(b"not json"), None);
    }

    #[tokio::test]
    async fn test_missing_token_rejected() {
        let plugin =
            WolfRbacPlugin::from_config(&HashMap::new(), &PluginResources::empty()).unwrap();
        let ctx = crate::context::Context::new(crate::context::GatewayRequest {
            method: "GET".into(),
            path: "/pet".into(),
            host: "h".into(),
            scheme: "http".into(),
            headers: HashMap::new(),
            query_params: HashMap::new(),
            body: Bytes::new(),
            remote_addr: "1.2.3.4:5".into(),
            protocol: crate::context::Protocol::Http1,
        });
        let out = plugin.execute(ctx).await.unwrap();
        assert_eq!(out.port, Some("denied"));
        assert_eq!(out.context.response.status_code, 401);
    }

    #[tokio::test]
    async fn test_bad_token_rejected() {
        let plugin =
            WolfRbacPlugin::from_config(&HashMap::new(), &PluginResources::empty()).unwrap();
        let mut headers = HashMap::new();
        headers.insert(
            "x-rbac-token".to_string(),
            vec!["not-a-valid-token".to_string()],
        );
        let ctx = crate::context::Context::new(crate::context::GatewayRequest {
            method: "GET".into(),
            path: "/pet".into(),
            host: "h".into(),
            scheme: "http".into(),
            headers,
            query_params: HashMap::new(),
            body: Bytes::new(),
            remote_addr: "1.2.3.4:5".into(),
            protocol: crate::context::Protocol::Http1,
        });
        // Parse failure is caught before any network call.
        let out = plugin.execute(ctx).await.unwrap();
        assert_eq!(out.port, Some("denied"));
    }

    #[tokio::test]
    async fn test_upstream_callout_failure_stays_on_error_port() {
        // A genuine infra failure (nothing listening on the wolf-server port)
        // must stay a raw `Err`, unlike the deliberate denials above.
        let mut cfg = HashMap::new();
        cfg.insert("server".to_string(), serde_json::json!("http://127.0.0.1:1"));
        cfg.insert("timeout_ms".to_string(), serde_json::json!(200));
        let plugin = WolfRbacPlugin::from_config(&cfg, &PluginResources::empty()).unwrap();

        let mut headers = HashMap::new();
        headers.insert(
            "x-rbac-token".to_string(),
            vec!["V1#app#tok".to_string()],
        );
        let ctx = crate::context::Context::new(crate::context::GatewayRequest {
            method: "GET".into(),
            path: "/pet".into(),
            host: "h".into(),
            scheme: "http".into(),
            headers,
            query_params: HashMap::new(),
            body: Bytes::new(),
            remote_addr: "1.2.3.4:5".into(),
            protocol: crate::context::Protocol::Http1,
        });
        let err = plugin.execute(ctx).await.unwrap_err();
        assert_eq!(err.error.code, "WOLF_RBAC_UPSTREAM_ERROR");
    }

    /// Only a status wolf-server uses to express an authorization verdict is a
    /// verdict; everything else means no verdict was obtained.
    #[test]
    fn test_classify_access_check_splits_verdicts_from_failures() {
        assert_eq!(classify_access_check(200), AccessCheck::Allowed);
        assert_eq!(classify_access_check(401), AccessCheck::Denied);
        assert_eq!(classify_access_check(403), AccessCheck::Denied);
        for status in [400u16, 404, 500, 502, 503] {
            assert_eq!(
                classify_access_check(status),
                AccessCheck::Unexpected,
                "status {status}"
            );
        }
    }

    /// Minimal one-shot HTTP server answering any request with a fixed status
    /// line and no body. Returns its port.
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

    fn tokened_ctx() -> Context {
        let mut headers = HashMap::new();
        headers.insert("x-rbac-token".to_string(), vec!["V1#app#tok".to_string()]);
        crate::context::Context::new(crate::context::GatewayRequest {
            method: "GET".into(),
            path: "/pet".into(),
            host: "h".into(),
            scheme: "http".into(),
            headers,
            query_params: HashMap::new(),
            body: Bytes::new(),
            remote_addr: "1.2.3.4:5".into(),
            protocol: crate::context::Protocol::Http1,
        })
    }

    fn plugin_against(port: u16) -> WolfRbacPlugin {
        let mut cfg = HashMap::new();
        cfg.insert(
            "server".to_string(),
            serde_json::json!(format!("http://127.0.0.1:{port}")),
        );
        cfg.insert("timeout_ms".to_string(), serde_json::json!(2000));
        WolfRbacPlugin::from_config(&cfg, &PluginResources::empty()).unwrap()
    }

    /// wolf-server evaluated the token and refused it → the `denied` port.
    #[tokio::test]
    async fn test_wolf_401_decision_is_denied() {
        let port = spawn_status_server("401 Unauthorized").await;
        let out = plugin_against(port).execute(tokened_ctx()).await.unwrap();
        assert_eq!(out.port, Some("denied"));
        assert_eq!(out.context.response.status_code, 401);
    }

    /// Regression: a `500` from wolf-server used to be laundered into the same
    /// 401 denial as a real refusal. It must exit on `error`.
    #[tokio::test]
    async fn test_wolf_5xx_is_error_port_not_denied() {
        let port = spawn_status_server("500 Internal Server Error").await;
        let err = plugin_against(port)
            .execute(tokened_ctx())
            .await
            .unwrap_err();
        assert_eq!(err.error.code, "WOLF_RBAC_UPSTREAM_ERROR");
        assert!(
            err.error.message.contains("unexpected status 500"),
            "{}",
            err.error.message
        );
    }

    /// A `404` from a mistyped `server` base URL is the same class of problem.
    #[tokio::test]
    async fn test_wolf_404_is_error_port_not_denied() {
        let port = spawn_status_server("404 Not Found").await;
        let err = plugin_against(port)
            .execute(tokened_ctx())
            .await
            .unwrap_err();
        assert_eq!(err.error.code, "WOLF_RBAC_UPSTREAM_ERROR");
    }
}
