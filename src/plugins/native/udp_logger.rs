//! The `udp-logger` node — ships per-request access-log entries to a remote
//! UDP endpoint, mirroring APISIX's `udp-logger` plugin.
//!
//! Entries are built with the shared [`build_entry`](crate::plugins::util::log_entry)
//! helper, buffered in a [`BatchSink`], and delivered by a background task that
//! binds an ephemeral [`tokio::net::UdpSocket`] and sends each entry as one
//! datagram of JSON bytes to `host:port`. UDP is inherently fire-and-forget: a
//! send that the kernel accepts is considered delivered; only local errors
//! (name resolution, socket) surface as flush failures.
//!
//! Delivery never blocks the request path — [`Plugin::execute`] pushes the
//! entry and returns the context unchanged — so this node should be placed in
//! the response pipeline **after the `upstream` node**.
//!
//! ## Deviations from APISIX
//! - Each entry is sent as its own datagram (one JSON object per packet),
//!   rather than APISIX's "single object when `batch_max_size == 1`, JSON array
//!   otherwise" shape.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;
use tokio::net::UdpSocket;

use crate::batch::{BatchConfig, BatchFlusher, BatchSink, FlushError};
use crate::context::Context;
use crate::plugins::util::log_entry::{build_entry, parse_log_format};
use crate::plugins::{Plugin, PluginOutput, PluginResult};

/// Serializes one entry to its datagram payload (compact JSON bytes). Pure and
/// network-independent for unit testing.
fn entry_to_datagram(entry: &Value) -> Vec<u8> {
    serde_json::to_vec(entry).unwrap_or_else(|_| b"null".to_vec())
}

/// [`BatchFlusher`] that sends each entry as one UDP datagram to `host:port`.
struct UdpFlusher {
    host: String,
    port: u16,
    timeout: Duration,
}

impl UdpFlusher {
    async fn send_all(&self, entries: &[Value]) -> Result<usize, (usize, String)> {
        let addr = format!("{}:{}", self.host, self.port);
        let socket = UdpSocket::bind("0.0.0.0:0")
            .await
            .map_err(|e| (0usize, format!("failed to bind UDP socket: {e}")))?;
        for (i, entry) in entries.iter().enumerate() {
            let payload = entry_to_datagram(entry);
            let send = socket.send_to(&payload, &addr);
            match tokio::time::timeout(self.timeout, send).await {
                Ok(Ok(_)) => {}
                Ok(Err(e)) => return Err((i, format!("failed to send to UDP server {addr}: {e}"))),
                Err(_) => return Err((i, format!("timed out sending to UDP server {addr}"))),
            }
        }
        Ok(entries.len())
    }
}

#[async_trait]
impl BatchFlusher for UdpFlusher {
    async fn flush(&self, entries: &[Value]) -> Result<(), FlushError> {
        match self.send_all(entries).await {
            Ok(_) => Ok(()),
            // Entries before `i` were already sent; retry only the tail.
            Err((i, message)) => Err(FlushError {
                message,
                first_fail: Some(i),
            }),
        }
    }
}

/// The `udp-logger` plugin node.
pub struct UdpLoggerPlugin {
    sink: BatchSink,
    log_format: Option<HashMap<String, Value>>,
    include_req_body: bool,
    include_resp_body: bool,
}

impl UdpLoggerPlugin {
    /// Builds the plugin from node config.
    ///
    /// Config keys:
    /// - `host` (string, **required**): UDP server hostname or IP.
    /// - `port` (integer, **required**): UDP server port.
    /// - `timeout` (integer seconds, default `3`): per-datagram send timeout.
    /// - `log_format` (object): custom `name -> "$var"` entry.
    /// - `include_req_body` / `include_resp_body` (bool, default `false`).
    /// - batch keys — see [`BatchConfig::from_config`].
    ///
    /// ```yaml
    /// - id: udp-log
    ///   type: udp-logger
    ///   config:
    ///     host: 127.0.0.1
    ///     port: 5140
    /// ```
    pub fn from_config(config: &HashMap<String, Value>) -> Result<Self, String> {
        let host = config
            .get("host")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .ok_or("udp-logger: `host` is required")?
            .to_string();
        let port = config
            .get("port")
            .and_then(|v| v.as_u64())
            .filter(|p| *p <= u16::MAX as u64)
            .ok_or("udp-logger: `port` is required and must be 0-65535")? as u16;

        // APISIX udp-logger `timeout` is in seconds.
        let timeout_s = config.get("timeout").and_then(|v| v.as_u64()).unwrap_or(3);
        let timeout = Duration::from_secs(timeout_s.max(1));

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
        let flusher = Arc::new(UdpFlusher {
            host: host.clone(),
            port,
            timeout,
        });
        let sink = BatchSink::spawn(&format!("udp-logger:{host}:{port}"), batch_cfg, flusher);

        Ok(Self {
            sink,
            log_format,
            include_req_body,
            include_resp_body,
        })
    }
}

#[async_trait]
impl Plugin for UdpLoggerPlugin {
    fn plugin_type(&self) -> &str {
        "udp-logger"
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
        c.insert("port".to_string(), json!(5140));
        c
    }

    #[test]
    fn from_config_requires_host_and_port() {
        assert!(UdpLoggerPlugin::from_config(&HashMap::new()).is_err());
        let mut c = HashMap::new();
        c.insert("port".to_string(), json!(514));
        assert!(
            UdpLoggerPlugin::from_config(&c).is_err(),
            "missing host must fail"
        );
    }

    #[tokio::test]
    async fn from_config_ok_with_valid_config() {
        assert!(UdpLoggerPlugin::from_config(&base_config()).is_ok());
    }

    #[test]
    fn entry_to_datagram_is_compact_json() {
        let payload = entry_to_datagram(&json!({"a": 1, "b": "x"}));
        let back: Value = serde_json::from_slice(&payload).unwrap();
        assert_eq!(back, json!({"a": 1, "b": "x"}));
    }
}
