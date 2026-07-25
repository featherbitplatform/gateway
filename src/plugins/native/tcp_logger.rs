//! The `tcp-logger` node — ships per-request access-log entries to a remote
//! TCP endpoint (a log collector such as Logstash, Fluentd, or a raw TCP
//! sink), mirroring APISIX's `tcp-logger` plugin.
//!
//! Entries are built with the shared [`build_entry`](crate::plugins::util::log_entry)
//! helper, buffered in a [`BatchSink`], and delivered by a background task that
//! opens a fresh [`tokio::net::TcpStream`] per flush, writes each entry as one
//! newline-delimited JSON object, and closes the connection. Delivery is
//! **fire-and-forget** on the request path: [`Plugin::execute`] never blocks on
//! the network and always returns the context unchanged, so this node should be
//! placed in the response pipeline **after the `upstream` node** where the
//! final status and body size are available.
//!
//! ## Deviations from APISIX
//! - Entries are always sent as newline-delimited JSON (one object per line),
//!   rather than APISIX's "single object when `batch_max_size == 1`, JSON array
//!   otherwise" shape. This keeps the wire format stable regardless of batching.
//! - `tls` / `tls_options` are **not yet supported**; configuring `tls: true`
//!   is rejected at config load. Plain TCP only.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;

use crate::batch::{BatchConfig, BatchFlusher, BatchSink, FlushError};
use crate::context::Context;
use crate::plugins::util::log_entry::{build_entry, parse_log_format};
use crate::plugins::{Plugin, PluginOutput, PluginResult};

/// Serializes each entry as one line of JSON, newline-terminated, and
/// concatenates them into the payload written to the socket. Pure and
/// independent of the network so it can be unit-tested directly.
fn entries_to_lines(entries: &[Value]) -> String {
    let mut out = String::new();
    for entry in entries {
        // Value serialization is infallible; fall back to `null` defensively.
        out.push_str(&serde_json::to_string(entry).unwrap_or_else(|_| "null".to_string()));
        out.push('\n');
    }
    out
}

/// [`BatchFlusher`] that connects to `host:port` and writes the batch.
struct TcpFlusher {
    host: String,
    port: u16,
    timeout: Duration,
}

impl TcpFlusher {
    async fn send(&self, payload: &[u8]) -> Result<(), String> {
        let addr = format!("{}:{}", self.host, self.port);
        let mut stream = tokio::time::timeout(self.timeout, TcpStream::connect(&addr))
            .await
            .map_err(|_| format!("timed out connecting to TCP server {addr}"))?
            .map_err(|e| format!("failed to connect to TCP server {addr}: {e}"))?;
        tokio::time::timeout(self.timeout, stream.write_all(payload))
            .await
            .map_err(|_| format!("timed out sending to TCP server {addr}"))?
            .map_err(|e| format!("failed to send to TCP server {addr}: {e}"))?;
        let _ = stream.shutdown().await;
        Ok(())
    }
}

#[async_trait]
impl BatchFlusher for TcpFlusher {
    async fn flush(&self, entries: &[Value]) -> Result<(), FlushError> {
        let payload = entries_to_lines(entries);
        self.send(payload.as_bytes())
            .await
            .map_err(|message| FlushError {
                message,
                // A TCP connect/write failure delivered nothing; retry the whole batch.
                first_fail: None,
            })
    }
}

/// The `tcp-logger` plugin node.
pub struct TcpLoggerPlugin {
    sink: BatchSink,
    log_format: Option<HashMap<String, Value>>,
    include_req_body: bool,
    include_resp_body: bool,
}

impl TcpLoggerPlugin {
    /// Builds the plugin from node config.
    ///
    /// Config keys:
    /// - `host` (string, **required**): TCP server hostname or IP.
    /// - `port` (integer, **required**): TCP server port.
    /// - `timeout` (integer ms, default `1000`): connect/send timeout.
    /// - `tls` (bool, default `false`): **not yet supported** — `true` is rejected.
    /// - `tls_options` (string): accepted but ignored (TLS unsupported).
    /// - `log_format` (object): custom `name -> "$var"` entry; when set, the
    ///   default structured entry is replaced by this flat object.
    /// - `include_req_body` / `include_resp_body` (bool, default `false`): add
    ///   the request/response body to the default entry.
    /// - batch keys (`batch_max_size`, `inactive_timeout`, `buffer_duration`,
    ///   `max_retry_count`, `retry_delay`, `max_pending_entries`) — see
    ///   [`BatchConfig::from_config`].
    ///
    /// ```yaml
    /// - id: tcp-log
    ///   type: tcp-logger
    ///   config:
    ///     host: 127.0.0.1
    ///     port: 5044
    ///     timeout: 1000
    ///     batch_max_size: 100
    /// ```
    pub fn from_config(config: &HashMap<String, Value>) -> Result<Self, String> {
        let host = config
            .get("host")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .ok_or("tcp-logger: `host` is required")?
            .to_string();
        let port = config
            .get("port")
            .and_then(|v| v.as_u64())
            .filter(|p| *p <= u16::MAX as u64)
            .ok_or("tcp-logger: `port` is required and must be 0-65535")? as u16;

        if config.get("tls").and_then(|v| v.as_bool()).unwrap_or(false) {
            return Err("tcp-logger: TLS (`tls: true`) is not yet supported".to_string());
        }

        let timeout_ms = config
            .get("timeout")
            .and_then(|v| v.as_u64())
            .unwrap_or(1000);
        let timeout = Duration::from_millis(timeout_ms.max(1));

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
        let flusher = Arc::new(TcpFlusher {
            host: host.clone(),
            port,
            timeout,
        });
        let sink = BatchSink::spawn(&format!("tcp-logger:{host}:{port}"), batch_cfg, flusher);

        Ok(Self {
            sink,
            log_format,
            include_req_body,
            include_resp_body,
        })
    }
}

#[async_trait]
impl Plugin for TcpLoggerPlugin {
    fn plugin_type(&self) -> &str {
        "tcp-logger"
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

    fn base_config() -> HashMap<String, Value> {
        let mut c = HashMap::new();
        c.insert("host".to_string(), json!("127.0.0.1"));
        c.insert("port".to_string(), json!(5044));
        c
    }

    #[test]
    fn from_config_requires_host_and_port() {
        assert!(TcpLoggerPlugin::from_config(&HashMap::new()).is_err());
        let mut c = HashMap::new();
        c.insert("host".to_string(), json!("h"));
        assert!(
            TcpLoggerPlugin::from_config(&c).is_err(),
            "missing port must fail"
        );
    }

    #[test]
    fn from_config_rejects_tls() {
        let mut c = base_config();
        c.insert("tls".to_string(), json!(true));
        let err = TcpLoggerPlugin::from_config(&c).err().unwrap();
        assert!(err.contains("TLS"), "error should mention TLS: {err}");
    }

    #[tokio::test]
    async fn from_config_ok_with_valid_config() {
        // Needs a tokio runtime because BatchSink::spawn spawns a task.
        assert!(TcpLoggerPlugin::from_config(&base_config()).is_ok());
    }

    #[test]
    fn entries_to_lines_is_newline_delimited_json() {
        let entries = vec![json!({"a": 1}), json!({"b": 2})];
        let out = entries_to_lines(&entries);
        assert_eq!(out, "{\"a\":1}\n{\"b\":2}\n");
        // Each line round-trips as JSON.
        for line in out.lines() {
            let _: Value = serde_json::from_str(line).unwrap();
        }
    }
}
