//! ClickHouse access-logger (`clickhouse-logger`).
//!
//! Ships access-log entries to ClickHouse over its HTTP interface. Each request
//! builds one log entry (the shared [`build_entry`] shape, or a custom
//! `log_format`) that is handed to a fire-and-forget [`BatchSink`]; a
//! background task POSTs batches as an
//! `INSERT INTO <logtable> FORMAT JSONEachRow` statement whose body is the
//! newline-delimited JSON entries.
//!
//! Ports APISIX's `clickhouse-logger`: the SQL prefix, the `JSONEachRow`
//! payload, and the `X-ClickHouse-User` / `X-ClickHouse-Key` /
//! `X-ClickHouse-Database` auth headers all mirror `send_http_data` in the Lua
//! plugin.
//!
//! ## Deviations from APISIX
//!
//! - APISIX joins multiple encoded entries with a single space; featherbit
//!   joins with a newline. ClickHouse's `JSONEachRow` format accepts either as
//!   a row separator, so the ingested rows are identical.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
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

/// Node that batches access-log entries and inserts them into ClickHouse.
pub struct ClickhouseLoggerPlugin {
    sink: BatchSink,
    log_format: Option<HashMap<String, Value>>,
    include_req_body: bool,
    include_resp_body: bool,
}

/// Delivers batches to a ClickHouse HTTP endpoint.
struct ClickhouseFlusher {
    client: Arc<OutboundClient>,
    /// Endpoint base URLs; one is chosen per flush.
    endpoints: Vec<String>,
    cursor: AtomicUsize,
    database: String,
    logtable: String,
    user: String,
    password: String,
    ssl_verify: bool,
    timeout: Duration,
}

impl ClickhouseLoggerPlugin {
    /// Builds the plugin from node config.
    ///
    /// Accepted keys:
    /// - `endpoint_addr` (string) / `endpoint_addrs` (array of strings):
    ///   ClickHouse HTTP URL(s), e.g. `http://clickhouse:8123`. At least one is
    ///   **required**.
    /// - `database` (string, **required**): target database (sent via the
    ///   `X-ClickHouse-Database` header).
    /// - `logtable` (string, **required**): target table, used in the
    ///   `INSERT INTO <logtable>` statement.
    /// - `user` / `password` (strings, default `""`): ClickHouse credentials,
    ///   sent as `X-ClickHouse-User` / `X-ClickHouse-Key`.
    /// - `ssl_verify` (bool, default `true`): verify TLS certificates.
    /// - `timeout` (integer seconds, default `3`): per-flush HTTP deadline.
    /// - `include_req_body` / `include_resp_body` (bool, default `false`).
    /// - `log_format` (object, optional): custom flat entry.
    /// - Batch tuning (see [`BatchConfig`]).
    ///
    /// ```yaml
    /// type: clickhouse-logger
    /// config:
    ///   endpoint_addr: http://clickhouse:8123
    ///   database: default
    ///   logtable: gateway_logs
    ///   user: default
    ///   password: ${CLICKHOUSE_PASSWORD}
    ///   timeout: 3
    /// ```
    pub fn from_config(
        config: &HashMap<String, Value>,
        resources: &Arc<PluginResources>,
    ) -> Result<Self, String> {
        let endpoints = collect_endpoints(config)?;

        let logtable = required_string(config, "logtable")?;
        let database = required_string(config, "database")?;
        let user = config
            .get("user")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let password = config
            .get("password")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        let ssl_verify = config
            .get("ssl_verify")
            .and_then(|v| v.as_bool())
            .unwrap_or(true);
        let timeout =
            Duration::from_secs(config.get("timeout").and_then(|v| v.as_u64()).unwrap_or(3));

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
        let flusher = Arc::new(ClickhouseFlusher {
            client: resources.outbound.clone(),
            endpoints,
            cursor: AtomicUsize::new(0),
            database,
            logtable,
            user,
            password,
            ssl_verify,
            timeout,
        });
        let sink = BatchSink::spawn("clickhouse-logger", batch_cfg, flusher);

        Ok(Self {
            sink,
            log_format,
            include_req_body,
            include_resp_body,
        })
    }
}

fn required_string(config: &HashMap<String, Value>, key: &str) -> Result<String, String> {
    config
        .get(key)
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .ok_or_else(|| format!("clickhouse-logger requires '{key}'"))
}

fn collect_endpoints(config: &HashMap<String, Value>) -> Result<Vec<String>, String> {
    let mut endpoints = Vec::new();
    if let Some(s) = config.get("endpoint_addr").and_then(|v| v.as_str()) {
        endpoints.push(s.to_string());
    }
    if let Some(arr) = config.get("endpoint_addrs").and_then(|v| v.as_array()) {
        for v in arr {
            if let Some(s) = v.as_str() {
                endpoints.push(s.to_string());
            }
        }
    }
    if endpoints.is_empty() {
        return Err("clickhouse-logger requires 'endpoint_addr' or 'endpoint_addrs'".to_string());
    }
    Ok(endpoints)
}

/// Builds the request body: an `INSERT INTO <logtable> FORMAT JSONEachRow`
/// statement followed by the newline-joined JSON entries (APISIX
/// `send_http_data`). Pure and network-free for testing.
fn build_insert_body(logtable: &str, entries: &[Value]) -> String {
    let rows: Vec<String> = entries
        .iter()
        .map(|e| serde_json::to_string(e).unwrap_or_else(|_| "{}".to_string()))
        .collect();
    format!(
        "INSERT INTO {logtable} FORMAT JSONEachRow {}",
        rows.join("\n")
    )
}

#[async_trait]
impl BatchFlusher for ClickhouseFlusher {
    async fn flush(&self, entries: &[Value]) -> Result<(), FlushError> {
        let idx = self.cursor.fetch_add(1, Ordering::Relaxed) % self.endpoints.len();
        let url = self.endpoints[idx].clone();
        let body = build_insert_body(&self.logtable, entries);

        let headers = vec![
            ("Content-Type".to_string(), "application/json".to_string()),
            ("X-ClickHouse-User".to_string(), self.user.clone()),
            ("X-ClickHouse-Key".to_string(), self.password.clone()),
            ("X-ClickHouse-Database".to_string(), self.database.clone()),
        ];

        let req = OutboundRequest {
            method: http::Method::POST,
            url,
            headers,
            body: Bytes::from(body),
            timeout: self.timeout,
            ssl_verify: self.ssl_verify,
        };

        match self.client.request(req).await {
            // ClickHouse HTTP returns 200 on success; >=400 is a failure.
            Ok(resp) if resp.status < 400 => Ok(()),
            Ok(resp) => Err(FlushError {
                message: format!(
                    "clickhouse returned status {}: {}",
                    resp.status,
                    String::from_utf8_lossy(&resp.body)
                ),
                first_fail: None,
            }),
            Err(e) => Err(FlushError {
                message: format!("clickhouse callout failed: {e}"),
                first_fail: None,
            }),
        }
    }
}

#[async_trait]
impl Plugin for ClickhouseLoggerPlugin {
    fn plugin_type(&self) -> &str {
        "clickhouse-logger"
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

    fn full_cfg() -> Value {
        json!({
            "endpoint_addr": "http://clickhouse:8123",
            "database": "default",
            "logtable": "logs"
        })
    }

    #[tokio::test]
    async fn requires_endpoint_database_and_table() {
        assert!(ClickhouseLoggerPlugin::from_config(
            &cfg(json!({ "database": "d", "logtable": "t" })),
            &PluginResources::empty()
        )
        .is_err());
        assert!(ClickhouseLoggerPlugin::from_config(
            &cfg(json!({ "endpoint_addr": "http://c:8123", "logtable": "t" })),
            &PluginResources::empty()
        )
        .is_err());
        assert!(ClickhouseLoggerPlugin::from_config(
            &cfg(json!({ "endpoint_addr": "http://c:8123", "database": "d" })),
            &PluginResources::empty()
        )
        .is_err());
        assert!(
            ClickhouseLoggerPlugin::from_config(&cfg(full_cfg()), &PluginResources::empty())
                .is_ok()
        );
    }

    #[test]
    fn insert_body_shape() {
        let entries = vec![json!({ "a": 1 }), json!({ "b": 2 })];
        let body = build_insert_body("logs", &entries);
        assert_eq!(
            body,
            "INSERT INTO logs FORMAT JSONEachRow {\"a\":1}\n{\"b\":2}"
        );
    }

    #[test]
    fn insert_body_single_entry() {
        let body = build_insert_body("t", &[json!({ "x": true })]);
        assert_eq!(body, "INSERT INTO t FORMAT JSONEachRow {\"x\":true}");
    }
}
