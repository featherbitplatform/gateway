//! Alibaba Cloud SLS (Simple Log Service) access-logger (`sls-logger`).
//!
//! Ships access-log entries to Alibaba Cloud's Log Service. Each request builds
//! one log entry (the shared [`build_entry`] shape, or a custom `log_format`)
//! that is handed to a fire-and-forget [`BatchSink`]; a background task POSTs
//! batches to the SLS `PutLogs` REST endpoint, signed with an HMAC-SHA1
//! `Authorization: LOG <id>:<signature>` header.
//!
//! ## Deviations from APISIX
//!
//! APISIX's `sls-logger` does **not** use the SLS HTTP REST API. It serializes
//! each entry into an RFC 5424 syslog frame (with the project, logstore, and
//! access keys embedded as syslog structured-data) and streams it over a raw
//! **TLS/TCP** socket to the SLS syslog ingress. featherbit has no TCP sink;
//! its shared infrastructure is HTTP-only. This port therefore targets the
//! **documented SLS `PutLogs` REST API** instead
//! (<https://www.alibabacloud.com/help/en/sls/developer-reference/api-putlogs>):
//!
//! - The batch is POSTed as JSON `{"__topic__","__source__","__logs__":[…]}`
//!   to `https://<project>.<host>:<port>/logstores/<logstore>/shards/lb`.
//! - Requests are signed per the SLS spec: `Content-MD5`, `Date`, the sorted
//!   `x-log-*` canonical headers, and the canonical resource are HMAC-SHA1
//!   signed with the access-key secret, base64-encoded, and sent as
//!   `Authorization: LOG <access_key_id>:<signature>`.
//! - SLS requires string log values, so non-string entry fields are
//!   JSON-encoded (mirroring how the syslog path stringifies the entry).
//!
//! The signing helper and its primitives (MD5, HMAC-SHA1) are unit-tested
//! against fixed vectors; no end-to-end SLS handshake is exercised.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

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

const API_VERSION: &str = "0.6.0";
const SIGNATURE_METHOD: &str = "hmac-sha1";

/// Node that batches access-log entries and ships them to Alibaba Cloud SLS.
pub struct SlsLoggerPlugin {
    sink: BatchSink,
    log_format: Option<HashMap<String, Value>>,
    include_req_body: bool,
    include_resp_body: bool,
}

/// Delivers batches to the SLS `PutLogs` REST endpoint.
struct SlsFlusher {
    client: Arc<OutboundClient>,
    /// `<project>.<host>` — the SLS endpoint host, also the URL authority.
    endpoint_host: String,
    port: u16,
    logstore: String,
    access_key_id: String,
    access_key_secret: String,
    timeout: Duration,
}

impl SlsLoggerPlugin {
    /// Builds the plugin from node config.
    ///
    /// Accepted keys (all of `host`/`port`/`project`/`logstore`/
    /// `access_key_id`/`access_key_secret` are **required**):
    /// - `host` (string): SLS endpoint, e.g. `cn-hangzhou.log.aliyuncs.com`.
    /// - `port` (integer): endpoint port, e.g. `443`.
    /// - `project` (string): SLS project; prefixed to `host` as the request
    ///   authority `<project>.<host>`.
    /// - `logstore` (string): destination logstore.
    /// - `access_key_id` / `access_key_secret` (strings): RAM credentials used
    ///   to sign each request.
    /// - `timeout` (integer ms, default `5000`): per-flush HTTP deadline.
    /// - `include_req_body` / `include_resp_body` (bool, default `false`).
    /// - `log_format` (object, optional): custom flat entry.
    /// - Batch tuning (see [`BatchConfig`]).
    ///
    /// ```yaml
    /// type: sls-logger
    /// config:
    ///   host: cn-hangzhou.log.aliyuncs.com
    ///   port: 443
    ///   project: my-project
    ///   logstore: gateway
    ///   access_key_id: ${SLS_KEY_ID}
    ///   access_key_secret: ${SLS_KEY_SECRET}
    /// ```
    pub fn from_config(
        config: &HashMap<String, Value>,
        resources: &Arc<PluginResources>,
    ) -> Result<Self, String> {
        let host = required_string(config, "host")?;
        let project = required_string(config, "project")?;
        let logstore = required_string(config, "logstore")?;
        let access_key_id = required_string(config, "access_key_id")?;
        let access_key_secret = required_string(config, "access_key_secret")?;
        let port = config
            .get("port")
            .and_then(|v| v.as_u64())
            .ok_or_else(|| "sls-logger requires 'port'".to_string())? as u16;

        let timeout = Duration::from_millis(
            config
                .get("timeout")
                .and_then(|v| v.as_u64())
                .unwrap_or(5000),
        );

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
        let flusher = Arc::new(SlsFlusher {
            client: resources.outbound.clone(),
            endpoint_host: format!("{project}.{host}"),
            port,
            logstore,
            access_key_id,
            access_key_secret,
            timeout,
        });
        let sink = BatchSink::spawn("sls-logger", batch_cfg, flusher);

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
        .ok_or_else(|| format!("sls-logger requires '{key}'"))
}

/// Builds the SLS `PutLogs` JSON payload. Each entry becomes a log object of
/// string values (non-string fields JSON-encoded) plus a `__time__` field.
/// Pure and network-free for testing.
fn build_logs_payload(entries: &[Value], now_unix: u64) -> Value {
    let logs: Vec<Value> = entries
        .iter()
        .map(|entry| {
            let mut obj = serde_json::Map::new();
            obj.insert("__time__".to_string(), Value::from(now_unix));
            if let Some(map) = entry.as_object() {
                for (k, v) in map {
                    obj.insert(k.clone(), Value::String(stringify(v)));
                }
            } else {
                obj.insert("log".to_string(), Value::String(stringify(entry)));
            }
            Value::Object(obj)
        })
        .collect();
    serde_json::json!({
        "__topic__": "",
        "__source__": "",
        "__logs__": logs,
    })
}

/// SLS log values must be strings; keep strings as-is, JSON-encode the rest.
fn stringify(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Builds the SLS request signature and the headers it covers.
///
/// Returns `(headers, signature)` where `headers` are the header pairs to send
/// (including `Authorization`). `body_md5` is the uppercase-hex MD5 of the
/// request body; `date` is the RFC 1123 GMT timestamp. Split out from the
/// network path so it is unit-testable with fixed inputs (mirrors the SLS
/// signing spec).
fn sign_request(
    access_key_id: &str,
    access_key_secret: &str,
    logstore: &str,
    body: &[u8],
    date: &str,
) -> (Vec<(String, String)>, String) {
    let content_md5 = md5_hex_upper(body);
    let content_type = "application/json";
    let body_size = body.len().to_string();

    // Canonical x-log-* headers, sorted by key ascending.
    let canonical_headers = format!(
        "x-log-apiversion:{API_VERSION}\nx-log-bodyrawsize:{body_size}\nx-log-signaturemethod:{SIGNATURE_METHOD}"
    );
    let canonical_resource = format!("/logstores/{logstore}/shards/lb");

    let sign_string = format!(
        "POST\n{content_md5}\n{content_type}\n{date}\n{canonical_headers}\n{canonical_resource}"
    );
    let signature = STANDARD.encode(hmac_sha1(
        access_key_secret.as_bytes(),
        sign_string.as_bytes(),
    ));

    let headers = vec![
        ("Content-Type".to_string(), content_type.to_string()),
        ("Content-MD5".to_string(), content_md5),
        ("Date".to_string(), date.to_string()),
        ("x-log-apiversion".to_string(), API_VERSION.to_string()),
        (
            "x-log-signaturemethod".to_string(),
            SIGNATURE_METHOD.to_string(),
        ),
        ("x-log-bodyrawsize".to_string(), body_size),
        (
            "Authorization".to_string(),
            format!("LOG {access_key_id}:{signature}"),
        ),
    ];
    (headers, signature)
}

/// HMAC-SHA1 via ring, returning the raw 20-byte tag.
fn hmac_sha1(key: &[u8], msg: &[u8]) -> Vec<u8> {
    let k = ring::hmac::Key::new(ring::hmac::HMAC_SHA1_FOR_LEGACY_USE_ONLY, key);
    ring::hmac::sign(&k, msg).as_ref().to_vec()
}

/// Uppercase hex MD5 digest, as required for the SLS `Content-MD5` header.
fn md5_hex_upper(data: &[u8]) -> String {
    let digest = md5(data);
    let mut s = String::with_capacity(32);
    for b in digest {
        s.push_str(&format!("{b:02X}"));
    }
    s
}

/// Self-contained MD5 (RFC 1321). SLS requires a `Content-MD5` header and the
/// project has no MD5 dependency, so it is implemented here and unit-tested
/// against the RFC vectors.
fn md5(input: &[u8]) -> [u8; 16] {
    const S: [u32; 64] = [
        7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 5, 9, 14, 20, 5, 9, 14, 20, 5,
        9, 14, 20, 5, 9, 14, 20, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 6, 10,
        15, 21, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21,
    ];
    const K: [u32; 64] = [
        0xd76aa478, 0xe8c7b756, 0x242070db, 0xc1bdceee, 0xf57c0faf, 0x4787c62a, 0xa8304613,
        0xfd469501, 0x698098d8, 0x8b44f7af, 0xffff5bb1, 0x895cd7be, 0x6b901122, 0xfd987193,
        0xa679438e, 0x49b40821, 0xf61e2562, 0xc040b340, 0x265e5a51, 0xe9b6c7aa, 0xd62f105d,
        0x02441453, 0xd8a1e681, 0xe7d3fbc8, 0x21e1cde6, 0xc33707d6, 0xf4d50d87, 0x455a14ed,
        0xa9e3e905, 0xfcefa3f8, 0x676f02d9, 0x8d2a4c8a, 0xfffa3942, 0x8771f681, 0x6d9d6122,
        0xfde5380c, 0xa4beea44, 0x4bdecfa9, 0xf6bb4b60, 0xbebfbc70, 0x289b7ec6, 0xeaa127fa,
        0xd4ef3085, 0x04881d05, 0xd9d4d039, 0xe6db99e5, 0x1fa27cf8, 0xc4ac5665, 0xf4292244,
        0x432aff97, 0xab9423a7, 0xfc93a039, 0x655b59c3, 0x8f0ccc92, 0xffeff47d, 0x85845dd1,
        0x6fa87e4f, 0xfe2ce6e0, 0xa3014314, 0x4e0811a1, 0xf7537e82, 0xbd3af235, 0x2ad7d2bb,
        0xeb86d391,
    ];

    let mut a0: u32 = 0x67452301;
    let mut b0: u32 = 0xefcdab89;
    let mut c0: u32 = 0x98badcfe;
    let mut d0: u32 = 0x10325476;

    let mut msg = input.to_vec();
    let bit_len = (input.len() as u64).wrapping_mul(8);
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bit_len.to_le_bytes());

    for chunk in msg.chunks_exact(64) {
        let mut m = [0u32; 16];
        for (i, word) in m.iter_mut().enumerate() {
            *word = u32::from_le_bytes([
                chunk[i * 4],
                chunk[i * 4 + 1],
                chunk[i * 4 + 2],
                chunk[i * 4 + 3],
            ]);
        }

        let (mut a, mut b, mut c, mut d) = (a0, b0, c0, d0);
        for i in 0..64 {
            let (f, g) = match i {
                0..=15 => ((b & c) | (!b & d), i),
                16..=31 => ((d & b) | (!d & c), (5 * i + 1) % 16),
                32..=47 => (b ^ c ^ d, (3 * i + 5) % 16),
                _ => (c ^ (b | !d), (7 * i) % 16),
            };
            let f = f.wrapping_add(a).wrapping_add(K[i]).wrapping_add(m[g]);
            a = d;
            d = c;
            c = b;
            b = b.wrapping_add(f.rotate_left(S[i]));
        }
        a0 = a0.wrapping_add(a);
        b0 = b0.wrapping_add(b);
        c0 = c0.wrapping_add(c);
        d0 = d0.wrapping_add(d);
    }

    let mut out = [0u8; 16];
    out[0..4].copy_from_slice(&a0.to_le_bytes());
    out[4..8].copy_from_slice(&b0.to_le_bytes());
    out[8..12].copy_from_slice(&c0.to_le_bytes());
    out[12..16].copy_from_slice(&d0.to_le_bytes());
    out
}

/// Formats a Unix timestamp (seconds) as an RFC 1123 GMT date, as SLS's `Date`
/// header requires (e.g. `Mon, 03 Jan 2022 04:05:06 GMT`).
fn format_http_date(unix_secs: u64) -> String {
    const DAYS: [&str; 7] = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"];
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let days = unix_secs / 86400;
    let secs_of_day = unix_secs % 86400;
    let (hour, minute, second) = (
        secs_of_day / 3600,
        (secs_of_day % 3600) / 60,
        secs_of_day % 60,
    );
    // 1970-01-01 was a Thursday (index 0 in DAYS).
    let weekday = DAYS[(days % 7) as usize];

    // Convert day count to civil (year, month, day).
    let mut year = 1970u64;
    let mut days_left = days;
    loop {
        let leap =
            (year.is_multiple_of(4) && !year.is_multiple_of(100)) || year.is_multiple_of(400);
        let year_days = if leap { 366 } else { 365 };
        if days_left < year_days {
            break;
        }
        days_left -= year_days;
        year += 1;
    }
    let leap = (year.is_multiple_of(4) && !year.is_multiple_of(100)) || year.is_multiple_of(400);
    let month_lengths = [
        31,
        if leap { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    let mut month = 0usize;
    while days_left >= month_lengths[month] {
        days_left -= month_lengths[month];
        month += 1;
    }
    let day = days_left + 1;

    format!(
        "{weekday}, {day:02} {mon} {year:04} {hour:02}:{minute:02}:{second:02} GMT",
        mon = MONTHS[month],
    )
}

#[async_trait]
impl BatchFlusher for SlsFlusher {
    async fn flush(&self, entries: &[Value]) -> Result<(), FlushError> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let payload = build_logs_payload(entries, now);
        let body = serde_json::to_vec(&payload).map_err(|e| FlushError {
            message: format!("sls payload encode failed: {e}"),
            first_fail: None,
        })?;

        let date = format_http_date(now);
        let (mut headers, _sig) = sign_request(
            &self.access_key_id,
            &self.access_key_secret,
            &self.logstore,
            &body,
            &date,
        );
        headers.push(("Host".to_string(), self.endpoint_host.clone()));

        let url = format!(
            "https://{}:{}/logstores/{}/shards/lb",
            self.endpoint_host, self.port, self.logstore
        );
        let req = OutboundRequest {
            method: http::Method::POST,
            url,
            headers,
            body: Bytes::from(body),
            timeout: self.timeout,
            ssl_verify: true,
        };

        match self.client.request(req).await {
            Ok(resp) if resp.status == 200 => Ok(()),
            Ok(resp) => Err(FlushError {
                message: format!(
                    "sls returned status {}: {}",
                    resp.status,
                    String::from_utf8_lossy(&resp.body)
                ),
                first_fail: None,
            }),
            Err(e) => Err(FlushError {
                message: format!("sls callout failed: {e}"),
                first_fail: None,
            }),
        }
    }
}

#[async_trait]
impl Plugin for SlsLoggerPlugin {
    fn plugin_type(&self) -> &str {
        "sls-logger"
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
            "host": "cn-hangzhou.log.aliyuncs.com",
            "port": 443,
            "project": "proj",
            "logstore": "store",
            "access_key_id": "id",
            "access_key_secret": "secret"
        })
    }

    #[tokio::test]
    async fn requires_all_fields() {
        assert!(SlsLoggerPlugin::from_config(
            &cfg(json!({ "host": "h", "port": 443, "project": "p", "logstore": "l", "access_key_id": "i" })),
            &PluginResources::empty()
        )
        .is_err());
        assert!(SlsLoggerPlugin::from_config(&cfg(full_cfg()), &PluginResources::empty()).is_ok());
    }

    #[test]
    fn md5_known_vectors() {
        assert_eq!(md5_hex_upper(b""), "D41D8CD98F00B204E9800998ECF8427E");
        assert_eq!(md5_hex_upper(b"abc"), "900150983CD24FB0D6963F7D28E17F72");
        assert_eq!(
            md5_hex_upper(b"The quick brown fox jumps over the lazy dog"),
            "9E107D9D372BB6826BD81D3542A419D6"
        );
    }

    #[test]
    fn hmac_sha1_rfc2202_vector() {
        // RFC 2202 test case 1: key = 20 x 0x0b, data = "Hi There".
        let key = [0x0bu8; 20];
        let tag = hmac_sha1(&key, b"Hi There");
        let hex: String = tag.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(hex, "b617318655057264e28bc0b6fb378c8ef146be00");
    }

    #[test]
    fn http_date_formatting() {
        // 1970-01-01T00:00:00Z was a Thursday.
        assert_eq!(format_http_date(0), "Thu, 01 Jan 1970 00:00:00 GMT");
        // 1234567890 = 2009-02-13T23:31:30Z (a Friday).
        assert_eq!(
            format_http_date(1234567890),
            "Fri, 13 Feb 2009 23:31:30 GMT"
        );
    }

    #[test]
    fn sign_request_is_stable() {
        let body = br#"{"__topic__":"","__source__":"","__logs__":[]}"#;
        let (headers, sig) = sign_request(
            "AKID",
            "AKSECRET",
            "store",
            body,
            "Mon, 03 Jan 2022 04:05:06 GMT",
        );
        // Signing is deterministic for fixed inputs.
        let (_, sig2) = sign_request(
            "AKID",
            "AKSECRET",
            "store",
            body,
            "Mon, 03 Jan 2022 04:05:06 GMT",
        );
        assert_eq!(sig, sig2);
        // Authorization header has the LOG <id>:<sig> shape.
        let auth = headers
            .iter()
            .find(|(k, _)| k == "Authorization")
            .map(|(_, v)| v.clone())
            .unwrap();
        assert_eq!(auth, format!("LOG AKID:{sig}"));
        // Stable base64 signature (recomputed vector).
        assert_eq!(sig, "ftz3mY2D1oijCA9FoZlVAx47RzQ=");
    }

    #[test]
    fn logs_payload_stringifies_values() {
        let entries = vec![json!({ "status": 200, "path": "/x" })];
        let payload = build_logs_payload(&entries, 1000);
        let log = &payload["__logs__"][0];
        assert_eq!(log["__time__"], json!(1000));
        assert_eq!(log["status"], json!("200"));
        assert_eq!(log["path"], json!("/x"));
        assert_eq!(payload["__topic__"], json!(""));
    }
}
