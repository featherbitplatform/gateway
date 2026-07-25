//! The `google-cloud-logging` node — ships access-log entries to
//! [Google Cloud Logging](https://cloud.google.com/logging) in batches.
//!
//! Ported from APISIX's `google-cloud-logging.lua`. On the request path the
//! node builds a log entry (via the shared
//! [`build_entry`](crate::plugins::util::log_entry::build_entry)) and hands it
//! to a [`BatchSink`]. A background task wraps the buffered entries into the
//! Cloud Logging `entries:write` payload and POSTs them to
//! `https://logging.googleapis.com/v2/entries:write` with an OAuth2 bearer
//! token. The node passes the context through unchanged.
//!
//! ## Authentication (service-account JWT → OAuth2 access token)
//! Cloud Logging is called with a short-lived OAuth2 access token obtained via
//! the service-account JWT-bearer grant:
//! 1. A JWT is assembled and RS256-signed with the service account's
//!    `private_key` — claims `iss` = `client_email`, `scope` = space-joined
//!    scopes, `aud` = `token_uri`, `iat`/`exp` (1h lifetime).
//! 2. The JWT is POSTed to `token_uri` as
//!    `grant_type=urn:ietf:params:oauth:grant-type:jwt-bearer&assertion=<jwt>`.
//! 3. The returned `access_token` is **cached** ([`tokio::sync::Mutex`]) and
//!    reused until ~60s before its `expires_in`, then refreshed.
//!
//! ## Deviations from APISIX
//! - APISIX derives per-entry `httpRequest`/`insertId` fields and merges
//!   `log_format_extra`. featherbit uses the shared log entry as the
//!   `jsonPayload` and omits `httpRequest`/`insertId`; use `log_format` to
//!   shape the payload.
//! - `log_id` defaults to `featherbit%2Flogs` (APISIX: `apisix.apache.org%2Flogs`).
//! - `resource` defaults to `{"type":"global"}`.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use jsonwebtoken::{encode, Algorithm, EncodingKey, Header};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::Mutex;
use tokio::time::Instant;

use crate::batch::{BatchConfig, BatchFlusher, BatchSink, FlushError};
use crate::context::Context;
use crate::outbound::{OutboundClient, OutboundRequest};
use crate::plugins::resources::PluginResources;
use crate::plugins::util::log_entry::{build_entry, parse_log_format};
use crate::plugins::{Plugin, PluginOutput, PluginResult};

const DEFAULT_TOKEN_URI: &str = "https://oauth2.googleapis.com/token";
const DEFAULT_ENTRIES_URI: &str = "https://logging.googleapis.com/v2/entries:write";
const DEFAULT_SCOPES: &[&str] = &[
    "https://www.googleapis.com/auth/logging.write",
    "https://www.googleapis.com/auth/cloud-platform",
];

/// Ships log entries to Google Cloud Logging in batches.
pub struct GoogleCloudLoggingPlugin {
    sink: BatchSink,
    log_format: Option<HashMap<String, Value>>,
    include_req_body: bool,
    include_resp_body: bool,
}

/// Service-account credentials resolved from `auth_config` or `auth_file`.
#[derive(Clone)]
struct AuthConfig {
    client_email: String,
    private_key: String,
    project_id: String,
    token_uri: String,
    scopes: Vec<String>,
}

/// Caches the OAuth2 access token and refreshes it near expiry.
struct TokenManager {
    auth: AuthConfig,
    client: Arc<OutboundClient>,
    ssl_verify: bool,
    timeout: Duration,
    cached: Mutex<Option<CachedToken>>,
}

struct CachedToken {
    token: String,
    /// When the token should be considered stale (already minus a safety margin).
    refresh_at: Instant,
}

/// Delivers batched log entries to the Cloud Logging `entries:write` endpoint.
struct GoogleCloudFlusher {
    client: Arc<OutboundClient>,
    tokens: TokenManager,
    entries_uri: String,
    log_name: String,
    resource: Value,
    ssl_verify: bool,
    timeout: Duration,
}

impl GoogleCloudLoggingPlugin {
    /// Builds the plugin from node config.
    ///
    /// Config keys:
    ///
    /// | Key | Type | Default | Description |
    /// |---|---|---|---|
    /// | `auth_config` | object | — | Inline service account: `client_email`, `private_key`, `project_id` (all required), optional `token_uri`, `scopes`. |
    /// | `auth_file` | string | — | Path to a service-account JSON file (used when `auth_config` is absent). |
    /// | `resource` | object | `{"type":"global"}` | [MonitoredResource](https://cloud.google.com/logging/docs/reference/v2/rest/v2/MonitoredResource) attached to each entry. |
    /// | `log_id` | string | `featherbit%2Flogs` | Log id; the `logName` becomes `projects/<project_id>/logs/<log_id>`. |
    /// | `ssl_verify` | bool | `true` | Verify Google TLS certificates. |
    /// | `timeout` | int (seconds) | `10` | Per-call HTTP timeout (token fetch and write). |
    /// | `log_format` | object | — | Custom `name -> "$var template"` entry used as the `jsonPayload`. |
    /// | `include_req_body` / `include_resp_body` | bool | `false` | Include bodies in the default entry. |
    ///
    /// Either `auth_config` (with `client_email` + `private_key` + `project_id`)
    /// or `auth_file` is required. Batch keys follow
    /// [`BatchConfig::from_config`].
    ///
    /// ```yaml
    /// type: google-cloud-logging
    /// config:
    ///   auth_config:
    ///     client_email: logger@my-project.iam.gserviceaccount.com
    ///     private_key: |
    ///       -----BEGIN PRIVATE KEY-----
    ///       ...
    ///       -----END PRIVATE KEY-----
    ///     project_id: my-project
    ///   log_id: featherbit%2Flogs
    /// ```
    pub fn from_config(
        config: &HashMap<String, Value>,
        resources: &Arc<PluginResources>,
    ) -> Result<Self, String> {
        let auth = resolve_auth(config)?;

        let ssl_verify = config
            .get("ssl_verify")
            .and_then(|v| v.as_bool())
            .unwrap_or(true);
        let timeout =
            Duration::from_secs(config.get("timeout").and_then(|v| v.as_u64()).unwrap_or(10));
        let log_id = config
            .get("log_id")
            .and_then(|v| v.as_str())
            .unwrap_or("featherbit%2Flogs");
        let resource = config
            .get("resource")
            .cloned()
            .unwrap_or_else(|| json!({ "type": "global" }));
        let entries_uri = config
            .get("entries_uri")
            .and_then(|v| v.as_str())
            .unwrap_or(DEFAULT_ENTRIES_URI)
            .to_string();

        let log_name = format!("projects/{}/logs/{}", auth.project_id, log_id);

        let log_format = parse_log_format(config)?;
        let include_req_body = config
            .get("include_req_body")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let include_resp_body = config
            .get("include_resp_body")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        let batch_cfg =
            BatchConfig::from_config(config).map_err(|e| format!("google-cloud-logging: {e}"))?;

        let flusher = Arc::new(GoogleCloudFlusher {
            client: resources.outbound.clone(),
            tokens: TokenManager {
                auth,
                client: resources.outbound.clone(),
                ssl_verify,
                timeout,
                cached: Mutex::new(None),
            },
            entries_uri,
            log_name,
            resource,
            ssl_verify,
            timeout,
        });
        let sink = BatchSink::spawn("google-cloud-logging", batch_cfg, flusher);

        Ok(Self {
            sink,
            log_format,
            include_req_body,
            include_resp_body,
        })
    }
}

/// Resolves the service-account credentials from `auth_config` (inline) or
/// `auth_file` (path to a JSON file). `client_email`, `private_key`, and
/// `project_id` are required.
fn resolve_auth(config: &HashMap<String, Value>) -> Result<AuthConfig, String> {
    let obj: serde_json::Map<String, Value> = match config.get("auth_config") {
        Some(Value::Object(m)) => m.clone(),
        _ => {
            let path = config
                .get("auth_file")
                .and_then(|v| v.as_str())
                .ok_or("google-cloud-logging: `auth_config` or `auth_file` is required")?;
            let content = std::fs::read_to_string(path).map_err(|e| {
                format!("google-cloud-logging: failed to read auth_file `{path}`: {e}")
            })?;
            serde_json::from_str::<serde_json::Map<String, Value>>(&content).map_err(|e| {
                format!("google-cloud-logging: auth_file `{path}` is not a JSON object: {e}")
            })?
        }
    };

    let get_str = |key: &str| obj.get(key).and_then(|v| v.as_str()).map(str::to_string);

    let client_email = get_str("client_email")
        .filter(|s| !s.is_empty())
        .ok_or("google-cloud-logging: `client_email` is required")?;
    let private_key = get_str("private_key")
        .filter(|s| !s.is_empty())
        .ok_or("google-cloud-logging: `private_key` is required")?;
    let project_id = get_str("project_id")
        .filter(|s| !s.is_empty())
        .ok_or("google-cloud-logging: `project_id` is required")?;
    let token_uri = get_str("token_uri")
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| DEFAULT_TOKEN_URI.to_string());

    // Accept `scopes` (preferred) or `scope`, as an array of strings.
    let scopes = obj
        .get("scopes")
        .or_else(|| obj.get("scope"))
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect::<Vec<_>>()
        })
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| DEFAULT_SCOPES.iter().map(|s| s.to_string()).collect());

    Ok(AuthConfig {
        client_email,
        private_key,
        project_id,
        token_uri,
        scopes,
    })
}

/// Assembles and RS256-signs the service-account JWT assertion.
fn build_jwt(
    client_email: &str,
    scope: &str,
    token_uri: &str,
    now_secs: u64,
    private_key_pem: &str,
) -> Result<String, String> {
    let claims = json!({
        "iss": client_email,
        "scope": scope,
        "aud": token_uri,
        "iat": now_secs,
        "exp": now_secs + 3600,
    });
    let key = EncodingKey::from_rsa_pem(private_key_pem.as_bytes())
        .map_err(|e| format!("invalid service-account private_key: {e}"))?;
    encode(&Header::new(Algorithm::RS256), &claims, &key)
        .map_err(|e| format!("failed to sign service-account JWT: {e}"))
}

/// Wraps one log entry into a Cloud Logging `LogEntry`.
fn build_log_entry(entry: &Value, log_name: &str, resource: &Value, timestamp: &str) -> Value {
    json!({
        "logName": log_name,
        "resource": resource,
        "jsonPayload": entry,
        "timestamp": timestamp,
        "labels": { "source": "featherbit-google-cloud-logging" },
    })
}

/// Builds the full `entries:write` request payload.
fn build_write_payload(
    entries: &[Value],
    log_name: &str,
    resource: &Value,
    timestamp: &str,
) -> Value {
    let wrapped: Vec<Value> = entries
        .iter()
        .map(|e| build_log_entry(e, log_name, resource, timestamp))
        .collect();
    json!({ "entries": wrapped, "partialSuccess": false })
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default)]
    expires_in: Option<u64>,
}

impl TokenManager {
    /// Returns a valid access token, refreshing via the JWT-bearer grant when
    /// the cached one is absent or near expiry.
    async fn access_token(&self) -> Result<String, String> {
        let mut guard = self.cached.lock().await;
        if let Some(cached) = guard.as_ref() {
            if Instant::now() < cached.refresh_at {
                return Ok(cached.token.clone());
            }
        }

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let scope = self.auth.scopes.join(" ");
        let assertion = build_jwt(
            &self.auth.client_email,
            &scope,
            &self.auth.token_uri,
            now,
            &self.auth.private_key,
        )?;

        // grant_type value percent-encoded; the JWT assertion is base64url-safe.
        let body = format!(
            "grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Ajwt-bearer&assertion={assertion}"
        );
        let req = OutboundRequest {
            method: http::Method::POST,
            url: self.auth.token_uri.clone(),
            headers: vec![(
                "Content-Type".to_string(),
                "application/x-www-form-urlencoded".to_string(),
            )],
            body: body.into_bytes().into(),
            timeout: self.timeout,
            ssl_verify: self.ssl_verify,
        };

        let resp = self
            .client
            .request(req)
            .await
            .map_err(|e| format!("token request failed: {e}"))?;
        if resp.status != 200 {
            return Err(format!(
                "token endpoint returned status {}: {}",
                resp.status,
                String::from_utf8_lossy(&resp.body)
            ));
        }
        let parsed: TokenResponse = serde_json::from_slice(&resp.body)
            .map_err(|e| format!("failed to parse token response: {e}"))?;

        // Refresh ~60s before expiry; default lifetime 3600s.
        let ttl = parsed.expires_in.unwrap_or(3600).saturating_sub(60).max(1);
        *guard = Some(CachedToken {
            token: parsed.access_token.clone(),
            refresh_at: Instant::now() + Duration::from_secs(ttl),
        });
        Ok(parsed.access_token)
    }
}

/// RFC3339 UTC timestamp (`YYYY-MM-DDTHH:MM:SSZ`) from a unix second count.
fn rfc3339_zulu(unix_secs: u64) -> String {
    let days = (unix_secs / 86400) as i64;
    let rem = unix_secs % 86400;
    let (hh, mm, ss) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    let (y, m, d) = civil_from_days(days);
    format!("{y:04}-{m:02}-{d:02}T{hh:02}:{mm:02}:{ss:02}Z")
}

/// Howard Hinnant's civil-from-days: days since the unix epoch → (year, month, day).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[async_trait]
impl BatchFlusher for GoogleCloudFlusher {
    async fn flush(&self, entries: &[Value]) -> Result<(), FlushError> {
        let token = self.tokens.access_token().await.map_err(|e| FlushError {
            message: format!("failed to obtain access token: {e}"),
            first_fail: None,
        })?;

        let timestamp = rfc3339_zulu(now_secs());
        let payload = build_write_payload(entries, &self.log_name, &self.resource, &timestamp);
        let body = serde_json::to_vec(&payload).map_err(|e| FlushError {
            message: format!("failed to encode entries:write payload: {e}"),
            first_fail: None,
        })?;

        let req = OutboundRequest {
            method: http::Method::POST,
            url: self.entries_uri.clone(),
            headers: vec![
                ("Content-Type".to_string(), "application/json".to_string()),
                ("Authorization".to_string(), format!("Bearer {token}")),
            ],
            body: body.into(),
            timeout: self.timeout,
            ssl_verify: self.ssl_verify,
        };

        match self.client.request(req).await {
            Ok(resp) if resp.status == 200 => Ok(()),
            Ok(resp) => Err(FlushError {
                message: format!(
                    "google cloud logging returned status {}: {}",
                    resp.status,
                    String::from_utf8_lossy(&resp.body)
                ),
                first_fail: None,
            }),
            Err(e) => Err(FlushError {
                message: e.to_string(),
                first_fail: None,
            }),
        }
    }
}

#[async_trait]
impl Plugin for GoogleCloudLoggingPlugin {
    fn plugin_type(&self) -> &str {
        "google-cloud-logging"
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
    use base64::Engine;

    // Test RSA private key (PKCS#8), reused from the openid_connect tests.
    const PRIV_PEM: &str = "-----BEGIN PRIVATE KEY-----\n\
MIIEvAIBADANBgkqhkiG9w0BAQEFAASCBKYwggSiAgEAAoIBAQCvciOuri5uG88q\n\
rZ3T6qUhTYl7nWDHvVGBBsA8ku3xUfOW97PGpWbTe/Yq/3jovVxAQsAe/QoIMyUU\n\
HKCdDKAsIBO9j9OEPs3Le6cThFx+/9Z1U9cw4wCIa4TNtGBhyDgqbqKpOLNnXLI6\n\
WEcrykkoV5nUUH/47aS2i9BiqZn6H9eEL1VH82IX/x4fWNIEyXQAxKZtyULgznR4\n\
oUz2QPaY/cWtpK85B12scs1IpLnzEdjy69t28ZQnYZ7Nrvl+aFjSkvnqxhoNJ9Ut\n\
Lw2/3vld8t3Lh6B4vTM4vdJsue1dum6WnyEKEx/SDuCSDxWONfmdhu/B4XUghaQS\n\
1wBNiEhvAgMBAAECggEASXDcee8ktWfDsShK9F35MLcd0VaAICxiFUInr1OL8ePt\n\
tSjMIt+y6t0tnzMgwEAgATBP7sjabbNHFqOjIgqac84bpVKy5l1J1R9WQWe7NlhO\n\
w/9MCYVEgFaNmXQjklr3E+ALDA4VnzNg0eaJKE39kLsWxBbMcv27YMSm/t3i/B2s\n\
rwZbzBgxXXR5r7j/Tt+hRJmGHXe0zZvsNLzFNj4CsyngBiY9CIcexroGxd3yGEf7\n\
0PKHwbZKkH0CPr6QAc4f+tPgIfHB+8+29QPrUTR9e60Sc6dZNUjTr1EWIxyvFxVK\n\
dI3ekR5W26a81+yxc2MpRK8wZsv+mJ6okaeVs2+3jQKBgQDr0b3YX4RC9trW+RsE\n\
9wUXeLr3o9Vb0FTHf/8ALAZ9EWywEmF+sdA8fKs8+H+IyIzX6KGw/UbzqIi2aDuJ\n\
q63IPxKyyXr7nfVSUz8qWIGT/WoG/1d4rpFN2sbR/r/oue7uJnaXMIPswVT+zO8q\n\
5YieEPDwhteJ8bJUC16NWwddBQKBgQC+dcEmNm7MzxI/cuwubkojhayXw1ouACu4\n\
giGp3lJywzIAnV1CsJTGTpvHk31j+/L9oB2U/586+65MGklGJ2TGs0IQZs0iAy1H\n\
Oq3zzsLp0KiVizyqchgkIWP6KVpx5aPkpJSgPJGyJzuwofZwRzPK7IZr8c4MOtsy\n\
M8j8up8p4wKBgGbUxTYvIJuazX7kjXWyydOcX9tQ497vj6iXFflbOVEcYgq9WSpI\n\
G4fkzT7/FY3t9gzIcomdSG1D1qnD9gJojJU/e8XeufQywyEtD+RFR+vim3OFsPz9\n\
EnuipQQ5VDIFsjzDJP90tnJtM8UQVFKeWN6kgIxCIIcUkDC57HczdJiJAoGASPG4\n\
g/YdAXvdNUfChRXgdzJfI9DB3RRbqlLMqc5oLWPs5qdebIhMspawuwMV5xE7wz9r\n\
lQFB7sktvB/lKGU2B5PoHXgB4KDu2nTy4omxxPMRXhTxqyX/cPcI32qvJSgaWRtf\n\
gO8xrdWw2rltNRtQDsv/v5/glnaENPn4ZDLlepkCgYAqag5Uxj0ps6WNE/D6IEWA\n\
eTGicEEJPJQB9bGrElna7WyOjntnO5miRmpM1jH39R417czBURmvZHO2oTnqghZF\n\
c/7P2kweQNU7vtM/iLcm8EyFRw2lVB3J/XVTEcPU6ZeZHlVbGtiKx3gukkMBc4Ct\n\
CQTyrvDSz5J6MQhLtbNHnQ==\n\
-----END PRIVATE KEY-----\n";

    fn cfg(pairs: &[(&str, Value)]) -> HashMap<String, Value> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect()
    }

    fn auth_config_value() -> Value {
        json!({
            "client_email": "logger@proj.iam.gserviceaccount.com",
            "private_key": PRIV_PEM,
            "project_id": "my-project",
        })
    }

    #[test]
    fn from_config_requires_auth() {
        let res = GoogleCloudLoggingPlugin::from_config(&HashMap::new(), &PluginResources::empty());
        let Err(e) = res else {
            panic!("expected error")
        };
        assert!(e.contains("auth_config"));
    }

    #[test]
    fn from_config_requires_core_auth_fields() {
        for missing in ["client_email", "private_key", "project_id"] {
            let mut auth = auth_config_value();
            auth.as_object_mut().unwrap().remove(missing);
            let c = cfg(&[("auth_config", auth)]);
            let res = GoogleCloudLoggingPlugin::from_config(&c, &PluginResources::empty());
            let Err(e) = res else {
                panic!("expected error when `{missing}` missing")
            };
            assert!(e.contains(missing));
        }
    }

    #[tokio::test]
    async fn from_config_ok_and_defaults() {
        let c = cfg(&[("auth_config", auth_config_value())]);
        let auth = resolve_auth(&c).unwrap();
        assert_eq!(auth.token_uri, DEFAULT_TOKEN_URI);
        assert_eq!(auth.scopes, DEFAULT_SCOPES);
        assert!(GoogleCloudLoggingPlugin::from_config(&c, &PluginResources::empty()).is_ok());
    }

    #[test]
    fn jwt_is_signed_from_private_key() {
        let jwt = build_jwt(
            "logger@proj.iam.gserviceaccount.com",
            "scope-a scope-b",
            DEFAULT_TOKEN_URI,
            1_700_000_000,
            PRIV_PEM,
        )
        .unwrap();

        let parts: Vec<&str> = jwt.split('.').collect();
        assert_eq!(parts.len(), 3, "a JWT has three dot-separated segments");

        let engine = base64::engine::general_purpose::URL_SAFE_NO_PAD;
        let header: Value = serde_json::from_slice(&engine.decode(parts[0]).unwrap()).unwrap();
        assert_eq!(header["alg"], "RS256");
        assert_eq!(header["typ"], "JWT");

        let claims: Value = serde_json::from_slice(&engine.decode(parts[1]).unwrap()).unwrap();
        assert_eq!(claims["iss"], "logger@proj.iam.gserviceaccount.com");
        assert_eq!(claims["scope"], "scope-a scope-b");
        assert_eq!(claims["aud"], DEFAULT_TOKEN_URI);
        assert_eq!(claims["iat"], 1_700_000_000u64);
        assert_eq!(claims["exp"], 1_700_003_600u64);
    }

    #[test]
    fn write_payload_shape() {
        let entries = vec![
            json!({ "request": { "method": "GET" } }),
            json!({ "request": { "method": "POST" } }),
        ];
        let resource = json!({ "type": "global" });
        let payload = build_write_payload(
            &entries,
            "projects/my-project/logs/featherbit%2Flogs",
            &resource,
            "2023-11-14T22:13:20Z",
        );
        assert_eq!(payload["partialSuccess"], false);
        let arr = payload["entries"].as_array().unwrap();
        assert_eq!(arr.len(), 2);
        assert_eq!(
            arr[0]["logName"],
            "projects/my-project/logs/featherbit%2Flogs"
        );
        assert_eq!(arr[0]["resource"]["type"], "global");
        assert_eq!(arr[0]["jsonPayload"]["request"]["method"], "GET");
        assert_eq!(arr[0]["timestamp"], "2023-11-14T22:13:20Z");
        assert_eq!(
            arr[0]["labels"]["source"],
            "featherbit-google-cloud-logging"
        );
        assert_eq!(arr[1]["jsonPayload"]["request"]["method"], "POST");
    }

    #[test]
    fn rfc3339_conversion() {
        assert_eq!(rfc3339_zulu(1_700_000_000), "2023-11-14T22:13:20Z");
        assert_eq!(rfc3339_zulu(0), "1970-01-01T00:00:00Z");
    }
}
