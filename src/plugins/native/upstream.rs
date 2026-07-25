//! The `upstream` node — forwards the request to a backend target over HTTP,
//! with round-robin, least-connections, or IP-hash load balancing across the
//! configured targets, and writes the backend's reply into `Context.response`.

use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use crate::balancer::{Balancer, Strategy, Target};
use crate::context::{Context, GatewayError, Protocol};
use crate::outbound::{OutboundClient, OutboundError, OutboundRequest};
use crate::plugins::resources::PluginResources;
use crate::plugins::{Plugin, PluginExecutionError, PluginOutput, PluginResult};

/// Proxies the request to one of the configured backend targets and populates
/// `Context.response` with the upstream's status, headers, and body.
///
/// Connection failures, request-build failures, and body-read failures are
/// returned as [`PluginExecutionError`]s so the graph engine can route them
/// through this node's error port.
pub struct UpstreamPlugin {
    /// Backend pool + load-balancing strategy (shared with the L4 stream proxy).
    balancer: Balancer,
    /// Shared pooled HTTP client (from `PluginResources`).
    client: Arc<OutboundClient>,
    /// Whole-call deadline per proxied request.
    timeout: Duration,
    /// Connect to the upstream over TLS (`https`/`wss`); default false.
    tls: bool,
    /// Verify the upstream's TLS certificate; default true. Only meaningful
    /// when `tls` is set.
    ssl_verify: bool,
}

impl UpstreamPlugin {
    /// Builds the plugin from node config.
    ///
    /// Accepted keys:
    /// - `targets` (array of `{host: string, port: integer}`, **required**):
    ///   the backend pool. Entries missing `host` or `port` are skipped;
    ///   an empty resulting pool is a config error.
    /// - `load_balancing` (string, default `round_robin`): one of
    ///   `round_robin`, `least_connections`, or `ip_hash`. Hyphenated and
    ///   short spellings (`round-robin`, `least-conn`) are accepted, as is
    ///   the legacy key name `load_balancer` (see [`Strategy::parse`]).
    ///
    /// - `timeout_ms` (integer, default `60000`): whole-call deadline
    ///   (connect + request + response body) per proxied request; exceeding
    ///   it fails the node with `UPSTREAM_TIMEOUT` through the error port.
    /// - `tls` (bool, default `false`): connect to the upstream over TLS
    ///   (`https` for the buffered path, `wss` for WebSocket).
    /// - `ssl_verify` (bool, default `true`): verify the upstream's TLS
    ///   certificate. Only meaningful when `tls` is set.
    ///
    /// Errors if no valid target is configured, if the load-balancing value
    /// is not a string, or if it names an unknown strategy.
    ///
    /// ```yaml
    /// type: upstream
    /// config:
    ///   targets:
    ///     - host: backend-1
    ///       port: 3000
    ///     - host: backend-2
    ///       port: 3000
    ///   load_balancing: least_connections
    ///   timeout_ms: 60000
    /// ```
    pub fn from_config(
        config: &HashMap<String, serde_json::Value>,
        resources: &Arc<PluginResources>,
    ) -> Result<Self, String> {
        // Tolerant parse: entries missing `host`/`port` are silently skipped
        // (an empty resulting pool is rejected by `Balancer::new`).
        let targets = config
            .get("targets")
            .and_then(|v| v.as_array())
            .map(|seq| {
                seq.iter()
                    .filter_map(|t| {
                        let mapping = t.as_object()?;
                        let host = mapping.get("host")?.as_str()?.to_string();
                        let port = mapping.get("port")?.as_u64()? as u16;
                        Some(Target { host, port })
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();

        // `load_balancing` is the canonical key; `load_balancer` is accepted
        // because earlier UI builds saved configs under that name.
        let strategy = match config
            .get("load_balancing")
            .or_else(|| config.get("load_balancer"))
        {
            None => Strategy::default(),
            Some(v) => {
                let s = v
                    .as_str()
                    .ok_or_else(|| "load_balancing must be a string".to_string())?;
                Strategy::parse(s)?
            }
        };

        let balancer = Balancer::new(targets, strategy)?;

        let timeout = Duration::from_millis(
            config
                .get("timeout_ms")
                .and_then(|v| v.as_u64())
                .unwrap_or(60_000),
        );

        let tls = config.get("tls").and_then(|v| v.as_bool()).unwrap_or(false);
        let ssl_verify = config
            .get("ssl_verify")
            .and_then(|v| v.as_bool())
            .unwrap_or(true);

        Ok(Self {
            balancer,
            client: resources.outbound.clone(),
            timeout,
            tls,
            ssl_verify,
        })
    }
}

#[async_trait]
impl Plugin for UpstreamPlugin {
    fn plugin_type(&self) -> &str {
        "upstream"
    }

    async fn execute(
        &self,
        mut ctx: Context,
        _named_inputs: &HashMap<String, serde_json::Value>,
    ) -> PluginResult {
        let target_idx = self.balancer.select(&ctx.request.remote_addr);
        let target = self.balancer.target(target_idx);

        // WebSocket upgrade: don't do a buffered round-trip. Resolve the target
        // (load balancing works the same — selection is pure) and stash it for
        // the listener, which owns the raw connection and performs the upstream
        // handshake + bidirectional relay. Signal intent with 101. The in-flight
        // counter is intentionally skipped: a WS tunnel outlives this node, so
        // there is no round-trip lifecycle to bound it.
        if ctx.request.protocol == Protocol::WebSocket {
            ctx.message.insert(
                "__ws_upstream_host".to_string(),
                serde_json::json!(target.host),
            );
            ctx.message.insert(
                "__ws_upstream_port".to_string(),
                serde_json::json!(target.port),
            );
            ctx.message.insert(
                "__ws_upstream_path".to_string(),
                serde_json::json!(ctx.request.path),
            );
            ctx.message
                .insert("__ws_upstream_tls".to_string(), serde_json::json!(self.tls));
            ctx.message.insert(
                "__ws_upstream_verify".to_string(),
                serde_json::json!(self.ssl_verify),
            );
            ctx.response.status_code = 101;
            return Ok(PluginOutput {
                context: ctx,
                named_outputs: HashMap::new(),
            });
        }

        let _in_flight_guard = self.balancer.acquire(target_idx);
        let scheme = if self.tls { "https" } else { "http" };
        let uri = format!(
            "{}://{}:{}{}",
            scheme, target.host, target.port, ctx.request.path
        );

        let method: http::Method = ctx.request.method.parse().unwrap_or(http::Method::GET);

        // Forward request headers, overriding Host with the upstream target.
        let mut headers: Vec<(String, String)> = Vec::new();
        for (key, values) in &ctx.request.headers {
            if key.eq_ignore_ascii_case("host") {
                continue;
            }
            for value in values {
                headers.push((key.clone(), value.clone()));
            }
        }
        headers.push((
            "host".to_string(),
            format!("{}:{}", target.host, target.port),
        ));

        let outbound = OutboundRequest {
            method,
            url: uri,
            headers,
            body: ctx.request.body.clone(),
            timeout: self.timeout,
            ssl_verify: self.ssl_verify,
        };

        let response = match self.client.request(outbound).await {
            Ok(resp) => resp,
            Err(e) => {
                let (code, message) = match &e {
                    OutboundError::Timeout(d) => (
                        "UPSTREAM_TIMEOUT",
                        format!(
                            "Upstream {}:{} timed out after {:?}",
                            target.host, target.port, d
                        ),
                    ),
                    OutboundError::InvalidRequest(m) => (
                        "UPSTREAM_REQUEST_BUILD_ERROR",
                        format!("Failed to build upstream request: {}", m),
                    ),
                    OutboundError::Transport(m) => (
                        "UPSTREAM_CONNECTION_ERROR",
                        format!(
                            "Failed to reach upstream {}:{}: {}",
                            target.host, target.port, m
                        ),
                    ),
                };
                let error = GatewayError {
                    node_id: String::new(),
                    code: code.to_string(),
                    message,
                    metadata: HashMap::new(),
                };
                return Err(PluginExecutionError {
                    context: ctx,
                    error,
                });
            }
        };

        // Populate context.response from the upstream response
        ctx.response.status_code = response.status;
        ctx.response.headers = response.headers;
        ctx.response.body = response.body;

        Ok(PluginOutput {
            context: ctx,
            named_outputs: HashMap::new(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plugin_with(strategy: Option<&str>, key: &str, n_targets: usize) -> UpstreamPlugin {
        let targets: Vec<serde_json::Value> = (0..n_targets)
            .map(|i| serde_json::json!({ "host": format!("backend-{}", i), "port": 3000 }))
            .collect();
        let mut config = HashMap::new();
        config.insert("targets".to_string(), serde_json::Value::Array(targets));
        if let Some(s) = strategy {
            config.insert(key.to_string(), serde_json::Value::String(s.to_string()));
        }
        UpstreamPlugin::from_config(&config, &PluginResources::empty()).unwrap()
    }

    #[test]
    fn test_load_balancing_parsing_and_aliases() {
        // canonical key, spec spelling
        assert_eq!(
            plugin_with(Some("least_connections"), "load_balancing", 2)
                .balancer
                .strategy(),
            Strategy::LeastConnections
        );
        // legacy UI key and hyphenated/short spellings
        assert_eq!(
            plugin_with(Some("round-robin"), "load_balancer", 2)
                .balancer
                .strategy(),
            Strategy::RoundRobin
        );
        assert_eq!(
            plugin_with(Some("least-conn"), "load_balancer", 2)
                .balancer
                .strategy(),
            Strategy::LeastConnections
        );
        assert_eq!(
            plugin_with(Some("ip_hash"), "load_balancing", 2)
                .balancer
                .strategy(),
            Strategy::IpHash
        );
        // absent -> default
        assert_eq!(
            plugin_with(None, "load_balancing", 2).balancer.strategy(),
            Strategy::RoundRobin
        );
    }

    #[test]
    fn test_load_balancing_rejects_unknown() {
        let mut config = HashMap::new();
        config.insert(
            "targets".to_string(),
            serde_json::json!([{ "host": "backend", "port": 3000 }]),
        );
        config.insert(
            "load_balancing".to_string(),
            serde_json::Value::String("random".to_string()),
        );
        assert!(UpstreamPlugin::from_config(&config, &PluginResources::empty()).is_err());
    }

    #[tokio::test]
    async fn test_websocket_branch_stashes_target_and_101() {
        use crate::context::GatewayRequest;

        let plugin = plugin_with(None, "load_balancing", 1);
        let mut req_headers = HashMap::new();
        req_headers.insert("upgrade".to_string(), vec!["websocket".to_string()]);
        let ctx = Context::new(GatewayRequest {
            method: "GET".into(),
            path: "/ws/chat".into(),
            host: "h".into(),
            scheme: "http".into(),
            headers: req_headers,
            query_params: HashMap::new(),
            body: bytes::Bytes::new(),
            remote_addr: "1.2.3.4:5".into(),
            protocol: Protocol::WebSocket,
        });

        // No target is reachable, but the WS branch must NOT do a round-trip —
        // it resolves the target and returns a 101 without any network call.
        let out = plugin.execute(ctx, &HashMap::new()).await.unwrap();
        assert_eq!(out.context.response.status_code, 101);
        assert_eq!(
            out.context.message.get("__ws_upstream_host").unwrap(),
            "backend-0"
        );
        assert_eq!(out.context.message.get("__ws_upstream_port").unwrap(), 3000);
        assert_eq!(
            out.context.message.get("__ws_upstream_path").unwrap(),
            "/ws/chat"
        );
        // The in-flight counter was not touched for the WS path.
        assert_eq!(plugin.balancer.in_flight_count(0), 0);
        // TLS flags default to plaintext + verify-on.
        assert_eq!(out.context.message.get("__ws_upstream_tls").unwrap(), false);
        assert_eq!(
            out.context.message.get("__ws_upstream_verify").unwrap(),
            true
        );
    }

    #[test]
    fn test_tls_config_parses_and_defaults() {
        // Defaults: plaintext, verify on.
        let default = plugin_with(None, "load_balancing", 1);
        assert!(!default.tls);
        assert!(default.ssl_verify);

        // Explicit tls + ssl_verify:false.
        let mut config = HashMap::new();
        config.insert(
            "targets".to_string(),
            serde_json::json!([{ "host": "backend", "port": 443 }]),
        );
        config.insert("tls".to_string(), serde_json::json!(true));
        config.insert("ssl_verify".to_string(), serde_json::json!(false));
        let plugin = UpstreamPlugin::from_config(&config, &PluginResources::empty()).unwrap();
        assert!(plugin.tls);
        assert!(!plugin.ssl_verify);
    }
}
