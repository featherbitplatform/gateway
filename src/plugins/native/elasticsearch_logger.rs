//! Elasticsearch access-logger (`elasticsearch-logger`).
//!
//! Ships access-log entries to Elasticsearch via its [`_bulk`] API. Each
//! request builds one log entry (the shared [`build_entry`] shape, or a custom
//! `log_format`) that is handed to a fire-and-forget [`BatchSink`]; a
//! background task POSTs batches as newline-delimited JSON (NDJSON) to
//! `<endpoint>/_bulk`.
//!
//! Ports APISIX's `elasticsearch-logger`: the bulk envelope pairs an
//! `{"index":{"_index":<name>}}` action line with each entry line, exactly as
//! `get_logger_entry` does in the Lua plugin.
//!
//! ## Deviations from APISIX
//!
//! - **No Elasticsearch version probe.** APISIX issues a `GET /` to detect the
//!   ES major version and, for ES 5/6, adds `_type: "_doc"` to the action
//!   line. featherbit targets ES 7+ and never emits `_type`, so it performs no
//!   version-probe callout.
//! - **Static index name.** APISIX resolves `{time}` strftime tokens and
//!   `$var` references in `field.index` per request against `ctx.var`. Because
//!   entries are flushed in batches without a request context, featherbit uses
//!   `field.index` as a literal string.
//!
//! [`_bulk`]: https://www.elastic.co/guide/en/elasticsearch/reference/current/docs-bulk.html
//! [`build_entry`]: crate::plugins::util::log_entry::build_entry

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use base64::{engine::general_purpose::STANDARD, Engine};
use bytes::Bytes;
use serde_json::Value;

use crate::batch::{BatchConfig, BatchFlusher, BatchSink, FlushError};
use crate::context::Context;
use crate::outbound::{OutboundClient, OutboundRequest};
use crate::plugins::resources::PluginResources;
use crate::plugins::util::log_entry::{build_entry, parse_log_format};
use crate::plugins::{Plugin, PluginOutput, PluginResult};

/// Node that batches access-log entries and ships them to Elasticsearch.
pub struct ElasticsearchLoggerPlugin {
    sink: BatchSink,
    log_format: Option<HashMap<String, Value>>,
    include_req_body: bool,
    include_resp_body: bool,
}

/// Delivers batches to Elasticsearch's `_bulk` endpoint.
struct EsFlusher {
    client: Arc<OutboundClient>,
    /// Endpoint base URLs (no trailing slash); one is chosen per flush.
    endpoints: Vec<String>,
    /// Rotating cursor for endpoint selection across flushes.
    cursor: AtomicUsize,
    /// Destination index name (`field.index`).
    index: String,
    /// Pre-built `Basic <base64>` header value, when auth is configured.
    authorization: Option<String>,
    ssl_verify: bool,
    timeout: Duration,
}

impl ElasticsearchLoggerPlugin {
    /// Builds the plugin from node config.
    ///
    /// Accepted keys:
    /// - `endpoint_addr` (string) / `endpoint_addrs` (array of strings):
    ///   Elasticsearch base URL(s), e.g. `http://es:9200`. At least one is
    ///   **required**. Trailing slashes are trimmed.
    /// - `field.index` (string, **required**): destination index name.
    /// - `field.type` (string, optional): accepted for compatibility; not
    ///   emitted (see module deviations).
    /// - `auth.username` / `auth.password` (strings, optional): when both are
    ///   present, sent as HTTP Basic auth.
    /// - `ssl_verify` (bool, default `true`): verify TLS certificates for
    ///   `https` endpoints.
    /// - `timeout` (integer seconds, default `10`): per-flush HTTP deadline.
    /// - `include_req_body` / `include_resp_body` (bool, default `false`):
    ///   include the request/response body in the default entry.
    /// - `log_format` (object, optional): custom flat entry of
    ///   `name -> "$var template"`.
    /// - Batch tuning: `batch_max_size`, `inactive_timeout`,
    ///   `buffer_duration`, `max_retry_count`, `retry_delay`,
    ///   `max_pending_entries` (see [`BatchConfig`]).
    ///
    /// ```yaml
    /// type: elasticsearch-logger
    /// config:
    ///   endpoint_addr: http://es:9200
    ///   field:
    ///     index: services
    ///   auth:
    ///     username: elastic
    ///     password: ${ES_PASSWORD}
    ///   ssl_verify: true
    ///   timeout: 10
    ///   batch_max_size: 1000
    /// ```
    pub fn from_config(
        config: &HashMap<String, Value>,
        resources: &Arc<PluginResources>,
    ) -> Result<Self, String> {
        let endpoints = collect_endpoints(config)?;

        let field = config
            .get("field")
            .and_then(|v| v.as_object())
            .ok_or_else(|| "elasticsearch-logger requires 'field'".to_string())?;
        let index = field
            .get("index")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| "elasticsearch-logger requires 'field.index'".to_string())?
            .to_string();

        let authorization = config
            .get("auth")
            .and_then(|v| v.as_object())
            .and_then(|auth| {
                let user = auth.get("username").and_then(|v| v.as_str())?;
                let pass = auth.get("password").and_then(|v| v.as_str())?;
                Some(format!(
                    "Basic {}",
                    STANDARD.encode(format!("{user}:{pass}"))
                ))
            });

        let ssl_verify = config
            .get("ssl_verify")
            .and_then(|v| v.as_bool())
            .unwrap_or(true);
        let timeout =
            Duration::from_secs(config.get("timeout").and_then(|v| v.as_u64()).unwrap_or(10));

        let include_req_body = config
            .get("include_req_body")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let include_resp_body = config
            .get("include_resp_body")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let log_format = parse_log_format(config)?;

        let batch_cfg = BatchConfig::from_config(config)?;
        let flusher = Arc::new(EsFlusher {
            client: resources.outbound.clone(),
            endpoints,
            cursor: AtomicUsize::new(0),
            index,
            authorization,
            ssl_verify,
            timeout,
        });
        let sink = BatchSink::spawn("elasticsearch-logger", batch_cfg, flusher);

        Ok(Self {
            sink,
            log_format,
            include_req_body,
            include_resp_body,
        })
    }
}

/// Reads `endpoint_addr` (string) and/or `endpoint_addrs` (array), trims
/// trailing slashes, and errors when none are present.
fn collect_endpoints(config: &HashMap<String, Value>) -> Result<Vec<String>, String> {
    let mut endpoints = Vec::new();
    if let Some(s) = config.get("endpoint_addr").and_then(|v| v.as_str()) {
        endpoints.push(s.trim_end_matches('/').to_string());
    }
    if let Some(arr) = config.get("endpoint_addrs").and_then(|v| v.as_array()) {
        for v in arr {
            if let Some(s) = v.as_str() {
                endpoints.push(s.trim_end_matches('/').to_string());
            }
        }
    }
    if endpoints.is_empty() {
        return Err(
            "elasticsearch-logger requires 'endpoint_addr' or 'endpoint_addrs'".to_string(),
        );
    }
    Ok(endpoints)
}

/// Builds the NDJSON `_bulk` body: for every entry an action line
/// `{"index":{"_index":<index>}}` followed by the entry, each terminated by a
/// newline (APISIX `get_logger_entry`). Pure and network-free for testing.
fn build_bulk_body(index: &str, entries: &[Value]) -> String {
    let action = serde_json::json!({ "index": { "_index": index } });
    let action_line = serde_json::to_string(&action).unwrap_or_default();
    let mut body = String::new();
    for entry in entries {
        body.push_str(&action_line);
        body.push('\n');
        body.push_str(&serde_json::to_string(entry).unwrap_or_else(|_| "{}".to_string()));
        body.push('\n');
    }
    body
}

#[async_trait]
impl BatchFlusher for EsFlusher {
    async fn flush(&self, entries: &[Value]) -> Result<(), FlushError> {
        let idx = self.cursor.fetch_add(1, Ordering::Relaxed) % self.endpoints.len();
        let url = format!("{}/_bulk", self.endpoints[idx]);
        let body = build_bulk_body(&self.index, entries);

        let mut headers = vec![
            (
                "Content-Type".to_string(),
                "application/x-ndjson".to_string(),
            ),
            (
                "Accept".to_string(),
                "application/vnd.elasticsearch+json".to_string(),
            ),
        ];
        if let Some(auth) = &self.authorization {
            headers.push(("Authorization".to_string(), auth.clone()));
        }

        let req = OutboundRequest {
            method: http::Method::POST,
            url,
            headers,
            body: Bytes::from(body),
            timeout: self.timeout,
            ssl_verify: self.ssl_verify,
        };

        match self.client.request(req).await {
            Ok(resp) if resp.status == 200 => Ok(()),
            Ok(resp) => Err(FlushError {
                message: format!(
                    "elasticsearch returned status {}: {}",
                    resp.status,
                    String::from_utf8_lossy(&resp.body)
                ),
                first_fail: None,
            }),
            Err(e) => Err(FlushError {
                message: format!("elasticsearch callout failed: {e}"),
                first_fail: None,
            }),
        }
    }
}

#[async_trait]
impl Plugin for ElasticsearchLoggerPlugin {
    fn plugin_type(&self) -> &str {
        "elasticsearch-logger"
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

    #[tokio::test]
    async fn requires_endpoint_and_index() {
        // no endpoint
        assert!(ElasticsearchLoggerPlugin::from_config(
            &cfg(json!({ "field": { "index": "svc" } })),
            &PluginResources::empty()
        )
        .is_err());
        // no field.index
        assert!(ElasticsearchLoggerPlugin::from_config(
            &cfg(json!({ "endpoint_addr": "http://es:9200" })),
            &PluginResources::empty()
        )
        .is_err());
        // both present
        assert!(ElasticsearchLoggerPlugin::from_config(
            &cfg(json!({ "endpoint_addr": "http://es:9200/", "field": { "index": "svc" } })),
            &PluginResources::empty()
        )
        .is_ok());
    }

    #[test]
    fn collects_and_trims_endpoints() {
        let e = collect_endpoints(&cfg(json!({
            "endpoint_addr": "http://a:9200/",
            "endpoint_addrs": ["http://b:9200/", "http://c:9200"]
        })))
        .unwrap();
        assert_eq!(e, vec!["http://a:9200", "http://b:9200", "http://c:9200"]);
    }

    #[tokio::test]
    async fn basic_auth_header_encoded() {
        let p = ElasticsearchLoggerPlugin::from_config(
            &cfg(json!({
                "endpoint_addr": "http://es:9200",
                "field": { "index": "svc" },
                "auth": { "username": "elastic", "password": "secret" }
            })),
            &PluginResources::empty(),
        );
        assert!(p.is_ok());
    }

    #[test]
    fn bulk_body_ndjson_shape() {
        let entries = vec![json!({ "a": 1 }), json!({ "b": 2 })];
        let body = build_bulk_body("svc", &entries);
        let lines: Vec<&str> = body.split('\n').collect();
        // action\nentry\naction\nentry\n -> 5 parts (trailing empty)
        assert_eq!(lines.len(), 5);
        assert_eq!(lines[0], r#"{"index":{"_index":"svc"}}"#);
        assert_eq!(lines[1], r#"{"a":1}"#);
        assert_eq!(lines[2], r#"{"index":{"_index":"svc"}}"#);
        assert_eq!(lines[3], r#"{"b":2}"#);
        assert_eq!(lines[4], "");
        assert!(body.ends_with('\n'));
    }
}
