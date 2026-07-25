//! Tencent Cloud CLS (Cloud Log Service) access-logger (`tencent-cloud-cls`).
//!
//! Ships access-log entries to Tencent Cloud's Log Service. Each request builds
//! one log entry (the shared [`build_entry`] shape, or a custom `log_format`)
//! that is handed to a fire-and-forget [`BatchSink`]; a background task POSTs
//! batches to the CLS `/structuredlog` upload endpoint, signed with the
//! CLS/COS-style `q-sign-algorithm=sha1` `Authorization` header.
//!
//! The signature is ported faithfully from APISIX's `cls-sdk.lua` `sign()`:
//! `sign_key = hex(hmac_sha1(secret_key, sign_time))`, then
//! `signature = hex(hmac_sha1(sign_key, string_to_sign))`, where
//! `string_to_sign = "sha1\n<sign_time>\n<sha1(http_request_info)>\n"` and
//! `http_request_info = "post\n/structuredlog\n\n\n"`. See
//! <https://cloud.tencent.com/document/product/614/12445>.
//!
//! ## Deviations from APISIX
//!
//! - **JSON body, not protobuf.** The CLS SDK serializes the `LogGroupList`
//!   with protobuf and sends `application/x-protobuf`. featherbit has no
//!   protobuf codec, so it sends the equivalent structured-log payload as JSON
//!   (`application/json`): each entry is normalized to a list of
//!   `{key, value}` `contents` (non-string values JSON-encoded), grouped into
//!   one `LogGroup`. The signature, endpoint, topic query parameter, and log
//!   normalization are otherwise faithful. Against a live CLS endpoint the
//!   protobuf content type would be required; this is documented as a subset.
//! - **`source` omitted.** The SDK sets each `LogGroup.source` to the host IP;
//!   featherbit does not resolve its own IP and leaves it empty.
//!
//! The signing helper and its primitives (SHA-1, HMAC-SHA1) are unit-tested
//! against fixed vectors.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use bytes::Bytes;
use serde_json::Value;

use crate::batch::{BatchConfig, BatchFlusher, BatchSink, FlushError};
use crate::context::Context;
use crate::outbound::{OutboundClient, OutboundRequest};
use crate::plugins::resources::PluginResources;
use crate::plugins::util::log_entry::{build_entry, parse_log_format};
use crate::plugins::{Plugin, PluginOutput, PluginResult};

const CLS_API_PATH: &str = "/structuredlog";
const AUTH_EXPIRE_SECS: u64 = 60;

/// Node that batches access-log entries and ships them to Tencent Cloud CLS.
pub struct TencentCloudClsPlugin {
    sink: BatchSink,
    log_format: Option<HashMap<String, Value>>,
    include_req_body: bool,
    include_resp_body: bool,
    /// Static tags merged into every entry before batching (`global_tag`).
    global_tag: HashMap<String, Value>,
}

/// Delivers batches to the CLS `/structuredlog` endpoint.
struct ClsFlusher {
    client: Arc<OutboundClient>,
    scheme: String,
    host: String,
    topic: String,
    secret_id: String,
    secret_key: String,
    ssl_verify: bool,
    timeout: Duration,
}

impl TencentCloudClsPlugin {
    /// Builds the plugin from node config.
    ///
    /// Accepted keys:
    /// - `cls_host` (alias `endpoint`) (string, **required**): CLS upload host,
    ///   e.g. `ap-guangzhou.cls.tencentcs.com`.
    /// - `cls_topic` (alias `topic_id`) (string, **required**): destination
    ///   topic id, sent as the `topic_id` query parameter.
    /// - `secret_id` / `secret_key` (strings, **required**): API credentials.
    /// - `scheme` (`http`|`https`, default `https`).
    /// - `ssl_verify` (bool, default `true`): verify TLS certificates.
    /// - `timeout` (integer ms, default `10000`): per-flush HTTP deadline.
    /// - `global_tag` (object, optional): fields merged into every entry.
    /// - `include_req_body` / `include_resp_body` (bool, default `false`).
    /// - `log_format` (object, optional): custom flat entry.
    /// - Batch tuning (see [`BatchConfig`]).
    ///
    /// ```yaml
    /// type: tencent-cloud-cls
    /// config:
    ///   cls_host: ap-guangzhou.cls.tencentcs.com
    ///   cls_topic: xxxxxxxx-xxxx-xxxx
    ///   secret_id: ${CLS_SECRET_ID}
    ///   secret_key: ${CLS_SECRET_KEY}
    ///   scheme: https
    /// ```
    pub fn from_config(
        config: &HashMap<String, Value>,
        resources: &Arc<PluginResources>,
    ) -> Result<Self, String> {
        let host = string_alias(config, "cls_host", "endpoint")
            .ok_or_else(|| "tencent-cloud-cls requires 'cls_host'".to_string())?;
        let topic = string_alias(config, "cls_topic", "topic_id")
            .ok_or_else(|| "tencent-cloud-cls requires 'cls_topic'".to_string())?;
        let secret_id = required_string(config, "secret_id")?;
        let secret_key = required_string(config, "secret_key")?;

        let scheme = config
            .get("scheme")
            .and_then(|v| v.as_str())
            .filter(|s| *s == "http" || *s == "https")
            .unwrap_or("https")
            .to_string();
        let ssl_verify = config
            .get("ssl_verify")
            .and_then(|v| v.as_bool())
            .unwrap_or(true);
        let timeout = Duration::from_millis(
            config
                .get("timeout")
                .and_then(|v| v.as_u64())
                .unwrap_or(10000),
        );

        let global_tag = config
            .get("global_tag")
            .and_then(|v| v.as_object())
            .map(|m| m.clone().into_iter().collect())
            .unwrap_or_default();

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
        let flusher = Arc::new(ClsFlusher {
            client: resources.outbound.clone(),
            scheme,
            host,
            topic,
            secret_id,
            secret_key,
            ssl_verify,
            timeout,
        });
        let sink = BatchSink::spawn("tencent-cloud-cls", batch_cfg, flusher);

        Ok(Self {
            sink,
            log_format,
            include_req_body,
            include_resp_body,
            global_tag,
        })
    }
}

fn required_string(config: &HashMap<String, Value>, key: &str) -> Result<String, String> {
    config
        .get(key)
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .ok_or_else(|| format!("tencent-cloud-cls requires '{key}'"))
}

fn string_alias(config: &HashMap<String, Value>, primary: &str, alias: &str) -> Option<String> {
    config
        .get(primary)
        .or_else(|| config.get(alias))
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
}

/// Builds the CLS structured-log payload: one `LogGroup` whose `logs` carry
/// the normalized `{key, value}` `contents` of each entry. Pure and
/// network-free for testing.
fn build_log_payload(entries: &[Value], now_ms: u64) -> Value {
    let logs: Vec<Value> = entries
        .iter()
        .map(|entry| {
            let contents: Vec<Value> = match entry.as_object() {
                Some(map) => map
                    .iter()
                    .map(|(k, v)| serde_json::json!({ "key": k, "value": stringify(v) }))
                    .collect(),
                None => vec![serde_json::json!({ "key": "log", "value": stringify(entry) })],
            };
            serde_json::json!({ "time": now_ms, "contents": contents })
        })
        .collect();
    serde_json::json!({ "logGroupList": [ { "logs": logs } ] })
}

/// CLS `content.value` is a string; keep strings as-is, JSON-encode the rest.
fn stringify(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Builds the CLS `Authorization` header value for the current time.
/// Faithful port of `cls-sdk.lua`'s `sign()`. `cur_time` (Unix seconds) is
/// injected so the signature is unit-testable.
fn sign(secret_id: &str, secret_key: &str, cur_time: u64) -> String {
    let http_request_info = format!("post\n{CLS_API_PATH}\n\n\n");
    let sign_time = format!("{};{}", cur_time, cur_time + AUTH_EXPIRE_SECS);
    let string_to_sign = format!(
        "sha1\n{sign_time}\n{}\n",
        sha1_hex(http_request_info.as_bytes())
    );

    let sign_key = hmac_sha1_hex(secret_key.as_bytes(), sign_time.as_bytes());
    let signature = hmac_sha1_hex(sign_key.as_bytes(), string_to_sign.as_bytes());

    [
        "q-sign-algorithm=sha1".to_string(),
        format!("q-ak={secret_id}"),
        format!("q-sign-time={sign_time}"),
        format!("q-key-time={sign_time}"),
        "q-header-list=".to_string(),
        "q-url-param-list=".to_string(),
        format!("q-signature={signature}"),
    ]
    .join("&")
}

/// Lowercase hex SHA-1 (`str_util.to_hex(ngx_sha1_bin(msg))`).
fn sha1_hex(msg: &[u8]) -> String {
    let digest = ring::digest::digest(&ring::digest::SHA1_FOR_LEGACY_USE_ONLY, msg);
    to_hex(digest.as_ref())
}

/// Lowercase hex HMAC-SHA1 (`str_util.to_hex(ngx_hmac_sha1(key, msg))`).
fn hmac_sha1_hex(key: &[u8], msg: &[u8]) -> String {
    let k = ring::hmac::Key::new(ring::hmac::HMAC_SHA1_FOR_LEGACY_USE_ONLY, key);
    to_hex(ring::hmac::sign(&k, msg).as_ref())
}

fn to_hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

#[async_trait]
impl BatchFlusher for ClsFlusher {
    async fn flush(&self, entries: &[Value]) -> Result<(), FlushError> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        let now_ms = now.as_millis() as u64;
        let payload = build_log_payload(entries, now_ms);
        let body = serde_json::to_vec(&payload).map_err(|e| FlushError {
            message: format!("cls payload encode failed: {e}"),
            first_fail: None,
        })?;

        let authorization = sign(&self.secret_id, &self.secret_key, now.as_secs());
        let url = format!(
            "{}://{}{}?topic_id={}",
            self.scheme, self.host, CLS_API_PATH, self.topic
        );
        let headers = vec![
            ("Host".to_string(), self.host.clone()),
            ("Content-Type".to_string(), "application/json".to_string()),
            ("Authorization".to_string(), authorization),
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
            Ok(resp) if resp.status == 200 => Ok(()),
            // 413/404/401/403 are non-retryable per the SDK; treat as delivered.
            Ok(resp) if matches!(resp.status, 401 | 403 | 404 | 413) => {
                tracing::error!(
                    status = resp.status,
                    "tencent-cloud-cls non-retryable error, dropping batch"
                );
                Ok(())
            }
            Ok(resp) => Err(FlushError {
                message: format!(
                    "cls returned status {}: {}",
                    resp.status,
                    String::from_utf8_lossy(&resp.body)
                ),
                first_fail: None,
            }),
            Err(e) => Err(FlushError {
                message: format!("cls callout failed: {e}"),
                first_fail: None,
            }),
        }
    }
}

#[async_trait]
impl Plugin for TencentCloudClsPlugin {
    fn plugin_type(&self) -> &str {
        "tencent-cloud-cls"
    }

    async fn execute(&self, ctx: Context, _named_inputs: &HashMap<String, Value>) -> PluginResult {
        let mut entry = build_entry(
            &ctx,
            self.log_format.as_ref(),
            self.include_req_body,
            self.include_resp_body,
        );
        if !self.global_tag.is_empty() {
            if let Some(map) = entry.as_object_mut() {
                for (k, v) in &self.global_tag {
                    map.insert(k.clone(), v.clone());
                }
            }
        }
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
            "cls_host": "ap-guangzhou.cls.tencentcs.com",
            "cls_topic": "topic-123",
            "secret_id": "id",
            "secret_key": "secret"
        })
    }

    #[tokio::test]
    async fn requires_host_topic_and_keys() {
        assert!(TencentCloudClsPlugin::from_config(
            &cfg(json!({ "cls_topic": "t", "secret_id": "i", "secret_key": "k" })),
            &PluginResources::empty()
        )
        .is_err());
        assert!(
            TencentCloudClsPlugin::from_config(&cfg(full_cfg()), &PluginResources::empty()).is_ok()
        );
    }

    #[tokio::test]
    async fn accepts_endpoint_and_topic_id_aliases() {
        assert!(TencentCloudClsPlugin::from_config(
            &cfg(json!({
                "endpoint": "h", "topic_id": "t", "secret_id": "i", "secret_key": "k"
            })),
            &PluginResources::empty()
        )
        .is_ok());
    }

    #[test]
    fn sha1_known_vector() {
        assert_eq!(sha1_hex(b"abc"), "a9993e364706816aba3e25717850c26c9cd0d89d");
        assert_eq!(sha1_hex(b""), "da39a3ee5e6b4b0d3255bfef95601890afd80709");
    }

    #[test]
    fn hmac_sha1_rfc2202_vector() {
        // RFC 2202 test case 2: key = "Jefe", data = "what do ya want ...".
        let mac = hmac_sha1_hex(b"Jefe", b"what do ya want for nothing?");
        assert_eq!(mac, "effcdf6ae5eb2fa2d27416d5f184df9c259a7c79");
    }

    #[test]
    fn sign_is_stable_and_shaped() {
        let a = sign("AKID", "AKSECRET", 1_600_000_000);
        let b = sign("AKID", "AKSECRET", 1_600_000_000);
        assert_eq!(a, b);
        assert!(a.starts_with("q-sign-algorithm=sha1&q-ak=AKID"));
        assert!(a.contains("q-sign-time=1600000000;1600000060"));
        // Stable signature (recomputed vector).
        assert!(
            a.ends_with("&q-signature=690a6e12e797585ceb04a4f21fd5e3886f997972"),
            "unexpected signature: {a}"
        );
    }

    #[test]
    fn log_payload_contents_shape() {
        let entries = vec![json!({ "status": 200, "path": "/x" })];
        let payload = build_log_payload(&entries, 1234);
        let log = &payload["logGroupList"][0]["logs"][0];
        assert_eq!(log["time"], json!(1234));
        let contents = log["contents"].as_array().unwrap();
        assert_eq!(contents.len(), 2);
        // values are all strings
        for c in contents {
            assert!(c["value"].is_string());
        }
    }
}
