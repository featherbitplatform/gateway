//! The `splunk-hec-logging` node — ships access logs to a Splunk HTTP Event
//! Collector (HEC) in batches.
//!
//! Ports the APISIX `splunk-hec-logging` plugin onto featherbit's shared
//! [`BatchSink`](crate::batch::BatchSink). Each request builds a log entry,
//! wraps it in a Splunk HEC event envelope (`{time, source, sourcetype,
//! event}`), and hands it to the sink with a non-blocking `push`; a background
//! task POSTs the concatenated events to the HEC `uri` with an
//! `Authorization: Splunk <token>` header. The node never mutates the
//! request/response and never fails, so it belongs in the response pipeline,
//! **after the upstream node**.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use bytes::Bytes;
use serde_json::{json, Value};

use crate::batch::{BatchConfig, BatchFlusher, BatchSink, FlushError};
use crate::context::Context;
use crate::outbound::{OutboundClient, OutboundRequest};
use crate::plugins::resources::PluginResources;
use crate::plugins::util::log_entry::{build_entry, parse_log_format};
use crate::plugins::{Plugin, PluginOutput, PluginResult};

const DEFAULT_SOURCE: &str = "featherbit-splunk-hec-logging";
const DEFAULT_SOURCETYPE: &str = "_json";

/// Wraps one log entry in a Splunk HEC event envelope with a shared timestamp.
fn wrap_event(entry: &Value, time: f64, source: &str) -> Value {
    json!({
        "time": time,
        "source": source,
        "sourcetype": DEFAULT_SOURCETYPE,
        "event": entry,
    })
}

/// Serializes a batch into the HEC request body: HEC accepts multiple JSON
/// event objects concatenated with no separator. Factored out for unit testing.
fn build_splunk_body(entries: &[Value], time: f64, source: &str) -> Bytes {
    let mut buf = String::new();
    for e in entries {
        let event = wrap_event(e, time, source);
        buf.push_str(&serde_json::to_string(&event).unwrap_or_default());
    }
    Bytes::from(buf)
}

/// Delivers batches by POSTing HEC events to the Splunk endpoint.
struct SplunkFlusher {
    client: Arc<OutboundClient>,
    uri: String,
    token: String,
    channel: Option<String>,
    source: String,
    ssl_verify: bool,
    timeout: Duration,
}

#[async_trait]
impl BatchFlusher for SplunkFlusher {
    async fn flush(&self, entries: &[Value]) -> Result<(), FlushError> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs_f64())
            .unwrap_or(0.0);
        let body = build_splunk_body(entries, now, &self.source);

        let mut headers = vec![
            ("Content-Type".to_string(), "application/json".to_string()),
            (
                "Authorization".to_string(),
                format!("Splunk {}", self.token),
            ),
        ];
        if let Some(ch) = &self.channel {
            headers.push(("X-Splunk-Request-Channel".to_string(), ch.clone()));
        }

        let req = OutboundRequest {
            method: http::Method::POST,
            url: self.uri.clone(),
            headers,
            body,
            timeout: self.timeout,
            ssl_verify: self.ssl_verify,
        };
        match self.client.request(req).await {
            Ok(resp) if resp.status == 200 => Ok(()),
            Ok(resp) => Err(FlushError {
                message: format!("splunk HEC returned status {}", resp.status),
                first_fail: None,
            }),
            Err(e) => Err(FlushError {
                message: e.to_string(),
                first_fail: None,
            }),
        }
    }
}

/// Batches access-log entries and POSTs them to a Splunk HEC endpoint.
pub struct SplunkHecLoggingPlugin {
    sink: BatchSink,
    log_format: Option<HashMap<String, Value>>,
    include_req_body: bool,
    include_resp_body: bool,
}

impl SplunkHecLoggingPlugin {
    /// Builds the plugin from node config.
    ///
    /// Accepted keys (the APISIX `endpoint` sub-object is flattened; both
    /// `endpoint.uri` and a top-level `uri` are accepted):
    /// - `endpoint.uri` (string, **required**): the HEC collector URL, e.g.
    ///   `https://splunk:8088/services/collector`.
    /// - `endpoint.token` (string, **required**): HEC token, sent as
    ///   `Authorization: Splunk <token>`.
    /// - `endpoint.channel` (string, optional): sent as the
    ///   `X-Splunk-Request-Channel` header.
    /// - `endpoint.timeout` (integer seconds, default `10`): whole-call
    ///   deadline per flush.
    /// - `source` (string, default `featherbit-splunk-hec-logging`): HEC event
    ///   `source` field.
    /// - `ssl_verify` (bool, default `true`): verify TLS certificates.
    /// - `log_format`, `include_req_body`, `include_resp_body` — see the shared
    ///   log-entry builder.
    /// - Batch keys — see [`BatchConfig`].
    ///
    /// ```yaml
    /// type: splunk-hec-logging
    /// config:
    ///   endpoint:
    ///     uri: https://splunk:8088/services/collector
    ///     token: 00000000-0000-0000-0000-000000000000
    ///   ssl_verify: true
    ///   batch_max_size: 1000
    /// ```
    pub fn from_config(
        config: &HashMap<String, Value>,
        resources: &Arc<PluginResources>,
    ) -> Result<Self, String> {
        let endpoint = config.get("endpoint").and_then(|v| v.as_object());

        let get_ep = |key: &str| -> Option<&Value> {
            endpoint
                .and_then(|e| e.get(key))
                .or_else(|| config.get(key))
        };

        let uri = get_ep("uri")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| "splunk-hec-logging plugin requires 'endpoint.uri'".to_string())?
            .to_string();
        let token = get_ep("token")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| "splunk-hec-logging plugin requires 'endpoint.token'".to_string())?
            .to_string();
        let channel = get_ep("channel")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string());
        let timeout = Duration::from_secs(
            get_ep("timeout")
                .and_then(|v| v.as_u64())
                .unwrap_or(10)
                .max(1),
        );

        let source = config
            .get("source")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .unwrap_or(DEFAULT_SOURCE)
            .to_string();

        let ssl_verify = config
            .get("ssl_verify")
            .and_then(|v| v.as_bool())
            .unwrap_or(true);

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
        let flusher = Arc::new(SplunkFlusher {
            client: resources.outbound.clone(),
            uri,
            token,
            channel,
            source,
            ssl_verify,
            timeout,
        });
        let sink = BatchSink::spawn("splunk-hec-logging", batch_cfg, flusher);

        Ok(Self {
            sink,
            log_format,
            include_req_body,
            include_resp_body,
        })
    }
}

#[async_trait]
impl Plugin for SplunkHecLoggingPlugin {
    fn plugin_type(&self) -> &str {
        "splunk-hec-logging"
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
    fn rejects_missing_uri_or_token() {
        // no endpoint at all
        assert!(
            SplunkHecLoggingPlugin::from_config(&HashMap::new(), &PluginResources::empty())
                .is_err()
        );
        // uri without token
        let c = cfg(json!({ "endpoint": { "uri": "https://splunk:8088/x" } }));
        assert!(SplunkHecLoggingPlugin::from_config(&c, &PluginResources::empty()).is_err());
    }

    #[tokio::test]
    async fn accepts_uri_and_token() {
        let c = cfg(json!({ "endpoint": { "uri": "https://splunk:8088/x", "token": "tok" } }));
        assert!(SplunkHecLoggingPlugin::from_config(&c, &PluginResources::empty()).is_ok());
    }

    #[test]
    fn event_envelope_wraps_entry() {
        let entry = json!({ "request": { "method": "GET" } });
        let ev = wrap_event(&entry, 1.5, "src");
        assert_eq!(ev["source"], "src");
        assert_eq!(ev["sourcetype"], "_json");
        assert_eq!(ev["time"], 1.5);
        assert_eq!(ev["event"]["request"]["method"], "GET");
    }

    #[test]
    fn body_concatenates_events_without_separator() {
        let entries = vec![json!({"a": 1}), json!({"a": 2})];
        let body = build_splunk_body(&entries, 0.0, DEFAULT_SOURCE);
        let text = String::from_utf8(body.to_vec()).unwrap();
        // Two JSON objects back-to-back: "}{"  boundary, no comma/newline.
        assert!(text.contains("}{"));
        assert!(!text.contains("},{"));
        assert_eq!(text.matches("\"event\"").count(), 2);
    }
}
