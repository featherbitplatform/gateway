//! The `loki-logger` node — ships access logs to a Grafana Loki push API in
//! batches.
//!
//! Ports the APISIX `loki-logger` plugin onto featherbit's shared
//! [`BatchSink`](crate::batch::BatchSink). Each request builds a log entry and
//! hands it to the sink with a non-blocking `push`; a background task groups a
//! batch into a single Loki stream (the configured labels) and POSTs the
//! `{streams:[{stream, values:[[ns_ts, line], ...]}]}` payload to
//! `<endpoint>/loki/api/v1/push`. The node never mutates the request/response
//! and never fails, so it belongs in the response pipeline, **after the
//! upstream node**.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use bytes::Bytes;
use serde_json::{json, Map, Value};

use crate::batch::{BatchConfig, BatchFlusher, BatchSink, FlushError};
use crate::context::Context;
use crate::outbound::{OutboundClient, OutboundRequest};
use crate::plugins::resources::PluginResources;
use crate::plugins::util::log_entry::{build_entry, parse_log_format};
use crate::plugins::{Plugin, PluginOutput, PluginResult};

/// Builds the Loki push payload: one stream carrying `labels`, with one
/// `[ns_timestamp, json_line]` value per entry. Timestamps are the current
/// wall-clock time in nanoseconds (as a decimal string, per the Loki API).
/// Factored out of the network path for unit testing.
fn build_loki_payload(entries: &[Value], labels: &Map<String, Value>) -> Value {
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
        .to_string();
    let values: Vec<Value> = entries
        .iter()
        .map(|e| json!([ts, serde_json::to_string(e).unwrap_or_default()]))
        .collect();
    json!({
        "streams": [ { "stream": Value::Object(labels.clone()), "values": values } ]
    })
}

/// Delivers batches by POSTing the Loki streams payload to a push endpoint.
struct LokiLoggerFlusher {
    client: Arc<OutboundClient>,
    /// Fully-qualified push URLs (`endpoint_addr + endpoint_uri`).
    urls: Vec<String>,
    tenant_id: String,
    labels: Map<String, Value>,
    extra_headers: Vec<(String, String)>,
    ssl_verify: bool,
    timeout: Duration,
}

#[async_trait]
impl BatchFlusher for LokiLoggerFlusher {
    async fn flush(&self, entries: &[Value]) -> Result<(), FlushError> {
        let payload = build_loki_payload(entries, &self.labels);
        let body = Bytes::from(serde_json::to_vec(&payload).unwrap_or_default());

        // Pick an endpoint pseudo-randomly to spread load across replicas.
        let idx = (SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0) as usize)
            % self.urls.len().max(1);
        let url = self.urls.get(idx).cloned().unwrap_or_default();

        let mut headers = self.extra_headers.clone();
        headers.push(("X-Scope-OrgID".to_string(), self.tenant_id.clone()));
        headers.push(("Content-Type".to_string(), "application/json".to_string()));

        let req = OutboundRequest {
            method: http::Method::POST,
            url,
            headers,
            body,
            timeout: self.timeout,
            ssl_verify: self.ssl_verify,
        };
        match self.client.request(req).await {
            Ok(resp) if resp.status < 300 => Ok(()),
            Ok(resp) => Err(FlushError {
                message: format!("loki server returned status {}", resp.status),
                first_fail: None,
            }),
            Err(e) => Err(FlushError {
                message: e.to_string(),
                first_fail: None,
            }),
        }
    }
}

/// Batches access-log entries and POSTs them to a Grafana Loki push endpoint.
pub struct LokiLoggerPlugin {
    sink: BatchSink,
    log_format: Option<HashMap<String, Value>>,
    include_req_body: bool,
    include_resp_body: bool,
}

impl LokiLoggerPlugin {
    /// Builds the plugin from node config.
    ///
    /// Accepted keys:
    /// - `endpoint_addrs` (array of strings, **required**): Loki base
    ///   addresses (e.g. `http://loki:3100`); one is chosen per flush. A
    ///   single-string `endpoint` is also accepted. Empty/absent is an error.
    /// - `endpoint_uri` (string, default `/loki/api/v1/push`): push path
    ///   appended to each base address.
    /// - `tenant_id` (string, default `fake`): sent as the `X-Scope-OrgID`
    ///   header.
    /// - `log_labels` (object of string→string, default `{job: featherbit}`):
    ///   Loki stream labels. `labels` is accepted as an alias.
    /// - `headers` (object of string→string, optional): extra request headers.
    /// - `ssl_verify` (bool, default `false`): verify TLS certificates.
    /// - `timeout` (integer ms, default `3000`): whole-call deadline per flush.
    /// - `log_format`, `include_req_body`, `include_resp_body` — see the
    ///   shared log-entry builder.
    /// - Batch keys — see [`BatchConfig`].
    ///
    /// ```yaml
    /// type: loki-logger
    /// config:
    ///   endpoint_addrs: [http://loki:3100]
    ///   tenant_id: my-org
    ///   log_labels:
    ///     job: featherbit
    ///     env: prod
    /// ```
    pub fn from_config(
        config: &HashMap<String, Value>,
        resources: &Arc<PluginResources>,
    ) -> Result<Self, String> {
        let mut addrs: Vec<String> = Vec::new();
        if let Some(arr) = config.get("endpoint_addrs").and_then(|v| v.as_array()) {
            for v in arr {
                if let Some(s) = v.as_str().filter(|s| !s.is_empty()) {
                    addrs.push(s.trim_end_matches('/').to_string());
                }
            }
        }
        if let Some(s) = config
            .get("endpoint")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
        {
            addrs.push(s.trim_end_matches('/').to_string());
        }
        if addrs.is_empty() {
            return Err("loki-logger plugin requires 'endpoint_addrs'".to_string());
        }

        let endpoint_uri = config
            .get("endpoint_uri")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .unwrap_or("/loki/api/v1/push");
        let urls: Vec<String> = addrs
            .iter()
            .map(|a| format!("{}{}", a, endpoint_uri))
            .collect();

        let tenant_id = config
            .get("tenant_id")
            .and_then(|v| v.as_str())
            .unwrap_or("fake")
            .to_string();

        let labels = parse_labels(config);

        let mut extra_headers: Vec<(String, String)> = Vec::new();
        if let Some(obj) = config.get("headers").and_then(|v| v.as_object()) {
            for (k, v) in obj {
                if let Some(s) = v.as_str() {
                    extra_headers.push((k.clone(), s.to_string()));
                }
            }
        }

        let ssl_verify = config
            .get("ssl_verify")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let timeout = Duration::from_millis(
            config
                .get("timeout")
                .and_then(|v| v.as_u64())
                .unwrap_or(3000)
                .max(1),
        );

        let log_format = parse_log_format(config)?;
        let include_req_body = config
            .get("include_req_body")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let include_resp_body = config
            .get("include_resp_body")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        let batch_cfg = BatchConfig::from_config(config)?;
        let flusher = Arc::new(LokiLoggerFlusher {
            client: resources.outbound.clone(),
            urls,
            tenant_id,
            labels,
            extra_headers,
            ssl_verify,
            timeout,
        });
        let sink = BatchSink::spawn("loki-logger", batch_cfg, flusher);

        Ok(Self {
            sink,
            log_format,
            include_req_body,
            include_resp_body,
        })
    }
}

/// Reads `log_labels` (or `labels`) as a string map, defaulting to
/// `{job: featherbit}`.
fn parse_labels(config: &HashMap<String, Value>) -> Map<String, Value> {
    let src = config
        .get("log_labels")
        .or_else(|| config.get("labels"))
        .and_then(|v| v.as_object());
    match src {
        Some(obj) => {
            let mut m = Map::new();
            for (k, v) in obj {
                let val = match v {
                    Value::String(s) => s.clone(),
                    Value::Number(n) => n.to_string(),
                    Value::Bool(b) => b.to_string(),
                    _ => continue,
                };
                m.insert(k.clone(), Value::String(val));
            }
            if m.is_empty() {
                default_labels()
            } else {
                m
            }
        }
        None => default_labels(),
    }
}

fn default_labels() -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("job".to_string(), Value::String("featherbit".to_string()));
    m
}

#[async_trait]
impl Plugin for LokiLoggerPlugin {
    fn plugin_type(&self) -> &str {
        "loki-logger"
    }

    async fn execute(&self, ctx: Context, _named_inputs: &HashMap<String, Value>) -> PluginResult {
        let entry = build_entry(
            &ctx,
            self.log_format.as_ref(),
            self.include_req_body,
            self.include_resp_body,
        );
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

    #[test]
    fn rejects_missing_endpoint() {
        assert!(LokiLoggerPlugin::from_config(&HashMap::new(), &PluginResources::empty()).is_err());
    }

    #[tokio::test]
    async fn accepts_endpoint_addrs() {
        let c = cfg(json!({ "endpoint_addrs": ["http://loki:3100"] }));
        assert!(LokiLoggerPlugin::from_config(&c, &PluginResources::empty()).is_ok());
    }

    #[test]
    fn default_labels_are_featherbit() {
        let labels = parse_labels(&HashMap::new());
        assert_eq!(
            labels.get("job"),
            Some(&Value::String("featherbit".to_string()))
        );
    }

    #[test]
    fn parses_custom_labels() {
        let c = cfg(json!({ "log_labels": { "job": "gw", "env": "prod" } }));
        let labels = parse_labels(&c);
        assert_eq!(labels.get("job"), Some(&json!("gw")));
        assert_eq!(labels.get("env"), Some(&json!("prod")));
    }

    #[test]
    fn payload_has_one_stream_and_value_per_entry() {
        let mut labels = Map::new();
        labels.insert("job".to_string(), json!("featherbit"));
        let entries = vec![json!({"n": 1}), json!({"n": 2})];
        let payload = build_loki_payload(&entries, &labels);

        let streams = payload["streams"].as_array().unwrap();
        assert_eq!(streams.len(), 1);
        assert_eq!(streams[0]["stream"]["job"], "featherbit");

        let values = streams[0]["values"].as_array().unwrap();
        assert_eq!(values.len(), 2);
        // each value is [ts_string, json_line_string]
        assert!(values[0][0]
            .as_str()
            .unwrap()
            .chars()
            .all(|c| c.is_ascii_digit()));
        let line: Value = serde_json::from_str(values[1][1].as_str().unwrap()).unwrap();
        assert_eq!(line["n"], 2);
    }
}
