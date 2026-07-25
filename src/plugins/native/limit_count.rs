//! Fixed-window request-count limiting plugin (`limit-count`).
//!
//! The featherbit port of Apache APISIX's `limit-count`: counts requests per
//! resolved key within a fixed time window using a shared [`CounterStore`]
//! backend (see [`crate::ratelimit`]). Requests that exceed `count` within
//! `time_window` seconds are rejected through the node's error port with the
//! configured status. Unlike the token-bucket `rate-limit` plugin (smooth
//! refill), this enforces a hard cap per discrete window, matching APISIX
//! semantics.

use async_trait::async_trait;
use bytes::Bytes;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use crate::context::{Context, GatewayError};
use crate::plugins::resources::PluginResources;
use crate::plugins::{Plugin, PluginExecutionError, PluginOutput, PluginResult};
use crate::ratelimit::CounterStore;
use crate::vars::interpolate;

/// Enforces a per-key request count within a fixed time window.
///
/// On each request the resolved key is counted against a [`CounterStore`]
/// (currently only the in-memory `local` backend). Within the limit the
/// request passes through the `success` port; over the limit it is rejected
/// with `rejected_code` and a JSON `{"error_msg": ...}` body through the
/// `error` port (code `RATE_LIMITED`). When `show_limit_quota_header` is set
/// the `X-RateLimit-Limit`/`-Remaining`/`-Reset` headers are written onto
/// `context.response` in both cases.
pub struct LimitCountPlugin {
    /// Maximum number of requests allowed per window.
    count: u64,
    /// Length of the fixed window.
    window: Duration,
    /// Key template (`$var` interpolated); empty result falls back to the
    /// client remote address.
    key_template: String,
    /// Optional counter-key prefix so multiple nodes share one counter.
    group: Option<String>,
    /// Status returned when a request is rejected.
    rejected_code: u16,
    /// Optional custom message used in the rejection body.
    rejected_msg: Option<String>,
    /// Whether to emit the `X-RateLimit-*` quota headers.
    show_limit_quota_header: bool,
    /// When true, a counter-backend error lets the request through instead of
    /// failing it.
    allow_degradation: bool,
    /// Resolved counter backend (from `policy`).
    store: Arc<dyn CounterStore>,
}

impl LimitCountPlugin {
    /// Builds the plugin from node config.
    ///
    /// Accepted keys:
    /// - `count` (integer > 0, **required**): requests allowed per window.
    /// - `time_window` (integer > 0, **required**): window length in seconds.
    /// - `key` (string, default `"$remote_addr"`): a `$var` template resolved
    ///   per request (e.g. `$remote_addr`, `$consumer_name`,
    ///   `$http_x_api_key`). An empty resolved value falls back to the client
    ///   remote address.
    /// - `policy` (string, default `"local"`): counter backend. Only `local`
    ///   is available; `redis`/others are rejected at config load.
    /// - `group` (string, optional): prefixes the counter key so multiple
    ///   nodes share one counter.
    /// - `rejected_code` (integer 200-599, default `503`): status for
    ///   over-limit requests.
    /// - `rejected_msg` (string, optional): message placed in the rejection
    ///   body (`{"error_msg": ...}`).
    /// - `show_limit_quota_header` (bool, default `true`): emit the
    ///   `X-RateLimit-*` headers onto the response.
    /// - `allow_degradation` (bool, default `false`): on a counter-backend
    ///   error, allow the request through instead of failing it.
    ///
    /// ```yaml
    /// type: limit-count
    /// config:
    ///   count: 100
    ///   time_window: 60
    ///   key: "$remote_addr"
    ///   policy: local
    ///   rejected_code: 429
    ///   show_limit_quota_header: true
    /// ```
    pub fn from_config(
        config: &HashMap<String, serde_json::Value>,
        resources: &Arc<PluginResources>,
    ) -> Result<Self, String> {
        let count = config
            .get("count")
            .and_then(|v| v.as_u64())
            .filter(|n| *n > 0)
            .ok_or("limit-count requires 'count' as an integer greater than 0")?;

        let time_window = config
            .get("time_window")
            .and_then(|v| v.as_u64())
            .filter(|n| *n > 0)
            .ok_or("limit-count requires 'time_window' as an integer greater than 0 (seconds)")?;

        let key_template = config
            .get("key")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .unwrap_or("$remote_addr")
            .to_string();

        let policy = config
            .get("policy")
            .and_then(|v| v.as_str())
            .unwrap_or("local");
        // Resolves the backend (and surfaces the supported-policy list on an
        // unknown/not-yet-implemented policy such as `redis`).
        let store = resources.counters.get(policy)?;

        let group = config
            .get("group")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(String::from);

        let rejected_code = config
            .get("rejected_code")
            .and_then(|v| v.as_u64())
            .filter(|n| (200..=599).contains(n))
            .map(|n| n as u16)
            .unwrap_or(503);

        let rejected_msg = config
            .get("rejected_msg")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(String::from);

        let show_limit_quota_header = config
            .get("show_limit_quota_header")
            .and_then(|v| v.as_bool())
            .unwrap_or(true);

        let allow_degradation = config
            .get("allow_degradation")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        Ok(Self {
            count,
            window: Duration::from_secs(time_window),
            key_template,
            group,
            rejected_code,
            rejected_msg,
            show_limit_quota_header,
            allow_degradation,
            store,
        })
    }

    /// Resolves the per-request counter key: interpolates the `key` template
    /// and, when it resolves to empty, falls back to the remote address
    /// (matching APISIX). The `group` prefix, when set, is prepended so nodes
    /// in the same group share one counter.
    fn resolve_key(&self, ctx: &Context) -> String {
        let mut key = interpolate(ctx, &self.key_template);
        if key.is_empty() {
            key = interpolate(ctx, "$remote_addr");
        }
        match &self.group {
            Some(group) => format!("{}:{}", group, key),
            None => key,
        }
    }

    /// Writes the `X-RateLimit-*` quota headers onto the response.
    fn set_quota_headers(&self, ctx: &mut Context, remaining: u64, reset: Duration) {
        if !self.show_limit_quota_header {
            return;
        }
        ctx.response.headers.insert(
            "x-ratelimit-limit".to_string(),
            vec![self.count.to_string()],
        );
        ctx.response.headers.insert(
            "x-ratelimit-remaining".to_string(),
            vec![remaining.to_string()],
        );
        // Whole seconds until the window resets (rounded up so a partial
        // second is never reported as 0 while the window is still open).
        let reset_secs = reset.as_secs_f64().ceil() as u64;
        ctx.response.headers.insert(
            "x-ratelimit-reset".to_string(),
            vec![reset_secs.to_string()],
        );
    }
}

#[async_trait]
impl Plugin for LimitCountPlugin {
    fn plugin_type(&self) -> &str {
        "limit-count"
    }

    async fn execute(
        &self,
        mut ctx: Context,
        _named_inputs: &HashMap<String, serde_json::Value>,
    ) -> PluginResult {
        let key = self.resolve_key(&ctx);

        let result = match self
            .store
            .incr_fixed_window(&key, self.count, self.window)
            .await
        {
            Ok(r) => r,
            Err(_) => {
                // Counter backend failed. Fail open when configured to
                // degrade, otherwise reject with a 500 through the error port.
                if self.allow_degradation {
                    return Ok(PluginOutput {
                        context: ctx,
                        named_outputs: HashMap::new(),
                    });
                }
                ctx.response.status_code = 500;
                ctx.response.body = Bytes::from(r#"{"error_msg": "failed to limit count"}"#);
                ctx.response.headers.insert(
                    "content-type".to_string(),
                    vec!["application/json".to_string()],
                );
                return Err(PluginExecutionError {
                    context: ctx,
                    error: GatewayError {
                        node_id: String::new(),
                        code: "RATE_LIMIT_UNAVAILABLE".to_string(),
                        message: "failed to limit count".to_string(),
                        metadata: HashMap::new(),
                    },
                });
            }
        };

        if result.allowed {
            self.set_quota_headers(&mut ctx, result.remaining, result.reset);
            return Ok(PluginOutput {
                context: ctx,
                named_outputs: HashMap::new(),
            });
        }

        // Rejected: quota headers show 0 remaining, then a JSON error body.
        self.set_quota_headers(&mut ctx, 0, result.reset);
        let msg = self
            .rejected_msg
            .clone()
            .unwrap_or_else(|| "Requests over the limit".to_string());
        let body = serde_json::json!({ "error_msg": msg }).to_string();
        ctx.response.status_code = self.rejected_code;
        ctx.response.body = Bytes::from(body);
        ctx.response.headers.insert(
            "content-type".to_string(),
            vec!["application/json".to_string()],
        );

        Err(PluginExecutionError {
            context: ctx,
            error: GatewayError {
                node_id: String::new(),
                code: "RATE_LIMITED".to_string(),
                message: msg,
                metadata: HashMap::new(),
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::{GatewayRequest, GatewayResponse, Protocol};

    fn test_ctx() -> Context {
        let mut headers = HashMap::new();
        headers.insert("x-api-key".to_string(), vec!["abc123".to_string()]);
        Context {
            request: GatewayRequest {
                method: "GET".to_string(),
                path: "/api".to_string(),
                host: "example.com".to_string(),
                scheme: "http".to_string(),
                headers,
                query_params: HashMap::new(),
                body: Bytes::new(),
                remote_addr: "10.1.2.3:44321".to_string(),
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

    fn plugin(config: serde_json::Value) -> Result<LimitCountPlugin, String> {
        let map: HashMap<String, serde_json::Value> = serde_json::from_value(config).unwrap();
        LimitCountPlugin::from_config(&map, &PluginResources::empty())
    }

    #[test]
    fn test_config_requires_count_and_window() {
        assert!(plugin(serde_json::json!({ "time_window": 60 })).is_err());
        assert!(plugin(serde_json::json!({ "count": 10 })).is_err());
        assert!(plugin(serde_json::json!({ "count": 0, "time_window": 60 })).is_err());
        assert!(plugin(serde_json::json!({ "count": 10, "time_window": 0 })).is_err());
        assert!(plugin(serde_json::json!({ "count": 10, "time_window": 60 })).is_ok());
    }

    #[test]
    fn test_unknown_policy_rejected() {
        let err = match plugin(serde_json::json!({
            "count": 10,
            "time_window": 60,
            "policy": "redis"
        })) {
            Ok(_) => panic!("'redis' policy should not resolve yet"),
            Err(e) => e,
        };
        assert!(err.contains("policy"), "{err}");
    }

    #[test]
    fn test_key_interpolation() {
        let ctx = test_ctx();

        // default: remote_addr (port stripped by the var resolver)
        let p = plugin(serde_json::json!({ "count": 10, "time_window": 60 })).unwrap();
        assert_eq!(p.resolve_key(&ctx), "10.1.2.3");

        // custom var template
        let p = plugin(serde_json::json!({
            "count": 10, "time_window": 60, "key": "$http_x_api_key"
        }))
        .unwrap();
        assert_eq!(p.resolve_key(&ctx), "abc123");

        // empty resolution falls back to remote_addr
        let p = plugin(serde_json::json!({
            "count": 10, "time_window": 60, "key": "$http_missing"
        }))
        .unwrap();
        assert_eq!(p.resolve_key(&ctx), "10.1.2.3");

        // group prefixes the key
        let p = plugin(serde_json::json!({
            "count": 10, "time_window": 60, "key": "$remote_addr", "group": "svc"
        }))
        .unwrap();
        assert_eq!(p.resolve_key(&ctx), "svc:10.1.2.3");
    }

    #[tokio::test]
    async fn test_rejects_after_count_requests() {
        let count = 3u64;
        let p = plugin(serde_json::json!({
            "count": count, "time_window": 60, "rejected_code": 429
        }))
        .unwrap();

        // First `count` requests pass.
        for i in 0..count {
            let out = p.execute(test_ctx(), &HashMap::new()).await;
            assert!(out.is_ok(), "request {i} should pass");
            let ctx = out.unwrap().context;
            assert_eq!(
                ctx.response.headers.get("x-ratelimit-limit"),
                Some(&vec!["3".to_string()])
            );
            assert_eq!(
                ctx.response.headers.get("x-ratelimit-remaining"),
                Some(&vec![(count - 1 - i).to_string()])
            );
        }

        // The next one is rejected.
        let err = p.execute(test_ctx(), &HashMap::new()).await.unwrap_err();
        assert_eq!(err.error.code, "RATE_LIMITED");
        assert_eq!(err.context.response.status_code, 429);
        assert_eq!(
            err.context.response.headers.get("x-ratelimit-remaining"),
            Some(&vec!["0".to_string()])
        );
        let body = String::from_utf8(err.context.response.body.to_vec()).unwrap();
        assert!(body.contains("error_msg"), "{body}");
    }

    #[tokio::test]
    async fn test_rejected_msg_used() {
        let p = plugin(serde_json::json!({
            "count": 1, "time_window": 60, "rejected_msg": "slow down"
        }))
        .unwrap();
        assert!(p.execute(test_ctx(), &HashMap::new()).await.is_ok());
        let err = p.execute(test_ctx(), &HashMap::new()).await.unwrap_err();
        let body = String::from_utf8(err.context.response.body.to_vec()).unwrap();
        assert!(body.contains("slow down"), "{body}");
        assert_eq!(err.error.message, "slow down");
    }

    #[tokio::test]
    async fn test_show_limit_quota_header_false_omits_headers() {
        let p = plugin(serde_json::json!({
            "count": 5, "time_window": 60, "show_limit_quota_header": false
        }))
        .unwrap();
        let ctx = p
            .execute(test_ctx(), &HashMap::new())
            .await
            .unwrap()
            .context;
        assert!(!ctx.response.headers.contains_key("x-ratelimit-limit"));
    }
}
