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
    /// Per-upstream TLS identity (client cert / private CA); None = shared
    /// clients, exactly the pre-mTLS behavior.
    tls_identity: Option<Arc<crate::outbound::tls::UpstreamTls>>,
}

// Manual `Debug`, scoped to what's useful in test/error output: `balancer`
// and `client` don't implement it (pooled hyper client, atomics-backed pool
// state), so a derive isn't available here.
impl std::fmt::Debug for UpstreamPlugin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UpstreamPlugin")
            .field("timeout", &self.timeout)
            .field("tls", &self.tls)
            .field("ssl_verify", &self.ssl_verify)
            .field("tls_identity_set", &self.tls_identity.is_some())
            .finish()
    }
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
    /// - `client_cert_path` / `client_key_path` (string, optional): PEM
    ///   client certificate and private key presented to the upstream for
    ///   mutual TLS. Must be set together, and only with `tls: true`.
    /// - `ca_cert_path` (string, optional): PEM CA bundle used to verify the
    ///   upstream's certificate, *replacing* the native root store for this
    ///   upstream. Requires `tls: true`; rejected together with
    ///   `ssl_verify: false` (a CA bundle to verify with is contradictory
    ///   when verification is off).
    ///
    /// Errors if no valid target is configured, if the load-balancing value
    /// is not a string, if it names an unknown strategy, if any mTLS key is
    /// set without `tls: true`, if `client_cert_path`/`client_key_path` are
    /// not both set, if `ca_cert_path` is set with `ssl_verify: false`, or if
    /// the configured cert/key/CA files can't be read or parsed.
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
    ///   tls: true
    ///   client_cert_path: /etc/gateway/client.crt
    ///   client_key_path: /etc/gateway/client.key
    ///   ca_cert_path: /etc/gateway/ca.crt
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

        let string_config_value = |key: &str| -> Result<Option<String>, String> {
            match config.get(key) {
                None => Ok(None),
                Some(v) => match v.as_str() {
                    Some(s) => Ok(Some(s.to_string())),
                    None => Err(format!("{} must be a string", key)),
                },
            }
        };

        let client_cert_path = string_config_value("client_cert_path")?;
        let client_key_path = string_config_value("client_key_path")?;
        let ca_cert_path = string_config_value("ca_cert_path")?;

        let any_mtls_key =
            client_cert_path.is_some() || client_key_path.is_some() || ca_cert_path.is_some();
        if any_mtls_key && !tls {
            return Err(
                "client_cert_path/client_key_path/ca_cert_path require tls: true".to_string(),
            );
        }
        if client_cert_path.is_some() != client_key_path.is_some() {
            return Err("client_cert_path and client_key_path must be set together".to_string());
        }
        // A CA bundle exists to *verify* the upstream; pairing it with
        // ssl_verify:false is contradictory, so reject rather than guess.
        if ca_cert_path.is_some() && !ssl_verify {
            return Err("ca_cert_path with ssl_verify: false is contradictory".to_string());
        }

        let tls_identity = if any_mtls_key {
            let client = client_cert_path.as_deref().zip(client_key_path.as_deref());
            let identity = crate::outbound::tls::UpstreamTls::load(
                client,
                ca_cert_path.as_deref(),
                ssl_verify,
            )?;
            crate::outbound::tls::UpstreamTls::register(&identity);
            Some(identity)
        } else {
            None
        };

        Ok(Self {
            balancer,
            client: resources.outbound.clone(),
            timeout,
            tls,
            ssl_verify,
            tls_identity,
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
            if let Some(identity) = &self.tls_identity {
                ctx.message.insert(
                    "__ws_upstream_tls_key".to_string(),
                    serde_json::json!(identity.cache_key()),
                );
            }
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
            tls: self.tls_identity.clone(),
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

    fn write_identity(tag: &str) -> (String, String, String) {
        // Same helper as src/outbound/tls.rs tests: CA + leaf, PEM files in
        // temp_dir, returns (cert_path, key_path, ca_path).
        let mut ca_params = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap();
        ca_params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        let ca_key = rcgen::KeyPair::generate().unwrap();
        let ca_cert = ca_params.self_signed(&ca_key).unwrap();
        let leaf_params = rcgen::CertificateParams::new(vec!["client".to_string()]).unwrap();
        let leaf_key = rcgen::KeyPair::generate().unwrap();
        let leaf_cert = leaf_params.signed_by(&leaf_key, &ca_cert, &ca_key).unwrap();
        let dir = std::env::temp_dir();
        let pid = std::process::id();
        let cert = dir.join(format!("featherbit_up_{}_{}.crt", tag, pid));
        let key = dir.join(format!("featherbit_up_{}_{}.key", tag, pid));
        let ca = dir.join(format!("featherbit_up_{}_{}.ca.crt", tag, pid));
        std::fs::write(&cert, leaf_cert.pem()).unwrap();
        std::fs::write(&key, leaf_key.serialize_pem()).unwrap();
        std::fs::write(&ca, ca_cert.pem()).unwrap();
        (
            cert.to_str().unwrap().to_string(),
            key.to_str().unwrap().to_string(),
            ca.to_str().unwrap().to_string(),
        )
    }

    /// Base config with one target; callers add TLS keys.
    fn mtls_config() -> HashMap<String, serde_json::Value> {
        let mut config = HashMap::new();
        config.insert(
            "targets".to_string(),
            serde_json::json!([{"host": "backend", "port": 443}]),
        );
        config
    }

    #[test]
    fn test_mtls_config_requires_tls_true() {
        let (cert, key, _) = write_identity("needstls");
        let mut config = mtls_config();
        config.insert("client_cert_path".to_string(), serde_json::json!(cert));
        config.insert("client_key_path".to_string(), serde_json::json!(key));
        // tls defaults to false -> config error.
        let err = UpstreamPlugin::from_config(&config, &PluginResources::empty()).unwrap_err();
        assert!(err.contains("tls"), "err was: {}", err);
    }

    #[test]
    fn test_mtls_config_cert_and_key_must_pair() {
        let (cert, _, _) = write_identity("pair");
        let mut config = mtls_config();
        config.insert("tls".to_string(), serde_json::json!(true));
        config.insert("client_cert_path".to_string(), serde_json::json!(cert));
        let err = UpstreamPlugin::from_config(&config, &PluginResources::empty()).unwrap_err();
        assert!(err.contains("together"), "err was: {}", err);
    }

    #[test]
    fn test_mtls_config_ca_with_no_verify_rejected() {
        let (_, _, ca) = write_identity("contradiction");
        let mut config = mtls_config();
        config.insert("tls".to_string(), serde_json::json!(true));
        config.insert("ssl_verify".to_string(), serde_json::json!(false));
        config.insert("ca_cert_path".to_string(), serde_json::json!(ca));
        assert!(UpstreamPlugin::from_config(&config, &PluginResources::empty()).is_err());
    }

    #[test]
    fn test_mtls_config_loads_identity_and_registers() {
        let (cert, key, ca) = write_identity("loads");
        let mut config = mtls_config();
        config.insert("tls".to_string(), serde_json::json!(true));
        config.insert("client_cert_path".to_string(), serde_json::json!(cert));
        config.insert("client_key_path".to_string(), serde_json::json!(key));
        config.insert("ca_cert_path".to_string(), serde_json::json!(ca));
        let plugin = UpstreamPlugin::from_config(&config, &PluginResources::empty()).unwrap();
        let id = plugin.tls_identity.as_ref().expect("identity loaded");
        // Registered for the WebSocket relay to look up.
        assert!(crate::outbound::tls::UpstreamTls::lookup(id.cache_key()).is_some());
    }

    #[test]
    fn test_mtls_config_absent_means_no_identity() {
        let mut config = mtls_config();
        config.insert("tls".to_string(), serde_json::json!(true));
        let plugin = UpstreamPlugin::from_config(&config, &PluginResources::empty()).unwrap();
        assert!(plugin.tls_identity.is_none());
    }

    #[test]
    fn test_mtls_config_non_string_keys_rejected() {
        for key in ["client_cert_path", "client_key_path", "ca_cert_path"] {
            for bad_value in [
                serde_json::json!(123),
                serde_json::json!(true),
                serde_json::json!(["x"]),
            ] {
                let mut config = mtls_config();
                config.insert("tls".to_string(), serde_json::json!(true));
                config.insert(key.to_string(), bad_value.clone());
                let err =
                    UpstreamPlugin::from_config(&config, &PluginResources::empty()).unwrap_err();
                assert!(
                    err.contains(&format!("{} must be a string", key)),
                    "key {} value {:?} produced err: {}",
                    key,
                    bad_value,
                    err
                );
            }
        }
    }
}
