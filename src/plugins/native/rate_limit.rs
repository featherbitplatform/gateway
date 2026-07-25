//! Token-bucket rate limiting plugin (`rate-limit`).
//!
//! Maintains one in-memory token bucket per client key (remote address or a
//! configured header) in a concurrent `DashMap`; requests that find the
//! bucket empty are rejected with 429 through the node's error port.

use async_trait::async_trait;
use bytes::Bytes;
use dashmap::DashMap;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use crate::context::{Context, GatewayError};
use crate::plugins::{Plugin, PluginExecutionError, PluginOutput, PluginResult};

/// Enforces a per-client request rate using the token bucket algorithm.
///
/// Each client key gets its own bucket sized `burst` and refilled at
/// `requests_per_second`; the key is the remote address by default or the
/// value of a configured header (falling back to the remote address when the
/// header is absent). Over-limit requests are rejected with a 429 JSON
/// response, a `Retry-After: 1` header, and error code `RATE_LIMITED`. Does
/// not write to `context.message`.
pub struct RateLimitPlugin {
    /// Sustained refill rate: tokens added per second.
    requests_per_second: u64,
    /// Bucket capacity: maximum requests allowed in a burst.
    burst: u64,
    /// Where the per-client key is taken from.
    key_source: KeySource,
    /// Per-key token buckets, created lazily on first request.
    buckets: Arc<DashMap<String, TokenBucket>>,
}

/// Source of the rate-limiting key for a request.
#[derive(Debug, Clone)]
enum KeySource {
    /// Key on the client's remote address (the default).
    RemoteAddr,
    /// Key on the first value of the named request header.
    Header(String),
}

/// Classic token bucket: refilled continuously based on elapsed time, capped
/// at `max_tokens`; each request consumes one token.
struct TokenBucket {
    tokens: f64,
    last_refill: Instant,
    max_tokens: f64,
    refill_rate: f64,
}

impl TokenBucket {
    fn new(max_tokens: f64, refill_rate: f64) -> Self {
        Self {
            tokens: max_tokens,
            last_refill: Instant::now(),
            max_tokens,
            refill_rate,
        }
    }

    /// Refills the bucket for the elapsed time, then attempts to take one
    /// token; returns false when the request should be rate-limited.
    fn try_consume(&mut self) -> bool {
        let now = Instant::now();
        let elapsed = now.duration_since(self.last_refill).as_secs_f64();
        self.tokens = (self.tokens + elapsed * self.refill_rate).min(self.max_tokens);
        self.last_refill = now;

        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            false
        }
    }
}

impl RateLimitPlugin {
    /// Builds the plugin from node config. Never fails; every key has a
    /// default.
    ///
    /// Accepted keys:
    /// - `requests_per_second` (integer, default `100`): sustained rate per
    ///   client key.
    /// - `burst` (integer, default = `requests_per_second`): bucket capacity.
    /// - `key_from` (string, default remote address): use
    ///   `"header:<name>"` to key on a request header instead; any other
    ///   value keys on the remote address.
    ///
    /// ```yaml
    /// type: rate-limit
    /// config:
    ///   requests_per_second: 10
    ///   burst: 20
    ///   key_from: "header:x-api-key"
    /// ```
    pub fn from_config(config: &HashMap<String, serde_json::Value>) -> Result<Self, String> {
        let requests_per_second = config
            .get("requests_per_second")
            .and_then(|v| v.as_u64())
            .unwrap_or(100);

        let burst = config
            .get("burst")
            .and_then(|v| v.as_u64())
            .unwrap_or(requests_per_second);

        let key_source = match config.get("key_from").and_then(|v| v.as_str()) {
            Some(header) if header.starts_with("header:") => {
                KeySource::Header(header[7..].to_string())
            }
            _ => KeySource::RemoteAddr,
        };

        Ok(Self {
            requests_per_second,
            burst,
            key_source,
            buckets: Arc::new(DashMap::new()),
        })
    }
}

#[async_trait]
impl Plugin for RateLimitPlugin {
    fn plugin_type(&self) -> &str {
        "rate-limit"
    }

    async fn execute(
        &self,
        mut ctx: Context,
        _named_inputs: &HashMap<String, serde_json::Value>,
    ) -> PluginResult {
        let key = match &self.key_source {
            KeySource::RemoteAddr => ctx.request.remote_addr.clone(),
            KeySource::Header(header) => ctx
                .request
                .headers
                .get(header)
                .and_then(|v| v.first())
                .cloned()
                .unwrap_or_else(|| ctx.request.remote_addr.clone()),
        };

        let allowed = {
            let mut bucket = self.buckets.entry(key).or_insert_with(|| {
                TokenBucket::new(self.burst as f64, self.requests_per_second as f64)
            });
            bucket.try_consume()
        };

        if allowed {
            Ok(PluginOutput {
                context: ctx,
                named_outputs: HashMap::new(),
            })
        } else {
            ctx.response.status_code = 429;
            ctx.response.body =
                Bytes::from(r#"{"error": "rate_limited", "message": "Too many requests"}"#);
            ctx.response.headers.insert(
                "content-type".to_string(),
                vec!["application/json".to_string()],
            );
            ctx.response
                .headers
                .insert("retry-after".to_string(), vec!["1".to_string()]);

            let error = GatewayError {
                node_id: String::new(),
                code: "RATE_LIMITED".to_string(),
                message: "Too many requests".to_string(),
                metadata: HashMap::new(),
            };
            Err(PluginExecutionError {
                context: ctx,
                error,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    //! Behavioral tests inspired by Apache APISIX's `t/plugin/limit-req.t`
    //! (token/leaky bucket rate limiting), adapted to featherbit's token bucket.
    //!
    //! The bucket starts full (`burst` tokens) and refills at `requests_per_second`
    //! per second. Over the sub-millisecond span of a test, refill is negligible
    //! (< 1 token), so "fire `burst` requests then one more" is deterministic.
    use super::*;
    use crate::context::{GatewayRequest, GatewayResponse, Protocol};

    fn ctx(remote_addr: &str, header: Option<(&str, &str)>) -> Context {
        let mut headers = HashMap::new();
        if let Some((k, v)) = header {
            headers.insert(k.to_string(), vec![v.to_string()]);
        }
        Context {
            request: GatewayRequest {
                method: "GET".to_string(),
                path: "/hello".to_string(),
                host: "h".to_string(),
                scheme: "http".to_string(),
                headers,
                query_params: HashMap::new(),
                body: Bytes::new(),
                remote_addr: remote_addr.to_string(),
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

    fn plugin(config: serde_json::Value) -> RateLimitPlugin {
        let map: HashMap<String, serde_json::Value> =
            config.as_object().unwrap().clone().into_iter().collect();
        RateLimitPlugin::from_config(&map).unwrap()
    }

    /// Requests within the burst allowance pass; the next is throttled with 429.
    #[tokio::test]
    async fn test_burst_then_throttle() {
        let p = plugin(serde_json::json!({ "requests_per_second": 1, "burst": 2 }));
        // Two tokens available -> two passes.
        assert!(p
            .execute(ctx("1.2.3.4:5", None), &HashMap::new())
            .await
            .is_ok());
        assert!(p
            .execute(ctx("1.2.3.4:5", None), &HashMap::new())
            .await
            .is_ok());
        // Third exhausts the bucket -> 429.
        let err = p
            .execute(ctx("1.2.3.4:5", None), &HashMap::new())
            .await
            .unwrap_err();
        assert_eq!(err.error.code, "RATE_LIMITED");
        assert_eq!(err.context.response.status_code, 429);
        assert_eq!(
            err.context
                .response
                .headers
                .get("retry-after")
                .and_then(|v| v.first())
                .map(String::as_str),
            Some("1")
        );
    }

    /// Each client key gets its own bucket: one client's exhaustion does not
    /// throttle another.
    #[tokio::test]
    async fn test_buckets_are_per_key() {
        let p = plugin(serde_json::json!({ "requests_per_second": 1, "burst": 1 }));
        assert!(p
            .execute(ctx("10.0.0.1:5", None), &HashMap::new())
            .await
            .is_ok());
        // Same client is now throttled...
        assert!(p
            .execute(ctx("10.0.0.1:5", None), &HashMap::new())
            .await
            .is_err());
        // ...but a different client still has a full bucket.
        assert!(p
            .execute(ctx("10.0.0.2:5", None), &HashMap::new())
            .await
            .is_ok());
    }

    /// `key_from: header:<name>` keys on a request header instead of the address.
    #[tokio::test]
    async fn test_key_from_header() {
        let p = plugin(serde_json::json!({
            "requests_per_second": 1, "burst": 1, "key_from": "header:x-api-key"
        }));
        // Two different remote addrs but the SAME api key share one bucket.
        assert!(p
            .execute(
                ctx("10.0.0.1:5", Some(("x-api-key", "k1"))),
                &HashMap::new()
            )
            .await
            .is_ok());
        assert!(p
            .execute(
                ctx("10.0.0.2:5", Some(("x-api-key", "k1"))),
                &HashMap::new()
            )
            .await
            .is_err());
        // A different key has its own bucket.
        assert!(p
            .execute(
                ctx("10.0.0.3:5", Some(("x-api-key", "k2"))),
                &HashMap::new()
            )
            .await
            .is_ok());
    }

    #[test]
    fn test_burst_defaults_to_rps() {
        let p = plugin(serde_json::json!({ "requests_per_second": 7 }));
        assert_eq!(p.burst, 7);
    }
}
