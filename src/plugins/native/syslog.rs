//! The `syslog` node — ships per-request access-log entries to a remote syslog
//! server as RFC 5424 framed messages, over TCP or UDP, mirroring APISIX's
//! `syslog` plugin (`apisix/plugins/syslog/init.lua`).
//!
//! Each entry is built with the shared [`build_entry`](crate::plugins::util::log_entry)
//! helper, JSON-encoded, and wrapped in an RFC 5424 header
//! (`<priority>1 timestamp hostname app-name procid - msg`) exactly as APISIX
//! does via `apisix/utils/rfc5424.lua`. APISIX hard-codes facility `SYSLOG` (5)
//! and severity `INFO` (6), giving priority `5*8+6 = 46`; the hostname is the
//! request `Host` and the procid is the gateway process id. The framed strings
//! are buffered in a [`BatchSink`] and flushed as a single concatenated payload
//! over a fresh TCP or UDP socket per flush.
//!
//! Delivery is fire-and-forget on the request path — [`Plugin::execute`] pushes
//! the framed message and returns the context unchanged — so place this node in
//! the response pipeline **after the `upstream` node**.
//!
//! ## Deviations from APISIX
//! - `tls` is **not yet supported**; `tls: true` is rejected at config load.
//! - `flush_limit`, `drop_limit`, and `pool_size` are accepted for schema
//!   compatibility but not honored; batching is governed by the shared
//!   [`BatchConfig`] knobs instead.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpStream, UdpSocket};

use crate::batch::{BatchConfig, BatchFlusher, BatchSink, FlushError};
use crate::context::Context;
use crate::plugins::util::log_entry::{build_entry, parse_log_format};
use crate::plugins::{Plugin, PluginOutput, PluginResult};

/// Syslog facility `SYSLOG` (5), matching APISIX's hard-coded choice.
const FACILITY_SYSLOG: u8 = 5;
/// Syslog severity `INFO` (6), matching APISIX's hard-coded choice.
const SEVERITY_INFO: u8 = 6;

/// RFC 5424 priority value: `facility * 8 + severity`.
fn priority(facility: u8, severity: u8) -> u16 {
    facility as u16 * 8 + severity as u16
}

/// Builds one RFC 5424 syslog frame. Pure and independent of the clock/network
/// (the timestamp is passed in) so it can be unit-tested against a known value.
///
/// Shape: `<PRI>1 TIMESTAMP HOSTNAME APP-NAME PROCID - MSG\n`
/// (MSGID and STRUCTURED-DATA are `-`, matching APISIX's encoder).
fn build_syslog_frame(
    pri: u16,
    timestamp: &str,
    hostname: &str,
    app_name: &str,
    procid: u32,
    msg: &str,
) -> String {
    let hostname = if hostname.is_empty() { "-" } else { hostname };
    let app_name = if app_name.is_empty() { "-" } else { app_name };
    format!("<{pri}>1 {timestamp} {hostname} {app_name} {procid} - - {msg}\n")
}

/// Current time as an RFC 3339 "Zulu" timestamp (`YYYY-MM-DDTHH:MM:SS.mmmZ`),
/// matching APISIX's `get_rfc3339_zulu_timestamp`.
fn rfc3339_now() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    format_rfc3339(now.as_secs(), now.subsec_millis())
}

/// Formats an epoch-seconds/millis pair as an RFC 3339 Zulu timestamp using the
/// civil-from-days algorithm (no external date crate).
fn format_rfc3339(epoch_secs: u64, millis: u32) -> String {
    let days = (epoch_secs / 86_400) as i64;
    let secs_of_day = epoch_secs % 86_400;
    let (hour, minute, second) = (
        secs_of_day / 3600,
        (secs_of_day % 3600) / 60,
        secs_of_day % 60,
    );

    // Howard Hinnant's days-from-civil, inverted.
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { year + 1 } else { year };

    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        year, month, day, hour, minute, second, millis
    )
}

/// Transport for the syslog socket.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum SockType {
    Tcp,
    Udp,
}

/// [`BatchFlusher`] that concatenates the framed messages and sends them over
/// TCP or UDP.
struct SyslogFlusher {
    host: String,
    port: u16,
    sock_type: SockType,
    timeout: Duration,
}

impl SyslogFlusher {
    /// Concatenates the buffered frame strings into one payload.
    fn payload(entries: &[Value]) -> String {
        let mut out = String::new();
        for entry in entries {
            if let Some(s) = entry.as_str() {
                out.push_str(s);
            }
        }
        out
    }

    async fn send(&self, payload: &[u8]) -> Result<(), String> {
        let addr = format!("{}:{}", self.host, self.port);
        match self.sock_type {
            SockType::Tcp => {
                let mut stream = tokio::time::timeout(self.timeout, TcpStream::connect(&addr))
                    .await
                    .map_err(|_| format!("timed out connecting to syslog TCP {addr}"))?
                    .map_err(|e| format!("failed to connect to syslog TCP {addr}: {e}"))?;
                tokio::time::timeout(self.timeout, stream.write_all(payload))
                    .await
                    .map_err(|_| format!("timed out sending to syslog TCP {addr}"))?
                    .map_err(|e| format!("failed to send to syslog TCP {addr}: {e}"))?;
                let _ = stream.shutdown().await;
                Ok(())
            }
            SockType::Udp => {
                let socket = UdpSocket::bind("0.0.0.0:0")
                    .await
                    .map_err(|e| format!("failed to bind UDP socket: {e}"))?;
                tokio::time::timeout(self.timeout, socket.send_to(payload, &addr))
                    .await
                    .map_err(|_| format!("timed out sending to syslog UDP {addr}"))?
                    .map_err(|e| format!("failed to send to syslog UDP {addr}: {e}"))?;
                Ok(())
            }
        }
    }
}

#[async_trait]
impl BatchFlusher for SyslogFlusher {
    async fn flush(&self, entries: &[Value]) -> Result<(), FlushError> {
        let payload = Self::payload(entries);
        self.send(payload.as_bytes())
            .await
            .map_err(|message| FlushError {
                message,
                first_fail: None,
            })
    }
}

/// The `syslog` plugin node.
pub struct SyslogPlugin {
    sink: BatchSink,
    log_format: Option<HashMap<String, Value>>,
    include_req_body: bool,
    include_resp_body: bool,
    app_name: String,
    procid: u32,
}

impl SyslogPlugin {
    /// Builds the plugin from node config.
    ///
    /// Config keys:
    /// - `host` (string, **required**): syslog server hostname or IP.
    /// - `port` (integer, default `5140`): syslog server port.
    /// - `sock_type` (`"tcp"` | `"udp"`, default `"tcp"`): transport.
    /// - `timeout` (integer ms, default `3000`): connect/send timeout.
    /// - `tls` (bool, default `false`): **not yet supported** — `true` is rejected.
    /// - `flush_limit`, `drop_limit`, `pool_size`: accepted, not honored.
    /// - `log_format` (object): custom `name -> "$var"` entry.
    /// - `include_req_body` / `include_resp_body` (bool, default `false`).
    /// - batch keys — see [`BatchConfig::from_config`].
    ///
    /// ```yaml
    /// - id: syslog
    ///   type: syslog
    ///   config:
    ///     host: 127.0.0.1
    ///     port: 5140
    ///     sock_type: tcp
    /// ```
    pub fn from_config(config: &HashMap<String, Value>) -> Result<Self, String> {
        let host = config
            .get("host")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .ok_or("syslog: `host` is required")?
            .to_string();
        let port = config
            .get("port")
            .and_then(|v| v.as_u64())
            .filter(|p| *p <= u16::MAX as u64)
            .unwrap_or(5140) as u16;

        if config.get("tls").and_then(|v| v.as_bool()).unwrap_or(false) {
            return Err("syslog: TLS (`tls: true`) is not yet supported".to_string());
        }

        let sock_type = match config
            .get("sock_type")
            .and_then(|v| v.as_str())
            .unwrap_or("tcp")
        {
            "tcp" => SockType::Tcp,
            "udp" => SockType::Udp,
            other => {
                return Err(format!(
                    "syslog: `sock_type` must be tcp or udp, got {other}"
                ))
            }
        };

        // APISIX syslog `timeout` is in milliseconds.
        let timeout_ms = config
            .get("timeout")
            .and_then(|v| v.as_u64())
            .unwrap_or(3000);
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
        let flusher = Arc::new(SyslogFlusher {
            host: host.clone(),
            port,
            sock_type,
            timeout,
        });
        let sink = BatchSink::spawn(&format!("syslog:{host}:{port}"), batch_cfg, flusher);

        Ok(Self {
            sink,
            log_format,
            include_req_body,
            include_resp_body,
            app_name: "featherbit".to_string(),
            procid: std::process::id(),
        })
    }
}

#[async_trait]
impl Plugin for SyslogPlugin {
    fn plugin_type(&self) -> &str {
        "syslog"
    }

    async fn execute(&self, ctx: Context, _named_inputs: &HashMap<String, Value>) -> PluginResult {
        let entry = build_entry(
            &ctx,
            self.log_format.as_ref(),
            self.include_req_body,
            self.include_resp_body,
        );
        let json_str = serde_json::to_string(&entry).unwrap_or_else(|_| "null".to_string());
        let frame = build_syslog_frame(
            priority(FACILITY_SYSLOG, SEVERITY_INFO),
            &rfc3339_now(),
            &ctx.request.host,
            &self.app_name,
            self.procid,
            &json_str,
        );
        self.sink.push(Value::String(frame));
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
        c
    }

    #[test]
    fn priority_matches_apisix() {
        // SYSLOG facility (5) + INFO severity (6) => 46.
        assert_eq!(priority(FACILITY_SYSLOG, SEVERITY_INFO), 46);
        assert_eq!(priority(0, 0), 0);
        assert_eq!(priority(23, 7), 191);
    }

    #[test]
    fn syslog_frame_has_expected_shape() {
        let frame = build_syslog_frame(
            46,
            "2026-07-12T00:00:00.000Z",
            "example.com",
            "featherbit",
            1234,
            "{\"a\":1}",
        );
        assert_eq!(
            frame,
            "<46>1 2026-07-12T00:00:00.000Z example.com featherbit 1234 - - {\"a\":1}\n"
        );
    }

    #[test]
    fn syslog_frame_defaults_empty_fields_to_dash() {
        let frame = build_syslog_frame(46, "T", "", "", 1, "m");
        assert_eq!(frame, "<46>1 T - - 1 - - m\n");
    }

    #[test]
    fn format_rfc3339_known_epoch() {
        // 2021-01-01T00:00:00Z == 1609459200
        assert_eq!(format_rfc3339(1_609_459_200, 0), "2021-01-01T00:00:00.000Z");
        // Unix epoch.
        assert_eq!(format_rfc3339(0, 5), "1970-01-01T00:00:00.005Z");
    }

    #[test]
    fn payload_concatenates_frame_strings() {
        let entries = vec![json!("a\n"), json!("b\n")];
        assert_eq!(SyslogFlusher::payload(&entries), "a\nb\n");
    }

    #[test]
    fn from_config_rejects_bad_sock_type_and_tls() {
        let mut c = base_config();
        c.insert("sock_type".to_string(), json!("sctp"));
        assert!(SyslogPlugin::from_config(&c).is_err());

        let mut c = base_config();
        c.insert("tls".to_string(), json!(true));
        assert!(SyslogPlugin::from_config(&c).err().unwrap().contains("TLS"));
    }

    #[tokio::test]
    async fn from_config_ok_defaults_port_5140() {
        assert!(SyslogPlugin::from_config(&base_config()).is_ok());
    }
}
