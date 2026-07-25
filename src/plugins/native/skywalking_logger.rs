//! The `skywalking-logger` node — ships access-log entries to a SkyWalking OAP
//! (Observability Analysis Platform) HTTP endpoint in batches.
//!
//! Ported from APISIX's `skywalking-logger.lua`. On the request path the node
//! builds a log entry (via the shared [`build_entry`](crate::plugins::util::log_entry::build_entry)),
//! wraps it in a SkyWalking `LogData` item, and hands it to a [`BatchSink`].
//! A background task POSTs the buffered items as a JSON array to
//! `<endpoint_addr>/v3/logs`; the node itself never blocks and passes the
//! context through unchanged.
//!
//! ## Deviations from APISIX
//! - The APISIX plugin parses the inbound `sw8` trace header to attach a
//!   `traceContext` to each item. featherbit does not thread trace propagation
//!   through the log entry, so `traceContext` is omitted.
//! - APISIX wraps the entry as `body.json.json`; we mirror that shape. A
//!   millisecond `timestamp` is added (SkyWalking's `LogData.timestamp`), which
//!   the APISIX plugin leaves to the OAP server to stamp.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{json, Value};

use crate::batch::{BatchConfig, BatchFlusher, BatchSink, FlushError};
use crate::context::Context;
use crate::outbound::{OutboundClient, OutboundRequest};
use crate::plugins::resources::PluginResources;
use crate::plugins::util::log_entry::{build_entry, parse_log_format};
use crate::plugins::{Plugin, PluginOutput, PluginResult};

/// Ships log entries to a SkyWalking OAP endpoint in batches.
pub struct SkywalkingLoggerPlugin {
    sink: BatchSink,
    log_format: Option<HashMap<String, Value>>,
    include_req_body: bool,
    include_resp_body: bool,
    service_name: String,
    service_instance_name: String,
}

/// Delivers batched SkyWalking `LogData` items to `<endpoint>/v3/logs`.
struct SkywalkingFlusher {
    client: Arc<OutboundClient>,
    url: String,
    timeout: Duration,
    ssl_verify: bool,
}

impl SkywalkingLoggerPlugin {
    /// Builds the plugin from node config.
    ///
    /// Config keys:
    ///
    /// | Key | Type | Default | Description |
    /// |---|---|---|---|
    /// | `endpoint_addr` | string | — (required) | SkyWalking OAP HTTP base, e.g. `http://127.0.0.1:12800`. |
    /// | `service_name` | string | `"featherbit"` | SkyWalking service name reported on each item. |
    /// | `service_instance_name` | string | `"featherbit Instance Name"` | SkyWalking service instance name. |
    /// | `ssl_verify` | bool | `true` | Verify the OAP TLS certificate. |
    /// | `timeout` | int (seconds) | `3` | Per-flush HTTP timeout. |
    /// | `log_format` | object | — | Custom `name -> "$var template"` entry; when set the default entry is replaced. |
    /// | `include_req_body` | bool | `false` | Include the (lossy UTF-8) request body in the default entry. |
    /// | `include_resp_body` | bool | `false` | Include the (lossy UTF-8) response body in the default entry. |
    ///
    /// Batch-processor keys (`batch_max_size`, `inactive_timeout`,
    /// `buffer_duration`, `max_retry_count`, `retry_delay`,
    /// `max_pending_entries`) are parsed by [`BatchConfig::from_config`].
    ///
    /// ```yaml
    /// type: skywalking-logger
    /// config:
    ///   endpoint_addr: http://127.0.0.1:12800
    ///   service_name: my-gateway
    ///   service_instance_name: gw-1
    /// ```
    pub fn from_config(
        config: &HashMap<String, Value>,
        resources: &Arc<PluginResources>,
    ) -> Result<Self, String> {
        let endpoint_addr = config
            .get("endpoint_addr")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .ok_or("skywalking-logger: `endpoint_addr` is required")?
            .trim_end_matches('/')
            .to_string();

        let service_name = config
            .get("service_name")
            .and_then(|v| v.as_str())
            .unwrap_or("featherbit")
            .to_string();
        let service_instance_name = config
            .get("service_instance_name")
            .and_then(|v| v.as_str())
            .unwrap_or("featherbit Instance Name")
            .to_string();
        let ssl_verify = config
            .get("ssl_verify")
            .and_then(|v| v.as_bool())
            .unwrap_or(true);
        let timeout =
            Duration::from_secs(config.get("timeout").and_then(|v| v.as_u64()).unwrap_or(3));

        let log_format = parse_log_format(config)?;
        let include_req_body = bool_key(config, "include_req_body");
        let include_resp_body = bool_key(config, "include_resp_body");

        let batch_cfg =
            BatchConfig::from_config(config).map_err(|e| format!("skywalking-logger: {e}"))?;

        let flusher = Arc::new(SkywalkingFlusher {
            client: resources.outbound.clone(),
            url: format!("{endpoint_addr}/v3/logs"),
            timeout,
            ssl_verify,
        });
        let sink = BatchSink::spawn("skywalking-logger", batch_cfg, flusher);

        Ok(Self {
            sink,
            log_format,
            include_req_body,
            include_resp_body,
            service_name,
            service_instance_name,
        })
    }
}

/// Builds one SkyWalking `LogData` item wrapping `entry` (JSON-encoded into
/// `body.json.json`, mirroring APISIX).
fn build_log_item(
    entry: &Value,
    service: &str,
    service_instance: &str,
    endpoint: &str,
    timestamp_ms: u64,
) -> Value {
    let entry_str = serde_json::to_string(entry).unwrap_or_else(|_| "{}".to_string());
    json!({
        "service": service,
        "serviceInstance": service_instance,
        "endpoint": endpoint,
        "timestamp": timestamp_ms,
        "body": { "json": { "json": entry_str } },
    })
}

fn bool_key(config: &HashMap<String, Value>, key: &str) -> bool {
    config.get(key).and_then(|v| v.as_bool()).unwrap_or(false)
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[async_trait]
impl BatchFlusher for SkywalkingFlusher {
    async fn flush(&self, entries: &[Value]) -> Result<(), FlushError> {
        let body = serde_json::to_vec(&Value::Array(entries.to_vec())).map_err(|e| FlushError {
            message: format!("failed to encode log batch: {e}"),
            first_fail: None,
        })?;

        let req = OutboundRequest {
            method: http::Method::POST,
            url: self.url.clone(),
            headers: vec![("Content-Type".to_string(), "application/json".to_string())],
            body: body.into(),
            timeout: self.timeout,
            ssl_verify: self.ssl_verify,
        };

        match self.client.request(req).await {
            Ok(resp) if resp.status < 400 => Ok(()),
            Ok(resp) => Err(FlushError {
                message: format!(
                    "skywalking OAP returned status {}: {}",
                    resp.status,
                    String::from_utf8_lossy(&resp.body)
                ),
                first_fail: None,
            }),
            Err(e) => Err(FlushError {
                message: e.to_string(),
                first_fail: None,
            }),
        }
    }
}

#[async_trait]
impl Plugin for SkywalkingLoggerPlugin {
    fn plugin_type(&self) -> &str {
        "skywalking-logger"
    }

    async fn execute(&self, ctx: Context, _named_inputs: &HashMap<String, Value>) -> PluginResult {
        let entry = build_entry(
            &ctx,
            self.log_format.as_ref(),
            self.include_req_body,
            self.include_resp_body,
        );
        let item = build_log_item(
            &entry,
            &self.service_name,
            &self.service_instance_name,
            &ctx.request.path,
            now_ms(),
        );
        self.sink.push(item);

        Ok(PluginOutput {
            context: ctx,
            named_outputs: HashMap::new(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(pairs: &[(&str, Value)]) -> HashMap<String, Value> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect()
    }

    #[test]
    fn from_config_requires_endpoint() {
        let res = SkywalkingLoggerPlugin::from_config(&HashMap::new(), &PluginResources::empty());
        let Err(e) = res else {
            panic!("expected error")
        };
        assert!(e.contains("endpoint_addr"));
    }

    #[tokio::test]
    async fn from_config_defaults() {
        let c = cfg(&[("endpoint_addr", json!("http://oap:12800/"))]);
        let p = SkywalkingLoggerPlugin::from_config(&c, &PluginResources::empty()).unwrap();
        assert_eq!(p.service_name, "featherbit");
        assert_eq!(p.service_instance_name, "featherbit Instance Name");
        assert!(!p.include_req_body);
    }

    #[test]
    fn log_item_shape() {
        let entry = json!({ "request": { "method": "GET" }, "response": { "status": 200 } });
        let item = build_log_item(&entry, "svc", "inst", "/api/x", 1_700_000_000_000);
        assert_eq!(item["service"], "svc");
        assert_eq!(item["serviceInstance"], "inst");
        assert_eq!(item["endpoint"], "/api/x");
        assert_eq!(item["timestamp"], 1_700_000_000_000u64);
        // The entry is embedded as a JSON *string* under body.json.json.
        let embedded = item["body"]["json"]["json"].as_str().unwrap();
        let reparsed: Value = serde_json::from_str(embedded).unwrap();
        assert_eq!(reparsed["request"]["method"], "GET");
        assert_eq!(reparsed["response"]["status"], 200);
    }
}
