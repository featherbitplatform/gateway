//! Request-mirroring plugin (`proxy-mirror`).
//!
//! The featherbit port of Apache APISIX's `proxy-mirror`: fires a fire-and-
//! forget clone of the incoming request at a shadow upstream for traffic
//! shadowing (comparing a new backend, capturing traffic, ...). The mirror is
//! sent on a detached background task via the shared outbound client; its
//! response and any error are ignored, and the request path is **never**
//! blocked or affected by it.
//!
//! **Placement**: put this node before `upstream` so it observes the request
//! as it will be proxied. It always continues through the `success` port and
//! never errors.

use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use crate::context::Context;
use crate::outbound::{OutboundClient, OutboundRequest};
use crate::plugins::resources::PluginResources;
use crate::plugins::{Plugin, PluginOutput, PluginResult};

/// Whole-call deadline for a mirrored (best-effort) request.
const MIRROR_TIMEOUT: Duration = Duration::from_secs(60);

/// Mirrors matching requests to a shadow host, best-effort.
pub struct ProxyMirrorPlugin {
    /// Shadow base URL, e.g. `http://shadow:8080` (scheme + host + optional
    /// port, no trailing path).
    host: String,
    /// Optional path override; when unset the original request path is used.
    path: Option<String>,
    /// Fraction of requests to mirror, in `0.0..=1.0`.
    sample_ratio: f64,
    /// Shared pooled outbound HTTP client (from `PluginResources`).
    client: Arc<OutboundClient>,
}

/// Draws a pseudo-random fraction in `[0.0, 1.0)`.
///
/// Uses a freshly seeded `RandomState` hasher (new per-call keys) rather than
/// a rand crate — the same cheap, statistically casual source the
/// `fault-injection` plugin uses for sampling. Not cryptographic and not a
/// deterministic sequence; good enough for `sample_ratio`.
fn roll_fraction() -> f64 {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};
    let n = RandomState::new().build_hasher().finish();
    // Map the u64 into [0, 1); dividing by 2^64 keeps it strictly below 1.
    n as f64 / (u64::MAX as f64 + 1.0)
}

impl ProxyMirrorPlugin {
    /// Builds the plugin from node config.
    ///
    /// Accepted keys:
    /// - `host` (string, **required**): shadow base URL — scheme + host +
    ///   optional port, e.g. `http://shadow:8080`. Must start with `http://`
    ///   or `https://`; no trailing path.
    /// - `path` (string, optional): overrides the mirrored request path; when
    ///   unset the original request path is mirrored. The original query
    ///   string is always appended.
    /// - `sample_ratio` (number, default `1.0`): fraction of requests to
    ///   mirror, in `0.0..=1.0`. `1.0` mirrors every request; `0.0` mirrors
    ///   none. Sampling uses a pseudo-random draw (see [`roll_fraction`]).
    ///
    /// ```yaml
    /// type: proxy-mirror
    /// config:
    ///   host: "http://shadow:8080"
    ///   path: "/mirror"
    ///   sample_ratio: 0.5
    /// ```
    pub fn from_config(
        config: &HashMap<String, serde_json::Value>,
        resources: &Arc<PluginResources>,
    ) -> Result<Self, String> {
        let host = config
            .get("host")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .ok_or("proxy-mirror requires 'host' (e.g. \"http://shadow:8080\")")?
            .trim_end_matches('/')
            .to_string();

        if !host.starts_with("http://") && !host.starts_with("https://") {
            return Err(format!(
                "proxy-mirror 'host' must start with http:// or https:// (got '{host}')"
            ));
        }

        let path = config
            .get("path")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(String::from);

        let sample_ratio = match config.get("sample_ratio") {
            None => 1.0,
            Some(v) => v
                .as_f64()
                .filter(|r| (0.0..=1.0).contains(r))
                .ok_or("proxy-mirror 'sample_ratio' must be a number in 0.0..=1.0")?,
        };

        Ok(Self {
            host,
            path,
            sample_ratio,
            client: resources.outbound.clone(),
        })
    }

    /// Decides whether this request should be mirrored, per `sample_ratio`.
    ///
    /// `>= 1.0` always mirrors, `0.0` never mirrors; otherwise a pseudo-random
    /// draw is compared against the ratio.
    fn should_mirror(&self) -> bool {
        if self.sample_ratio >= 1.0 {
            true
        } else {
            roll_fraction() < self.sample_ratio
        }
    }

    /// Builds the outbound mirror request from the current context (pure; does
    /// no I/O). The mirrored URL is `host` + (`path` override or the original
    /// path) + the original query string. All request headers and the body are
    /// copied.
    fn build_request(&self, ctx: &Context) -> OutboundRequest {
        let path = self
            .path
            .clone()
            .unwrap_or_else(|| ctx.request.path.clone());
        let url = match crate::vars::resolve(ctx, "query_string") {
            Some(qs) => format!("{}{}?{}", self.host, path, qs),
            None => format!("{}{}", self.host, path),
        };

        let method: http::Method = ctx.request.method.parse().unwrap_or(http::Method::GET);

        let mut headers: Vec<(String, String)> = Vec::new();
        for (name, values) in &ctx.request.headers {
            for value in values {
                headers.push((name.clone(), value.clone()));
            }
        }

        OutboundRequest {
            method,
            url,
            headers,
            body: ctx.request.body.clone(),
            timeout: MIRROR_TIMEOUT,
            ssl_verify: true,
        }
    }
}

#[async_trait]
impl Plugin for ProxyMirrorPlugin {
    fn plugin_type(&self) -> &str {
        "proxy-mirror"
    }

    async fn execute(
        &self,
        ctx: Context,
        _named_inputs: &HashMap<String, serde_json::Value>,
    ) -> PluginResult {
        if self.should_mirror() {
            // Build everything the detached task needs, then spawn it. The
            // mirror never blocks or affects the request path: its response
            // and any error are dropped.
            let request = self.build_request(&ctx);
            let client = self.client.clone();
            tokio::spawn(async move {
                let _ = client.request(request).await;
            });
        }

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
    use bytes::Bytes;

    fn test_ctx() -> Context {
        let mut headers = HashMap::new();
        headers.insert("x-trace".to_string(), vec!["t1".to_string()]);
        let mut query = HashMap::new();
        query.insert("q".to_string(), vec!["1".to_string()]);
        Context {
            request: GatewayRequest {
                method: "POST".to_string(),
                path: "/api/users".to_string(),
                host: "example.com".to_string(),
                scheme: "http".to_string(),
                headers,
                query_params: query,
                body: Bytes::from_static(b"payload"),
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

    fn plugin(config: serde_json::Value) -> Result<ProxyMirrorPlugin, String> {
        let map: HashMap<String, serde_json::Value> = serde_json::from_value(config).unwrap();
        ProxyMirrorPlugin::from_config(&map, &PluginResources::empty())
    }

    #[test]
    fn test_config_requires_valid_host() {
        assert!(plugin(serde_json::json!({})).is_err());
        assert!(plugin(serde_json::json!({ "host": "" })).is_err());
        assert!(plugin(serde_json::json!({ "host": "shadow:8080" })).is_err());
        assert!(plugin(serde_json::json!({ "host": "http://shadow:8080" })).is_ok());
        // sample_ratio out of range
        assert!(plugin(serde_json::json!({ "host": "http://s", "sample_ratio": 2 })).is_err());
    }

    #[test]
    fn test_sampling_decision() {
        let never = plugin(serde_json::json!({ "host": "http://s", "sample_ratio": 0 })).unwrap();
        let always = plugin(serde_json::json!({ "host": "http://s", "sample_ratio": 1 })).unwrap();
        for _ in 0..100 {
            assert!(!never.should_mirror(), "ratio 0 must never mirror");
            assert!(always.should_mirror(), "ratio 1 must always mirror");
        }
    }

    #[test]
    fn test_build_request_default_path() {
        let p = plugin(serde_json::json!({ "host": "http://shadow:8080" })).unwrap();
        let req = p.build_request(&test_ctx());
        assert_eq!(req.url, "http://shadow:8080/api/users?q=1");
        assert_eq!(req.method, http::Method::POST);
        assert_eq!(req.body, Bytes::from_static(b"payload"));
        assert!(req.headers.iter().any(|(k, v)| k == "x-trace" && v == "t1"));
    }

    #[test]
    fn test_build_request_path_override() {
        let p = plugin(serde_json::json!({
            "host": "http://shadow:8080/", "path": "/mirror"
        }))
        .unwrap();
        let req = p.build_request(&test_ctx());
        // trailing slash on host is trimmed; query string is preserved
        assert_eq!(req.url, "http://shadow:8080/mirror?q=1");
    }

    #[tokio::test]
    async fn test_execute_returns_ok_and_leaves_context() {
        // ratio 1 spawns a task that fails to connect; that's ignored and
        // execute still returns Ok with the context unchanged.
        let p = plugin(serde_json::json!({
            "host": "http://127.0.0.1:1", "sample_ratio": 1
        }))
        .unwrap();
        let out = p.execute(test_ctx(), &HashMap::new()).await.unwrap();
        assert_eq!(out.context.request.path, "/api/users");
        assert_eq!(out.context.response.status_code, 0);
    }
}
