//! The `loggly` node — ships access logs to SolarWinds Loggly in batches via
//! the HTTP/S bulk endpoint.
//!
//! Ports the APISIX `loggly` plugin onto featherbit's shared
//! [`BatchSink`](crate::batch::BatchSink) and pooled outbound HTTP client.
//! Each request builds a log entry and hands it to the sink with a
//! non-blocking `push`; a background task POSTs the batch as newline-delimited
//! JSON to `https://<host>/bulk/<customer_token>/tag/<tags>/`.
//!
//! **Deviation:** APISIX's `loggly` defaults to a syslog-over-UDP transport and
//! only uses the HTTP bulk endpoint when its `plugin-metadata.protocol` is
//! `http`/`https`. This port implements the HTTP/S bulk path only (the shape
//! matching the other featherbit loggers); the RFC5424 syslog framing,
//! `severity`/`severity_map`, and UDP transport are not implemented. `severity`
//! is accepted for compatibility but ignored.
//!
//! The node never mutates the request/response and never fails; place it in the
//! response pipeline, **after the upstream node**.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use serde_json::Value;

use crate::batch::{BatchConfig, BatchFlusher, BatchSink, FlushError};
use crate::context::Context;
use crate::outbound::{OutboundClient, OutboundRequest};
use crate::plugins::resources::PluginResources;
use crate::plugins::util::log_entry::{build_entry, parse_log_format};
use crate::plugins::{Plugin, PluginOutput, PluginResult};

const DEFAULT_HOST: &str = "logs-01.loggly.com";

/// Builds the Loggly bulk endpoint URL for a token and tag set.
fn build_bulk_url(host: &str, token: &str, tags: &[String]) -> String {
    let base = if host.starts_with("http://") || host.starts_with("https://") {
        host.trim_end_matches('/').to_string()
    } else {
        format!("https://{}", host.trim_end_matches('/'))
    };
    let tag_seg = if tags.is_empty() {
        "featherbit".to_string()
    } else {
        tags.join(",")
    };
    format!("{}/bulk/{}/tag/{}/", base, token, tag_seg)
}

/// Serializes a batch as newline-delimited JSON, the format the Loggly bulk
/// endpoint expects. Factored out for unit testing.
fn build_bulk_body(entries: &[Value]) -> Bytes {
    let lines: Vec<String> = entries
        .iter()
        .map(|e| serde_json::to_string(e).unwrap_or_default())
        .collect();
    Bytes::from(lines.join("\n"))
}

/// Delivers batches by POSTing newline-delimited JSON to the Loggly bulk API.
struct LogglyFlusher {
    client: Arc<OutboundClient>,
    url: String,
    tags_header: String,
    ssl_verify: bool,
    timeout: Duration,
}

#[async_trait]
impl BatchFlusher for LogglyFlusher {
    async fn flush(&self, entries: &[Value]) -> Result<(), FlushError> {
        let body = build_bulk_body(entries);
        let headers = vec![
            ("Content-Type".to_string(), "application/json".to_string()),
            ("X-LOGGLY-TAG".to_string(), self.tags_header.clone()),
        ];
        let req = OutboundRequest {
            method: http::Method::POST,
            url: self.url.clone(),
            headers,
            body,
            timeout: self.timeout,
            ssl_verify: self.ssl_verify,
        };
        match self.client.request(req).await {
            Ok(resp) if resp.status == 200 => Ok(()),
            Ok(resp) => Err(FlushError {
                message: format!("loggly returned status {}", resp.status),
                first_fail: None,
            }),
            Err(e) => Err(FlushError {
                message: e.to_string(),
                first_fail: None,
            }),
        }
    }
}

/// Batches access-log entries and POSTs them to the Loggly bulk endpoint.
pub struct LogglyPlugin {
    sink: BatchSink,
    log_format: Option<HashMap<String, Value>>,
    include_req_body: bool,
    include_resp_body: bool,
}

impl LogglyPlugin {
    /// Builds the plugin from node config.
    ///
    /// Accepted keys:
    /// - `customer_token` (string, **required**): Loggly customer token; forms
    ///   part of the bulk URL. Missing/empty is a config-load error.
    /// - `tags` (array of strings, default `[featherbit]`): Loggly tags,
    ///   comma-joined into the URL and the `X-LOGGLY-TAG` header.
    /// - `host` (string, default `logs-01.loggly.com`): Loggly host; a bare
    ///   host gets an `https://` scheme.
    /// - `severity` (string, default `INFO`): accepted for APISIX
    ///   compatibility but ignored (HTTP bulk mode carries no syslog severity).
    /// - `ssl_verify` (bool, default `true`): verify TLS certificates.
    /// - `timeout` (integer ms, default `5000`): whole-call deadline per flush.
    /// - `log_format`, `include_req_body`, `include_resp_body` — see the shared
    ///   log-entry builder.
    /// - Batch keys — see [`BatchConfig`].
    ///
    /// ```yaml
    /// type: loggly
    /// config:
    ///   customer_token: 00000000-0000-0000-0000-000000000000
    ///   tags: [featherbit, prod]
    ///   ssl_verify: true
    /// ```
    pub fn from_config(
        config: &HashMap<String, Value>,
        resources: &Arc<PluginResources>,
    ) -> Result<Self, String> {
        let token = config
            .get("customer_token")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| "loggly plugin requires 'customer_token'".to_string())?
            .to_string();

        let tags: Vec<String> = config
            .get("tags")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_else(|| vec!["featherbit".to_string()]);

        let host = config
            .get("host")
            .and_then(|v| v.as_str())
            .unwrap_or(DEFAULT_HOST);
        let url = build_bulk_url(host, &token, &tags);
        let tags_header = if tags.is_empty() {
            "featherbit".to_string()
        } else {
            tags.join(",")
        };

        let ssl_verify = config
            .get("ssl_verify")
            .and_then(|v| v.as_bool())
            .unwrap_or(true);
        let timeout = Duration::from_millis(
            config
                .get("timeout")
                .and_then(|v| v.as_u64())
                .unwrap_or(5000)
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
        let flusher = Arc::new(LogglyFlusher {
            client: resources.outbound.clone(),
            url,
            tags_header,
            ssl_verify,
            timeout,
        });
        let sink = BatchSink::spawn("loggly", batch_cfg, flusher);

        Ok(Self {
            sink,
            log_format,
            include_req_body,
            include_resp_body,
        })
    }
}

#[async_trait]
impl Plugin for LogglyPlugin {
    fn plugin_type(&self) -> &str {
        "loggly"
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
    fn rejects_missing_token() {
        assert!(LogglyPlugin::from_config(&HashMap::new(), &PluginResources::empty()).is_err());
        let c = cfg(json!({ "customer_token": "" }));
        assert!(LogglyPlugin::from_config(&c, &PluginResources::empty()).is_err());
    }

    #[tokio::test]
    async fn accepts_token() {
        let c = cfg(json!({ "customer_token": "tok" }));
        assert!(LogglyPlugin::from_config(&c, &PluginResources::empty()).is_ok());
    }

    #[test]
    fn bulk_url_defaults_to_https_host() {
        let url = build_bulk_url(DEFAULT_HOST, "tok", &["a".to_string(), "b".to_string()]);
        assert_eq!(url, "https://logs-01.loggly.com/bulk/tok/tag/a,b/");
    }

    #[test]
    fn bulk_url_respects_explicit_scheme() {
        let url = build_bulk_url("http://loggly.local", "tok", &["x".to_string()]);
        assert_eq!(url, "http://loggly.local/bulk/tok/tag/x/");
    }

    #[test]
    fn bulk_body_is_newline_delimited_json() {
        let entries = vec![json!({"a": 1}), json!({"a": 2})];
        let body = build_bulk_body(&entries);
        let text = String::from_utf8(body.to_vec()).unwrap();
        let lines: Vec<&str> = text.split('\n').collect();
        assert_eq!(lines.len(), 2);
        let second: Value = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(second["a"], 2);
    }
}
