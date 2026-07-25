//! The `http-logger` node — ships access logs to an arbitrary HTTP endpoint
//! in batches.
//!
//! Ports the APISIX `http-logger` plugin onto featherbit's shared
//! [`BatchSink`](crate::batch::BatchSink) and pooled outbound HTTP client.
//! Each request builds a log entry via
//! [`build_entry`](crate::plugins::util::log_entry::build_entry) and hands it
//! to the sink with a non-blocking, fire-and-forget `push`; a background task
//! POSTs accumulated batches to `uri`. The node never mutates the
//! request/response and never fails, so it belongs in the response pipeline,
//! **after the upstream node**, where the final status/body are available.

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

/// How batched entries are serialized into the request body.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConcatMethod {
    /// A single JSON array of entries (`application/json`).
    Json,
    /// One JSON object per line, `\n`-separated (`text/plain`).
    NewLine,
}

/// Serializes a batch into an HTTP body plus its `Content-Type`. Factored out
/// of the network path so payload shaping is unit-testable without a socket.
fn build_http_body(entries: &[Value], concat: ConcatMethod) -> (Bytes, &'static str) {
    match concat {
        ConcatMethod::Json => {
            let body = serde_json::to_vec(entries).unwrap_or_default();
            (Bytes::from(body), "application/json")
        }
        ConcatMethod::NewLine => {
            let lines: Vec<String> = entries
                .iter()
                .map(|e| serde_json::to_string(e).unwrap_or_default())
                .collect();
            (Bytes::from(lines.join("\n")), "text/plain")
        }
    }
}

/// Delivers batches by POSTing them to the configured HTTP endpoint.
struct HttpLoggerFlusher {
    client: Arc<OutboundClient>,
    uri: String,
    method: http::Method,
    /// Extra headers applied to every request (e.g. `Authorization`).
    headers: Vec<(String, String)>,
    concat: ConcatMethod,
    ssl_verify: bool,
    timeout: Duration,
}

#[async_trait]
impl BatchFlusher for HttpLoggerFlusher {
    async fn flush(&self, entries: &[Value]) -> Result<(), FlushError> {
        let (body, content_type) = build_http_body(entries, self.concat);
        let mut headers = self.headers.clone();
        headers.push(("Content-Type".to_string(), content_type.to_string()));

        let req = OutboundRequest {
            method: self.method.clone(),
            url: self.uri.clone(),
            headers,
            body,
            timeout: self.timeout,
            ssl_verify: self.ssl_verify,
        };
        match self.client.request(req).await {
            Ok(resp) if resp.status < 400 => Ok(()),
            Ok(resp) => Err(FlushError {
                message: format!("http-logger endpoint returned status {}", resp.status),
                first_fail: None,
            }),
            Err(e) => Err(FlushError {
                message: e.to_string(),
                first_fail: None,
            }),
        }
    }
}

/// Batches access-log entries and POSTs them to an HTTP endpoint.
pub struct HttpLoggerPlugin {
    sink: BatchSink,
    log_format: Option<HashMap<String, Value>>,
    include_req_body: bool,
    include_resp_body: bool,
}

impl HttpLoggerPlugin {
    /// Builds the plugin from node config.
    ///
    /// Accepted keys:
    /// - `uri` (string, **required**): the HTTP endpoint batches are POSTed to.
    ///   A missing/empty `uri` is a config-load error.
    /// - `method` (string, default `POST`): HTTP method for the callout.
    /// - `headers` (object of string→string, optional): extra request headers
    ///   applied to every batch (e.g. custom auth).
    /// - `auth_header` (string, optional): convenience for a single
    ///   `Authorization` header value.
    /// - `concat_method` (string, default `json`): `json` sends a JSON array
    ///   (`application/json`); `new_line` sends `\n`-separated JSON objects
    ///   (`text/plain`).
    /// - `ssl_verify` (bool, default `false`): verify TLS certificates for
    ///   `https` endpoints.
    /// - `timeout` (integer seconds, default `3`): whole-call deadline per flush.
    /// - `log_format` (object, optional): custom flat entry of
    ///   `name -> "$var template"`; when absent the default structured entry is
    ///   used.
    /// - `include_req_body` / `include_resp_body` (bool, default `false`): add
    ///   the request/response body to the default entry.
    /// - Batch keys (`batch_max_size`, `inactive_timeout`, `buffer_duration`,
    ///   `max_retry_count`, `retry_delay`, `max_pending_entries`) — see
    ///   [`BatchConfig`].
    ///
    /// ```yaml
    /// type: http-logger
    /// config:
    ///   uri: http://log-collector:3000/logs
    ///   method: POST
    ///   headers:
    ///     X-Token: secret
    ///   concat_method: json
    ///   batch_max_size: 1000
    ///   inactive_timeout: 5
    /// ```
    pub fn from_config(
        config: &HashMap<String, Value>,
        resources: &Arc<PluginResources>,
    ) -> Result<Self, String> {
        let uri = config
            .get("uri")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| "http-logger plugin requires 'uri'".to_string())?
            .to_string();

        let method_str = config
            .get("method")
            .and_then(|v| v.as_str())
            .unwrap_or("POST")
            .to_uppercase();
        let method = http::Method::from_bytes(method_str.as_bytes())
            .map_err(|_| format!("http-logger invalid method '{}'", method_str))?;

        let concat = match config
            .get("concat_method")
            .and_then(|v| v.as_str())
            .unwrap_or("json")
        {
            "json" => ConcatMethod::Json,
            "new_line" => ConcatMethod::NewLine,
            other => {
                return Err(format!(
                    "http-logger concat_method must be 'json' or 'new_line', got '{}'",
                    other
                ))
            }
        };

        let mut headers: Vec<(String, String)> = Vec::new();
        if let Some(obj) = config.get("headers").and_then(|v| v.as_object()) {
            for (k, v) in obj {
                if let Some(s) = header_value(v) {
                    headers.push((k.clone(), s));
                }
            }
        }
        if let Some(auth) = config.get("auth_header").and_then(|v| v.as_str()) {
            if !auth.is_empty() {
                headers.push(("Authorization".to_string(), auth.to_string()));
            }
        }

        let ssl_verify = config
            .get("ssl_verify")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let timeout = Duration::from_secs(
            config
                .get("timeout")
                .and_then(|v| v.as_u64())
                .unwrap_or(3)
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
        let flusher = Arc::new(HttpLoggerFlusher {
            client: resources.outbound.clone(),
            uri,
            method,
            headers,
            concat,
            ssl_verify,
            timeout,
        });
        let sink = BatchSink::spawn("http-logger", batch_cfg, flusher);

        Ok(Self {
            sink,
            log_format,
            include_req_body,
            include_resp_body,
        })
    }
}

/// Coerces a JSON scalar to a header string; `None` for non-scalars.
fn header_value(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

#[async_trait]
impl Plugin for HttpLoggerPlugin {
    fn plugin_type(&self) -> &str {
        "http-logger"
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
    fn rejects_missing_uri() {
        assert!(HttpLoggerPlugin::from_config(&HashMap::new(), &PluginResources::empty()).is_err());
        let c = cfg(json!({ "uri": "" }));
        assert!(HttpLoggerPlugin::from_config(&c, &PluginResources::empty()).is_err());
    }

    #[tokio::test]
    async fn accepts_minimal_config() {
        let c = cfg(json!({ "uri": "http://collector/logs" }));
        assert!(HttpLoggerPlugin::from_config(&c, &PluginResources::empty()).is_ok());
    }

    #[test]
    fn rejects_bad_concat_method() {
        let c = cfg(json!({ "uri": "http://c", "concat_method": "csv" }));
        assert!(HttpLoggerPlugin::from_config(&c, &PluginResources::empty()).is_err());
    }

    #[test]
    fn json_body_is_an_array() {
        let entries = vec![json!({"a": 1}), json!({"a": 2})];
        let (body, ct) = build_http_body(&entries, ConcatMethod::Json);
        assert_eq!(ct, "application/json");
        let parsed: Value = serde_json::from_slice(&body).unwrap();
        assert!(parsed.is_array());
        assert_eq!(parsed.as_array().unwrap().len(), 2);
        assert_eq!(parsed[1]["a"], 2);
    }

    #[test]
    fn new_line_body_is_newline_delimited() {
        let entries = vec![json!({"a": 1}), json!({"a": 2})];
        let (body, ct) = build_http_body(&entries, ConcatMethod::NewLine);
        assert_eq!(ct, "text/plain");
        let text = String::from_utf8(body.to_vec()).unwrap();
        let lines: Vec<&str> = text.split('\n').collect();
        assert_eq!(lines.len(), 2);
        let first: Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(first["a"], 1);
    }
}
