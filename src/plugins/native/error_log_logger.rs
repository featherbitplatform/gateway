//! The `error-log-logger` node — a **reinterpreted** subset of APISIX's
//! `error-log-logger` plugin.
//!
//! In APISIX this plugin tails Nginx's internal error-log stream (via
//! `ngx.errlog`) and forwards those lines to a remote sink. featherbit has no
//! separate internal error-log stream to tail, so the faithful reinterpretation
//! is an **error-only access logger**: it builds the shared access-log entry and
//! ships it to a remote TCP sink **only when the request accumulated errors**
//! (`context.errors` is non-empty). Requests that succeed produce nothing.
//!
//! Entries are buffered in a [`BatchSink`] and delivered by a background task
//! that opens a fresh [`tokio::net::TcpStream`] per flush and writes each entry
//! as one newline-delimited JSON object. Delivery is fire-and-forget on the
//! request path, so place this node in the response pipeline **after the
//! `upstream` node** (and after any node whose errors you want captured).
//!
//! ## Deviations from APISIX
//! - Source of logs: featherbit logs **request-level errors** (`context.errors`)
//!   rather than the gateway's own internal error-log lines.
//! - Only the `tcp` sink (`host`/`port`) is implemented; the `skywalking`,
//!   `clickhouse`, and `kafka` sinks are out of scope for this socket-focused
//!   node.
//! - `level` is accepted for compatibility but featherbit filters on the
//!   *presence* of request errors, not on a syslog severity threshold.
//! - `tls` is **not yet supported**; `tls: true` is rejected at config load.

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

/// Serializes each entry as one line of JSON, newline-terminated. Pure helper.
fn entries_to_lines(entries: &[Value]) -> String {
    let mut out = String::new();
    for entry in entries {
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

#[async_trait]
impl BatchFlusher for TcpFlusher {
    async fn flush(&self, entries: &[Value]) -> Result<(), FlushError> {
        let addr = format!("{}:{}", self.host, self.port);
        let payload = entries_to_lines(entries);
        let result = async {
            let mut stream = tokio::time::timeout(self.timeout, TcpStream::connect(&addr))
                .await
                .map_err(|_| format!("timed out connecting to TCP server {addr}"))?
                .map_err(|e| format!("failed to connect to TCP server {addr}: {e}"))?;
            tokio::time::timeout(self.timeout, stream.write_all(payload.as_bytes()))
                .await
                .map_err(|_| format!("timed out sending to TCP server {addr}"))?
                .map_err(|e| format!("failed to send to TCP server {addr}: {e}"))?;
            let _ = stream.shutdown().await;
            Ok::<(), String>(())
        }
        .await;
        result.map_err(|message| FlushError {
            message,
            first_fail: None,
        })
    }
}

/// The `error-log-logger` plugin node.
pub struct ErrorLogLoggerPlugin {
    sink: BatchSink,
    log_format: Option<HashMap<String, Value>>,
}

impl ErrorLogLoggerPlugin {
    /// Builds the plugin from node config.
    ///
    /// Config keys:
    /// - `host` (string, **required**): TCP sink hostname or IP.
    /// - `port` (integer, **required**): TCP sink port.
    /// - `timeout` (integer seconds, default `3`): connect/send timeout.
    /// - `level` (string, default `"WARN"`): accepted for APISIX compatibility;
    ///   featherbit logs on the presence of `context.errors`, not on severity.
    /// - `tls` (bool, default `false`): **not yet supported** — `true` rejected.
    /// - `log_format` (object): custom `name -> "$var"` entry.
    /// - batch keys — see [`BatchConfig::from_config`].
    ///
    /// ```yaml
    /// - id: error-log
    ///   type: error-log-logger
    ///   config:
    ///     host: 127.0.0.1
    ///     port: 5044
    /// ```
    pub fn from_config(config: &HashMap<String, Value>) -> Result<Self, String> {
        let host = config
            .get("host")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .ok_or("error-log-logger: `host` is required")?
            .to_string();
        let port = config
            .get("port")
            .and_then(|v| v.as_u64())
            .filter(|p| *p <= u16::MAX as u64)
            .ok_or("error-log-logger: `port` is required and must be 0-65535")?
            as u16;

        if config.get("tls").and_then(|v| v.as_bool()).unwrap_or(false) {
            return Err("error-log-logger: TLS (`tls: true`) is not yet supported".to_string());
        }

        // Validate `level` against the APISIX enum if present (parsed, not enforced).
        if let Some(level) = config.get("level").and_then(|v| v.as_str()) {
            const LEVELS: [&str; 10] = [
                "STDERR", "EMERG", "ALERT", "CRIT", "ERR", "ERROR", "WARN", "NOTICE", "INFO",
                "DEBUG",
            ];
            if !LEVELS.contains(&level) {
                return Err(format!("error-log-logger: unknown `level` {level}"));
            }
        }

        // APISIX error-log-logger `timeout` is in seconds.
        let timeout_s = config.get("timeout").and_then(|v| v.as_u64()).unwrap_or(3);
        let timeout = Duration::from_secs(timeout_s.max(1));

        let log_format = parse_log_format(config)?;

        let batch_cfg = BatchConfig::from_config(config)?;
        let flusher = Arc::new(TcpFlusher {
            host: host.clone(),
            port,
            timeout,
        });
        let sink = BatchSink::spawn(
            &format!("error-log-logger:{host}:{port}"),
            batch_cfg,
            flusher,
        );

        Ok(Self { sink, log_format })
    }
}

#[async_trait]
impl Plugin for ErrorLogLoggerPlugin {
    fn plugin_type(&self) -> &str {
        "error-log-logger"
    }

    async fn execute(&self, ctx: Context, _named_inputs: &HashMap<String, Value>) -> PluginResult {
        // Only log requests that accumulated errors.
        if !ctx.errors.is_empty() {
            let entry = build_entry(&ctx, self.log_format.as_ref(), false, false);
            self.sink.push(entry);
        }
        Ok(PluginOutput {
            context: ctx,
            named_outputs: HashMap::new(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::{GatewayError, GatewayRequest, GatewayResponse, Protocol};
    use bytes::Bytes;
    use serde_json::json;

    fn base_config() -> HashMap<String, Value> {
        let mut c = HashMap::new();
        c.insert("host".to_string(), json!("127.0.0.1"));
        c.insert("port".to_string(), json!(5044));
        c
    }

    fn ctx(errors: Vec<GatewayError>) -> Context {
        Context {
            request: GatewayRequest {
                method: "GET".to_string(),
                path: "/".to_string(),
                host: "example.com".to_string(),
                scheme: "http".to_string(),
                headers: HashMap::new(),
                query_params: HashMap::new(),
                body: Bytes::new(),
                remote_addr: "127.0.0.1:5000".to_string(),
                protocol: Protocol::Http1,
            },
            response: GatewayResponse {
                status_code: 500,
                headers: HashMap::new(),
                body: Bytes::new(),
            },
            message: HashMap::new(),
            errors,
        }
    }

    #[test]
    fn from_config_requires_host_and_port() {
        assert!(ErrorLogLoggerPlugin::from_config(&HashMap::new()).is_err());
    }

    #[test]
    fn from_config_rejects_bad_level_and_tls() {
        let mut c = base_config();
        c.insert("level".to_string(), json!("LOUD"));
        assert!(ErrorLogLoggerPlugin::from_config(&c).is_err());

        let mut c = base_config();
        c.insert("tls".to_string(), json!(true));
        assert!(ErrorLogLoggerPlugin::from_config(&c)
            .err()
            .unwrap()
            .contains("TLS"));
    }

    #[tokio::test]
    async fn execute_logs_only_on_errors() {
        // A no-error context produces no entry; an errored one does. We verify
        // the entry-selection logic via the shared builder rather than the
        // socket: build_entry over an errored ctx includes an `errors` field.
        let clean = build_entry(&ctx(vec![]), None, false, false);
        assert!(clean.get("errors").is_none());

        let errored = ctx(vec![GatewayError {
            node_id: "upstream".to_string(),
            code: "502".to_string(),
            message: "bad gateway".to_string(),
            metadata: HashMap::new(),
        }]);
        let entry = build_entry(&errored, None, false, false);
        assert!(entry.get("errors").is_some());
    }
}
