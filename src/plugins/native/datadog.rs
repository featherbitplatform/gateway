//! The `datadog` node — ships request metrics to a Datadog agent over
//! DogStatsD (UDP) in batches.
//!
//! **Deviation from the other loggers:** APISIX's `datadog` plugin does *not*
//! ship JSON access logs to an HTTP endpoint. It emits DogStatsD metrics
//! (`request.counter`, `request.latency`, `ingress.size`, `egress.size`, ...)
//! as UDP datagrams to a local Datadog agent. This port stays faithful: the
//! flusher is a [`tokio::net::UdpSocket`] rather than the shared outbound HTTP
//! client, and each buffered log entry is rendered into DogStatsD metric lines.
//! The APISIX `plugin-metadata` fields (`host`, `port`, `namespace`,
//! `constant_tags`) are flattened into this node's config. Because featherbit's
//! shared log entry carries no route/service identifiers, `prefer_name` is
//! accepted for compatibility but has no effect, and no `route_name`/
//! `service_name` tags are emitted.
//!
//! The node never mutates the request/response and never fails; place it in the
//! response pipeline, **after the upstream node**.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;
use tokio::net::UdpSocket;

use crate::batch::{BatchConfig, BatchFlusher, BatchSink, FlushError};
use crate::context::Context;
use crate::plugins::resources::PluginResources;
use crate::plugins::util::log_entry::build_entry;
use crate::plugins::{Plugin, PluginOutput, PluginResult};

/// DogStatsD agent's default receive buffer; larger datagrams are truncated,
/// so coalesced payloads are capped here (matching APISIX).
const MAX_DATAGRAM_SIZE: usize = 8192;

/// Builds the DogStatsD metric lines for one featherbit log entry.
///
/// Rendered as `namespace.metric:value|type|#tag,tag`. Factored out of the
/// socket path so metric formatting is unit-testable.
fn build_metric_lines(
    entry: &Value,
    namespace: &str,
    constant_tags: &[String],
    include_path: bool,
    include_method: bool,
) -> Vec<String> {
    let prefix = if namespace.is_empty() {
        String::new()
    } else {
        format!("{}.", namespace)
    };

    // Assemble tags.
    let mut tags: Vec<String> = constant_tags.to_vec();
    if include_method {
        if let Some(m) = entry.pointer("/request/method").and_then(|v| v.as_str()) {
            tags.push(format!("method:{}", m));
        }
    }
    if include_path {
        if let Some(p) = entry.pointer("/request/uri").and_then(|v| v.as_str()) {
            tags.push(format!("path:{}", p));
        }
    }
    if let Some(status) = entry.pointer("/response/status").and_then(|v| v.as_u64()) {
        tags.push(format!("response_status:{}", status));
        tags.push(format!("response_status_class:{}xx", status / 100));
    }
    if let Some(scheme) = entry.pointer("/request/scheme").and_then(|v| v.as_str()) {
        tags.push(format!("scheme:{}", scheme));
    }
    let suffix = if tags.is_empty() {
        String::new()
    } else {
        format!("|#{}", tags.join(","))
    };

    let mut lines = vec![format!("{}request.counter:1|c{}", prefix, suffix)];
    if let Some(latency) = entry.get("latency").and_then(|v| v.as_u64()) {
        lines.push(format!("{}request.latency:{}|h{}", prefix, latency, suffix));
    }
    if let Some(size) = entry.pointer("/request/size").and_then(|v| v.as_u64()) {
        lines.push(format!("{}ingress.size:{}|ms{}", prefix, size, suffix));
    }
    if let Some(size) = entry.pointer("/response/size").and_then(|v| v.as_u64()) {
        lines.push(format!("{}egress.size:{}|ms{}", prefix, size, suffix));
    }
    lines
}

/// Delivers batches by sending DogStatsD datagrams to the agent over UDP.
struct DatadogFlusher {
    addr: String,
    namespace: String,
    constant_tags: Vec<String>,
    include_path: bool,
    include_method: bool,
}

#[async_trait]
impl BatchFlusher for DatadogFlusher {
    async fn flush(&self, entries: &[Value]) -> Result<(), FlushError> {
        let sock = UdpSocket::bind("0.0.0.0:0").await.map_err(|e| FlushError {
            message: format!("udp bind failed: {}", e),
            first_fail: None,
        })?;
        sock.connect(&self.addr).await.map_err(|e| FlushError {
            message: format!("udp connect to {} failed: {}", self.addr, e),
            first_fail: None,
        })?;

        for (i, entry) in entries.iter().enumerate() {
            let lines = build_metric_lines(
                entry,
                &self.namespace,
                &self.constant_tags,
                self.include_path,
                self.include_method,
            );
            let payload = lines.join("\n");
            let send_res = if payload.len() <= MAX_DATAGRAM_SIZE {
                sock.send(payload.as_bytes()).await.map(|_| ())
            } else {
                // Too large to coalesce: one datagram per metric line.
                let mut r = Ok(());
                for line in &lines {
                    if let Err(e) = sock.send(line.as_bytes()).await {
                        r = Err(e);
                        break;
                    }
                }
                r
            };
            if let Err(e) = send_res {
                // Entries before `i` were already delivered; retry only the tail.
                return Err(FlushError {
                    message: format!("failed to send metrics to {}: {}", self.addr, e),
                    first_fail: Some(i),
                });
            }
        }
        Ok(())
    }
}

/// Batches request metrics and sends them to a Datadog agent via DogStatsD.
pub struct DatadogPlugin {
    sink: BatchSink,
    include_path: bool,
    include_method: bool,
}

impl DatadogPlugin {
    /// Builds the plugin from node config.
    ///
    /// Accepted keys (APISIX `plugin-metadata` fields flattened in):
    /// - `host` (string, default `127.0.0.1`): Datadog agent address.
    /// - `port` (integer, default `8125`): DogStatsD UDP port.
    /// - `namespace` (string, default `featherbit`): metric name prefix.
    /// - `constant_tags` (array of strings, default `[]`): tags added to every
    ///   metric.
    /// - `include_path` (bool, default `false`): add a `path:` tag.
    /// - `include_method` (bool, default `false`): add a `method:` tag.
    /// - `prefer_name` (bool, default `true`): accepted for APISIX
    ///   compatibility but has no effect here (no route/service names in the
    ///   log entry).
    /// - Batch keys — see [`BatchConfig`].
    ///
    /// ```yaml
    /// type: datadog
    /// config:
    ///   host: 127.0.0.1
    ///   port: 8125
    ///   namespace: featherbit
    ///   constant_tags: [source:featherbit]
    ///   include_method: true
    /// ```
    pub fn from_config(
        config: &HashMap<String, Value>,
        _resources: &Arc<PluginResources>,
    ) -> Result<Self, String> {
        let host = config
            .get("host")
            .and_then(|v| v.as_str())
            .unwrap_or("127.0.0.1");
        let port = config.get("port").and_then(|v| v.as_u64()).unwrap_or(8125);
        if port > 65535 {
            return Err(format!("datadog port out of range: {}", port));
        }
        let addr = format!("{}:{}", host, port);

        let namespace = config
            .get("namespace")
            .and_then(|v| v.as_str())
            .unwrap_or("featherbit")
            .to_string();

        let constant_tags: Vec<String> = config
            .get("constant_tags")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();

        let include_path = config
            .get("include_path")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let include_method = config
            .get("include_method")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        // datadog batches at 5000 by default in APISIX; the shared BatchConfig
        // default of 1000 is used unless overridden.
        let batch_cfg = BatchConfig::from_config(config)?;
        let flusher = Arc::new(DatadogFlusher {
            addr,
            namespace,
            constant_tags,
            include_path,
            include_method,
        });
        let sink = BatchSink::spawn("datadog", batch_cfg, flusher);

        Ok(Self {
            sink,
            include_path,
            include_method,
        })
    }
}

#[async_trait]
impl Plugin for DatadogPlugin {
    fn plugin_type(&self) -> &str {
        "datadog"
    }

    async fn execute(&self, ctx: Context, _named_inputs: &HashMap<String, Value>) -> PluginResult {
        // Datadog derives metrics from the default structured entry; it does
        // not support log_format or body capture.
        let _ = (self.include_path, self.include_method);
        let entry = build_entry(&ctx, None, false, false);
        self.sink.push(entry);
        Ok(PluginOutput {
            context: ctx,
            named_outputs: HashMap::new(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn cfg(v: Value) -> HashMap<String, Value> {
        serde_json::from_value(v).unwrap()
    }

    fn entry() -> Value {
        json!({
            "request": { "method": "GET", "uri": "/api", "size": 12, "scheme": "https" },
            "response": { "status": 200, "size": 34 },
            "latency": 7
        })
    }

    #[tokio::test]
    async fn accepts_empty_config_with_defaults() {
        let p = DatadogPlugin::from_config(&HashMap::new(), &PluginResources::empty());
        assert!(p.is_ok());
    }

    #[test]
    fn rejects_bad_port() {
        let c = cfg(json!({ "port": 70000 }));
        assert!(DatadogPlugin::from_config(&c, &PluginResources::empty()).is_err());
    }

    #[test]
    fn metric_lines_cover_counter_latency_and_sizes() {
        let lines = build_metric_lines(
            &entry(),
            "featherbit",
            &["source:fb".to_string()],
            true,
            true,
        );
        let joined = lines.join("\n");
        assert!(joined.contains("featherbit.request.counter:1|c"));
        assert!(joined.contains("featherbit.request.latency:7|h"));
        assert!(joined.contains("featherbit.ingress.size:12|ms"));
        assert!(joined.contains("featherbit.egress.size:34|ms"));
        // tags
        assert!(joined.contains("source:fb"));
        assert!(joined.contains("method:GET"));
        assert!(joined.contains("path:/api"));
        assert!(joined.contains("response_status:200"));
        assert!(joined.contains("response_status_class:2xx"));
        assert!(joined.contains("scheme:https"));
    }

    #[test]
    fn empty_namespace_omits_prefix() {
        let lines = build_metric_lines(&entry(), "", &[], false, false);
        assert!(lines[0].starts_with("request.counter:1|c"));
        // no method/path tags when disabled
        assert!(!lines[0].contains("method:"));
        assert!(!lines[0].contains("path:"));
    }
}
