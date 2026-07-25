//! HMAC request-signing authentication plugin (`hmac-auth`).
//!
//! Port of APISIX's `hmac-auth` plugin (3.17). A client proves possession of a
//! shared `secret_key` by signing a canonical *signing string* built from the
//! request and sending the base64 signature alongside the `access_key` that
//! identifies the credential. featherbit recomputes the signature with the
//! matching secret and compares; a mismatch, an unknown key, a stale `Date`,
//! or a missing required signed header is rejected as `401 HMAC_INVALID`
//! through the node's error port.
//!
//! # Wire format
//!
//! The signature parameters are read from either:
//!
//! - an `Authorization` header of the form
//!   `Signature keyId="<access_key>",algorithm="hmac-sha256",headers="date @request-target",signature="<base64>"`
//!   (APISIX 3.17's format — `keyId` is the featherbit `access_key`), or
//! - the discrete headers `X-HMAC-ACCESS-KEY`, `X-HMAC-ALGORITHM`,
//!   `X-HMAC-SIGNED-HEADERS` (space-separated), and `X-HMAC-SIGNATURE`.
//!
//! The `Date` header (RFC 1123 / GMT) carries the timestamp checked against
//! `clock_skew`.
//!
//! # Signing string
//!
//! Mirrors APISIX's `generate_signature`: the access key on the first line,
//! then one line per signed header, terminated by a trailing newline:
//!
//! ```text
//! <access_key>\n
//! <h1>: <value1>\n
//! <h2>: <value2>\n
//! ```
//!
//! The pseudo-header `@request-target` is rendered as `<METHOD> <request-uri>`
//! instead of a header lookup. `signature = base64(HMAC(secret_key, signing_string))`.
//!
//! # Deviations from APISIX
//!
//! - Consumer credentials use the field names `access_key` / `secret_key`
//!   (featherbit's `hmac-auth` consumer index is keyed on `access_key`),
//!   whereas APISIX names them `key_id` / `secret_key`.
//! - A single `algorithm` is accepted per node (default `hmac-sha256`) rather
//!   than APISIX's `allowed_algorithms` list; the client's declared algorithm
//!   must match it.
//! - `@request-target`'s request URI is reconstructed from the parsed path
//!   plus a **sorted** `key=value` query string (the original query byte order
//!   is not retained), so a client signing `@request-target` must canonicalise
//!   its query the same way.
//! - Only the RFC 1123 (`Sun, 06 Nov 1994 08:49:37 GMT`) `Date` format is
//!   parsed for clock-skew checks.
//! - Request-body digest validation (`validate_request_body`) is not
//!   implemented.

use async_trait::async_trait;
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use bytes::Bytes;
use ring::hmac;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::consumers::attach_consumer;
use crate::context::{Context, GatewayError};
use crate::plugins::resources::PluginResources;
use crate::plugins::{Plugin, PluginExecutionError, PluginOutput, PluginResult};

/// The X-HMAC-* header names (lowercased) used as the alternative to the
/// `Authorization: Signature ...` presentation.
const HDR_ACCESS_KEY: &str = "x-hmac-access-key";
const HDR_ALGORITHM: &str = "x-hmac-algorithm";
const HDR_SIGNED_HEADERS: &str = "x-hmac-signed-headers";
const HDR_SIGNATURE: &str = "x-hmac-signature";

/// Signature parameters extracted from the request.
struct HmacParams {
    access_key: String,
    algorithm: Option<String>,
    signature: String,
    signed_headers: Vec<String>,
}

/// Authenticates requests by verifying an HMAC signature over a canonical
/// signing string.
///
/// With inline `access_key`/`secret_key` a single credential is accepted. With
/// `use_consumers: true` the presented `access_key` is resolved against the
/// gateway's `consumers:` section (their `hmac-auth: {access_key, secret_key}`
/// credentials) and, on a valid signature, the consumer's identity is attached
/// to the request. Both may be enabled together — the inline key is checked
/// first.
pub struct HmacAuthPlugin {
    /// Inline credential access key (the `keyId`), if configured.
    access_key: Option<String>,
    /// Inline secret paired with `access_key`.
    secret_key: Option<String>,
    /// The single algorithm this node accepts.
    algorithm: HmacAlgorithm,
    /// Maximum allowed difference between the `Date` header and now, in
    /// seconds; `0` disables the check.
    clock_skew: u64,
    /// Header names the client MUST have included in its signature.
    signed_headers: Vec<String>,
    /// When true, keys are also resolved against the consumer store.
    use_consumers: bool,
    /// Consumer attached when no credential matches (instead of rejecting).
    anonymous_consumer: Option<String>,
    /// When false (default), the X-HMAC-* proof headers are stripped before
    /// proxying upstream.
    keep_headers: bool,
    /// When true, the `Authorization` header is stripped before proxying.
    hide_credentials: bool,
    /// Realm advertised in the `WWW-Authenticate` challenge.
    realm: String,
    resources: Arc<PluginResources>,
}

/// Supported HMAC algorithms.
#[derive(Clone, Copy, PartialEq)]
enum HmacAlgorithm {
    Sha1,
    Sha256,
    Sha512,
}

impl HmacAlgorithm {
    /// Parses an APISIX-style algorithm name (`hmac-sha1` / `hmac-sha256` /
    /// `hmac-sha512`), defaulting to `hmac-sha256` when absent.
    fn parse(name: Option<&str>) -> Result<Self, String> {
        match name {
            None | Some("hmac-sha256") => Ok(Self::Sha256),
            Some("hmac-sha1") => Ok(Self::Sha1),
            Some("hmac-sha512") => Ok(Self::Sha512),
            Some(other) => Err(format!(
                "Unknown hmac-auth algorithm '{}' — supported: hmac-sha1, hmac-sha256, hmac-sha512",
                other
            )),
        }
    }

    /// Canonical name as it appears on the wire.
    fn name(&self) -> &'static str {
        match self {
            Self::Sha1 => "hmac-sha1",
            Self::Sha256 => "hmac-sha256",
            Self::Sha512 => "hmac-sha512",
        }
    }

    /// The corresponding `ring` HMAC algorithm.
    fn ring_algorithm(&self) -> hmac::Algorithm {
        match self {
            Self::Sha1 => hmac::HMAC_SHA1_FOR_LEGACY_USE_ONLY,
            Self::Sha256 => hmac::HMAC_SHA256,
            Self::Sha512 => hmac::HMAC_SHA512,
        }
    }
}

/// Current unix timestamp in seconds.
fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Parses an RFC 1123 HTTP date (`Sun, 06 Nov 1994 08:49:37 GMT`) into a unix
/// timestamp. Returns `None` on any malformed component.
fn parse_http_date(s: &str) -> Option<i64> {
    // Expected tokens: ["Sun,", "06", "Nov", "1994", "08:49:37", "GMT"]
    let parts: Vec<&str> = s.split_whitespace().collect();
    if parts.len() != 6 {
        return None;
    }
    let day: i64 = parts[1].parse().ok()?;
    let month = match parts[2] {
        "Jan" => 1,
        "Feb" => 2,
        "Mar" => 3,
        "Apr" => 4,
        "May" => 5,
        "Jun" => 6,
        "Jul" => 7,
        "Aug" => 8,
        "Sep" => 9,
        "Oct" => 10,
        "Nov" => 11,
        "Dec" => 12,
        _ => return None,
    };
    let year: i64 = parts[3].parse().ok()?;
    let hms: Vec<&str> = parts[4].split(':').collect();
    if hms.len() != 3 {
        return None;
    }
    let hour: i64 = hms[0].parse().ok()?;
    let minute: i64 = hms[1].parse().ok()?;
    let second: i64 = hms[2].parse().ok()?;

    // days_from_civil (Howard Hinnant's algorithm).
    let y = if month <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let m = month;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;

    Some(days * 86400 + hour * 3600 + minute * 60 + second)
}

impl HmacAuthPlugin {
    /// Builds the plugin from node config.
    ///
    /// Accepted keys:
    /// - `use_consumers` (bool, default `false`): resolve the `access_key`
    ///   against the gateway's `consumers:` section and attach the consumer.
    /// - `access_key` (string, optional): inline single-credential key id.
    /// - `secret_key` (string, required when `access_key` is set): the paired
    ///   secret.
    /// - At least one of `access_key` / `use_consumers` must be provided.
    /// - `algorithm` (string, default `"hmac-sha256"`): one of `hmac-sha1`,
    ///   `hmac-sha256`, `hmac-sha512`.
    /// - `clock_skew` (integer seconds, default `300`): max `Date` drift; `0`
    ///   disables the check.
    /// - `signed_headers` (array of strings, optional): headers the client
    ///   must have signed; a request omitting any is rejected.
    /// - `keep_headers` (bool, default `false`): keep the `X-HMAC-*` proof
    ///   headers when proxying (they are stripped by default).
    /// - `hide_credentials` (bool, default `false`): strip the `Authorization`
    ///   header before proxying.
    /// - `anonymous_consumer` (string, optional): consumer attached when no
    ///   credential matches, instead of rejecting.
    /// - `realm` (string, default `"hmac"`): `WWW-Authenticate` realm.
    ///
    /// ```yaml
    /// type: hmac-auth
    /// config:
    ///   use_consumers: true
    ///   algorithm: hmac-sha256
    ///   clock_skew: 300
    ///   signed_headers: [date]
    /// ```
    pub fn from_config(
        config: &HashMap<String, serde_json::Value>,
        resources: &Arc<PluginResources>,
    ) -> Result<Self, String> {
        let access_key = config
            .get("access_key")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(String::from);

        let secret_key = config
            .get("secret_key")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(String::from);

        let use_consumers = config
            .get("use_consumers")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        if access_key.is_none() && !use_consumers {
            return Err(
                "hmac-auth plugin requires 'access_key'+'secret_key' or 'use_consumers: true'"
                    .to_string(),
            );
        }
        if access_key.is_some() && secret_key.is_none() {
            return Err("hmac-auth plugin: 'access_key' requires 'secret_key'".to_string());
        }

        let algorithm = HmacAlgorithm::parse(config.get("algorithm").and_then(|v| v.as_str()))?;

        let clock_skew = match config.get("clock_skew") {
            None => 300,
            Some(v) => v.as_u64().ok_or_else(|| {
                "hmac-auth: clock_skew must be a non-negative integer".to_string()
            })?,
        };

        let signed_headers: Vec<String> = config
            .get("signed_headers")
            .and_then(|v| v.as_array())
            .map(|seq| {
                seq.iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_lowercase()))
                    .collect()
            })
            .unwrap_or_default();

        let use_flag = |key: &str| config.get(key).and_then(|v| v.as_bool()).unwrap_or(false);

        let anonymous_consumer = config
            .get("anonymous_consumer")
            .and_then(|v| v.as_str())
            .map(String::from);

        let realm = config
            .get("realm")
            .and_then(|v| v.as_str())
            .unwrap_or("hmac")
            .to_string();

        Ok(Self {
            access_key,
            secret_key,
            algorithm,
            clock_skew,
            signed_headers,
            use_consumers,
            anonymous_consumer,
            keep_headers: use_flag("keep_headers"),
            hide_credentials: use_flag("hide_credentials"),
            realm,
            resources: resources.clone(),
        })
    }

    /// Builds the 401 rejection routed through the error port with code
    /// `HMAC_INVALID`.
    fn reject(&self, mut ctx: Context, msg: &str) -> PluginResult {
        ctx.response.status_code = 401;
        ctx.response.body = Bytes::from(format!(
            r#"{{"error": "unauthorized", "message": "{}"}}"#,
            msg
        ));
        ctx.response.headers.insert(
            "content-type".to_string(),
            vec!["application/json".to_string()],
        );
        ctx.response.headers.insert(
            "www-authenticate".to_string(),
            vec![format!("hmac realm=\"{}\"", self.realm)],
        );
        Err(PluginExecutionError {
            context: ctx,
            error: GatewayError {
                node_id: String::new(),
                code: "HMAC_INVALID".to_string(),
                message: msg.to_string(),
                metadata: HashMap::new(),
            },
        })
    }

    /// Reads a single-valued request header (lowercased key).
    fn header<'a>(ctx: &'a Context, name: &str) -> Option<&'a str> {
        ctx.request
            .headers
            .get(name)
            .and_then(|v| v.first())
            .map(|s| s.as_str())
    }

    /// Extracts the signature parameters from the `Authorization: Signature`
    /// header or, failing that, the `X-HMAC-*` headers.
    fn retrieve_params(ctx: &Context) -> Option<HmacParams> {
        if let Some(auth) = Self::header(ctx, "authorization") {
            if let Some(rest) = auth.strip_prefix("Signature ") {
                return Self::parse_authorization(rest);
            }
        }

        // X-HMAC-* header form.
        let access_key = Self::header(ctx, HDR_ACCESS_KEY)?.to_string();
        let signature = Self::header(ctx, HDR_SIGNATURE)?.to_string();
        let algorithm = Self::header(ctx, HDR_ALGORITHM).map(String::from);
        let signed_headers = Self::header(ctx, HDR_SIGNED_HEADERS)
            .map(|s| s.split_whitespace().map(|h| h.to_string()).collect())
            .unwrap_or_default();
        Some(HmacParams {
            access_key,
            algorithm,
            signature,
            signed_headers,
        })
    }

    /// Parses the comma-separated `keyId="..",algorithm="..",headers="..",signature=".."`
    /// field list (the part after `Signature `).
    fn parse_authorization(rest: &str) -> Option<HmacParams> {
        let mut key_id = None;
        let mut algorithm = None;
        let mut signature = None;
        let mut headers = Vec::new();

        for field in rest.split(',') {
            let field = field.trim();
            let Some((k, v)) = field.split_once('=') else {
                continue;
            };
            let value = v.trim().trim_matches('"');
            match k.trim() {
                "keyId" => key_id = Some(value.to_string()),
                "algorithm" => algorithm = Some(value.to_string()),
                "signature" => signature = Some(value.to_string()),
                "headers" => headers = value.split_whitespace().map(|h| h.to_string()).collect(),
                _ => {}
            }
        }

        Some(HmacParams {
            access_key: key_id?,
            algorithm,
            signature: signature?,
            signed_headers: headers,
        })
    }

    /// Reconstructs the request URI (path plus a sorted query string) used for
    /// the `@request-target` pseudo-header.
    fn request_uri(ctx: &Context) -> String {
        if ctx.request.query_params.is_empty() {
            return ctx.request.path.clone();
        }
        let mut pairs: Vec<(String, String)> = Vec::new();
        for (k, vs) in &ctx.request.query_params {
            for v in vs {
                pairs.push((k.clone(), v.clone()));
            }
        }
        pairs.sort();
        let query = pairs
            .iter()
            .map(|(k, v)| format!("{}={}", k, v))
            .collect::<Vec<_>>()
            .join("&");
        format!("{}?{}", ctx.request.path, query)
    }

    /// Builds the canonical signing string from the presented signed headers.
    fn signing_string(&self, ctx: &Context, params: &HmacParams) -> String {
        let mut items = vec![params.access_key.clone()];
        for h in &params.signed_headers {
            if h == "@request-target" {
                items.push(format!("{} {}", ctx.request.method, Self::request_uri(ctx)));
            } else if let Some(value) = Self::header(ctx, &h.to_lowercase()) {
                items.push(format!("{}: {}", h, value));
            }
            // A listed-but-absent real header is skipped (APISIX parity).
        }
        let mut s = items.join("\n");
        s.push('\n');
        s
    }

    /// Verifies the client signature against the recomputed one.
    fn verify_signature(&self, ctx: &Context, params: &HmacParams, secret: &str) -> bool {
        let Ok(sig_bytes) = BASE64.decode(&params.signature) else {
            return false;
        };
        let signing_string = self.signing_string(ctx, params);
        let key = hmac::Key::new(self.algorithm.ring_algorithm(), secret.as_bytes());
        hmac::verify(&key, signing_string.as_bytes(), &sig_bytes).is_ok()
    }

    /// Enforces the algorithm, clock skew, and required signed headers common
    /// to both credential sources. Returns `Err(reason)` on any violation.
    fn validate_common(&self, ctx: &Context, params: &HmacParams) -> Result<(), String> {
        // Algorithm: if the client declared one it must match the node's.
        if let Some(ref algo) = params.algorithm {
            if algo != self.algorithm.name() {
                return Err("Invalid algorithm".to_string());
            }
        }

        // Clock skew against the Date header.
        if self.clock_skew > 0 {
            let date = Self::header(ctx, "date")
                .ok_or("Date header missing, failed to validate clock skew")?;
            let ts = parse_http_date(date).ok_or("Invalid GMT format time")?;
            let diff = (now() as i64 - ts).unsigned_abs();
            if diff > self.clock_skew {
                return Err("Clock skew exceeded".to_string());
            }
        }

        // Required signed headers must all be present in the client's list.
        for required in &self.signed_headers {
            if !params
                .signed_headers
                .iter()
                .any(|h| h.to_lowercase() == *required)
            {
                return Err(format!(
                    "expected header \"{}\" missing in signing",
                    required
                ));
            }
        }

        Ok(())
    }

    /// Strips the proof/credential headers per `keep_headers`/`hide_credentials`.
    fn strip_headers(&self, ctx: &mut Context) {
        if !self.keep_headers {
            ctx.request.headers.remove(HDR_ACCESS_KEY);
            ctx.request.headers.remove(HDR_ALGORITHM);
            ctx.request.headers.remove(HDR_SIGNED_HEADERS);
            ctx.request.headers.remove(HDR_SIGNATURE);
        }
        if self.hide_credentials {
            ctx.request.headers.remove("authorization");
        }
    }
}

#[async_trait]
impl Plugin for HmacAuthPlugin {
    fn plugin_type(&self) -> &str {
        "hmac-auth"
    }

    async fn execute(
        &self,
        mut ctx: Context,
        _named_inputs: &HashMap<String, serde_json::Value>,
    ) -> PluginResult {
        let params = match Self::retrieve_params(&ctx) {
            Some(p) => p,
            None => {
                if self.anonymous_consumer.is_some() {
                    return self.attach_anonymous(ctx);
                }
                return self.reject(ctx, "client request can't be validated");
            }
        };

        if let Err(e) = self.validate_common(&ctx, &params) {
            return self.reject(ctx, &e);
        }

        // Inline credential first.
        if let (Some(ak), Some(sk)) = (&self.access_key, &self.secret_key) {
            if &params.access_key == ak {
                if self.verify_signature(&ctx, &params, sk) {
                    self.strip_headers(&mut ctx);
                    return Ok(PluginOutput {
                        context: ctx,
                        named_outputs: HashMap::new(),
                    });
                }
                return self.reject(ctx, "Invalid signature");
            }
        }

        // Consumer store.
        if self.use_consumers {
            let store = self.resources.consumers.load();
            if let Some(consumer) = store.find_by_credential("hmac-auth", &params.access_key) {
                let secret = consumer
                    .credentials
                    .get("hmac-auth")
                    .and_then(|c| c.get("secret_key"))
                    .and_then(|v| v.as_str());
                if let Some(secret) = secret {
                    if self.verify_signature(&ctx, &params, secret) {
                        self.strip_headers(&mut ctx);
                        attach_consumer(&mut ctx, &consumer, "hmac-auth");
                        return Ok(PluginOutput {
                            context: ctx,
                            named_outputs: HashMap::new(),
                        });
                    }
                }
                return self.reject(ctx, "Invalid signature");
            }
        }

        // Anonymous fallback.
        if self.anonymous_consumer.is_some() {
            return self.attach_anonymous(ctx);
        }

        self.reject(ctx, "Invalid access key")
    }
}

impl HmacAuthPlugin {
    /// Attaches the configured anonymous consumer, or rejects if it is unknown.
    fn attach_anonymous(&self, mut ctx: Context) -> PluginResult {
        if let Some(ref name) = self.anonymous_consumer {
            let store = self.resources.consumers.load();
            if let Some(consumer) = store.get(name) {
                attach_consumer(&mut ctx, &consumer, "hmac-auth");
                return Ok(PluginOutput {
                    context: ctx,
                    named_outputs: HashMap::new(),
                });
            }
        }
        self.reject(ctx, "Invalid user authorization")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::consumers::{ConsumerConfig, ConsumerStore};
    use crate::context::{GatewayRequest, GatewayResponse, Protocol};

    /// A recent RFC 1123 date built from the current timestamp so clock-skew
    /// checks pass. We format it here to avoid pulling in a date crate.
    fn http_date(ts: i64) -> String {
        // Reverse of parse_http_date's civil algorithm for a small range.
        let days = ts.div_euclid(86400);
        let secs = ts.rem_euclid(86400);
        let (h, m, s) = (secs / 3600, (secs % 3600) / 60, secs % 60);
        // civil_from_days (Hinnant).
        let z = days + 719468;
        let era = if z >= 0 { z } else { z - 146096 } / 146097;
        let doe = z - era * 146097;
        let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
        let y = yoe + era * 400;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let d = doy - (153 * mp + 2) / 5 + 1;
        let month = if mp < 10 { mp + 3 } else { mp - 9 };
        let year = if month <= 2 { y + 1 } else { y };
        let month_name = [
            "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
        ][(month - 1) as usize];
        // Weekday is not validated by the parser, so any token works.
        format!(
            "Mon, {:02} {} {:04} {:02}:{:02}:{:02} GMT",
            d, month_name, year, h, m, s
        )
    }

    fn base_ctx() -> Context {
        Context {
            request: GatewayRequest {
                method: "GET".to_string(),
                path: "/api".to_string(),
                host: "h".to_string(),
                scheme: "http".to_string(),
                headers: HashMap::new(),
                query_params: HashMap::new(),
                body: Bytes::new(),
                remote_addr: "1.2.3.4:5".to_string(),
                protocol: Protocol::Http1,
            },
            response: GatewayResponse {
                status_code: 0,
                headers: HashMap::new(),
                body: Bytes::new(),
            },
            message: HashMap::new(),
            errors: Vec::new(),
        }
    }

    /// Computes the base64 signature a well-behaved client would send for the
    /// given signed headers.
    fn sign(
        secret: &str,
        alg: HmacAlgorithm,
        access_key: &str,
        signed: &[(&str, &str)],
        method: &str,
        uri: &str,
    ) -> String {
        let mut items = vec![access_key.to_string()];
        for (h, v) in signed {
            if *h == "@request-target" {
                items.push(format!("{} {}", method, uri));
            } else {
                items.push(format!("{}: {}", h, v));
            }
        }
        let mut s = items.join("\n");
        s.push('\n');
        let key = hmac::Key::new(alg.ring_algorithm(), secret.as_bytes());
        BASE64.encode(hmac::sign(&key, s.as_bytes()).as_ref())
    }

    /// Builds a request signed with the X-HMAC-* headers over `date` only.
    fn signed_request(secret: &str, access_key: &str, alg: HmacAlgorithm, date: &str) -> Context {
        let mut ctx = base_ctx();
        let signature = sign(secret, alg, access_key, &[("date", date)], "GET", "/api");
        ctx.request
            .headers
            .insert("date".to_string(), vec![date.to_string()]);
        ctx.request
            .headers
            .insert(HDR_ACCESS_KEY.to_string(), vec![access_key.to_string()]);
        ctx.request
            .headers
            .insert(HDR_ALGORITHM.to_string(), vec![alg.name().to_string()]);
        ctx.request
            .headers
            .insert(HDR_SIGNED_HEADERS.to_string(), vec!["date".to_string()]);
        ctx.request
            .headers
            .insert(HDR_SIGNATURE.to_string(), vec![signature]);
        ctx
    }

    fn inline_plugin(extra: serde_json::Value) -> HmacAuthPlugin {
        let mut config: HashMap<String, serde_json::Value> =
            serde_json::from_value(serde_json::json!({
                "access_key": "ak1",
                "secret_key": "sk1",
            }))
            .unwrap();
        if let serde_json::Value::Object(m) = extra {
            for (k, v) in m {
                config.insert(k, v);
            }
        }
        HmacAuthPlugin::from_config(&config, &PluginResources::empty()).unwrap()
    }

    #[test]
    fn test_config_requires_credential_or_consumers() {
        assert!(HmacAuthPlugin::from_config(&HashMap::new(), &PluginResources::empty()).is_err());
        // access_key without secret_key is rejected
        let cfg: HashMap<String, serde_json::Value> =
            serde_json::from_value(serde_json::json!({ "access_key": "x" })).unwrap();
        assert!(HmacAuthPlugin::from_config(&cfg, &PluginResources::empty()).is_err());
    }

    #[test]
    fn test_rejects_unknown_algorithm() {
        let cfg: HashMap<String, serde_json::Value> = serde_json::from_value(
            serde_json::json!({ "access_key": "a", "secret_key": "b", "algorithm": "md5" }),
        )
        .unwrap();
        assert!(HmacAuthPlugin::from_config(&cfg, &PluginResources::empty()).is_err());
    }

    #[test]
    fn test_http_date_round_trip() {
        // A known timestamp: 2001-09-09 01:46:40 UTC = 1_000_000_000.
        assert_eq!(
            parse_http_date("Sun, 09 Sep 2001 01:46:40 GMT"),
            Some(1_000_000_000)
        );
        // The formatter and parser agree.
        let ts = 1_600_000_000;
        assert_eq!(parse_http_date(&http_date(ts)), Some(ts));
    }

    #[tokio::test]
    async fn test_inline_valid_signature_passes() {
        let plugin = inline_plugin(serde_json::json!({}));
        let date = http_date(now() as i64);
        let ctx = signed_request("sk1", "ak1", HmacAlgorithm::Sha256, &date);
        let out = plugin.execute(ctx, &HashMap::new()).await.unwrap();
        // keep_headers defaults false → proof headers stripped
        assert!(!out.context.request.headers.contains_key(HDR_SIGNATURE));
    }

    #[tokio::test]
    async fn test_wrong_secret_rejected() {
        let plugin = inline_plugin(serde_json::json!({}));
        let date = http_date(now() as i64);
        // client signs with the wrong secret
        let ctx = signed_request("wrong", "ak1", HmacAlgorithm::Sha256, &date);
        let err = plugin.execute(ctx, &HashMap::new()).await.unwrap_err();
        assert_eq!(err.error.code, "HMAC_INVALID");
        assert_eq!(err.context.response.status_code, 401);
    }

    #[tokio::test]
    async fn test_clock_skew_exceeded_rejected() {
        let plugin = inline_plugin(serde_json::json!({ "clock_skew": 10 }));
        let stale = http_date(now() as i64 - 3600);
        let ctx = signed_request("sk1", "ak1", HmacAlgorithm::Sha256, &stale);
        assert!(plugin.execute(ctx, &HashMap::new()).await.is_err());
    }

    #[tokio::test]
    async fn test_missing_required_signed_header_rejected() {
        // require @request-target but the client only signs date
        let plugin = inline_plugin(serde_json::json!({ "signed_headers": ["@request-target"] }));
        let date = http_date(now() as i64);
        let ctx = signed_request("sk1", "ak1", HmacAlgorithm::Sha256, &date);
        assert!(plugin.execute(ctx, &HashMap::new()).await.is_err());
    }

    #[tokio::test]
    async fn test_authorization_signature_form() {
        let plugin = inline_plugin(serde_json::json!({ "clock_skew": 0 }));
        let mut ctx = base_ctx();
        let signature = sign(
            "sk1",
            HmacAlgorithm::Sha256,
            "ak1",
            &[("@request-target", "")],
            "GET",
            "/api",
        );
        let auth = format!(
            "Signature keyId=\"ak1\",algorithm=\"hmac-sha256\",headers=\"@request-target\",signature=\"{}\"",
            signature
        );
        ctx.request
            .headers
            .insert("authorization".to_string(), vec![auth]);
        assert!(plugin.execute(ctx, &HashMap::new()).await.is_ok());
    }

    fn resources_with_consumers() -> Arc<PluginResources> {
        let resources = PluginResources::empty();
        let consumers: Vec<ConsumerConfig> = serde_json::from_value(serde_json::json!([
            {
                "name": "alice",
                "credentials": { "hmac-auth": { "access_key": "alice-ak", "secret_key": "alice-sk" } }
            },
            { "name": "guest" }
        ]))
        .unwrap();
        resources
            .consumers
            .store(Arc::new(ConsumerStore::from_config(&consumers).unwrap()));
        resources
    }

    #[tokio::test]
    async fn test_consumer_mode_attaches_identity() {
        let resources = resources_with_consumers();
        let mut config = HashMap::new();
        config.insert("use_consumers".to_string(), serde_json::json!(true));
        config.insert("clock_skew".to_string(), serde_json::json!(0));
        let plugin = HmacAuthPlugin::from_config(&config, &resources).unwrap();

        let date = http_date(now() as i64);
        let ctx = signed_request("alice-sk", "alice-ak", HmacAlgorithm::Sha256, &date);
        let out = plugin.execute(ctx, &HashMap::new()).await.unwrap();
        assert_eq!(
            out.context.message.get("consumer.name"),
            Some(&serde_json::json!("alice"))
        );
        assert_eq!(
            out.context.request.headers.get("x-consumer-username"),
            Some(&vec!["alice".to_string()])
        );

        // unknown access key rejected
        let ctx = signed_request("x", "nobody", HmacAlgorithm::Sha256, &date);
        assert!(plugin.execute(ctx, &HashMap::new()).await.is_err());
    }

    #[tokio::test]
    async fn test_anonymous_consumer_fallback() {
        let resources = resources_with_consumers();
        let mut config = HashMap::new();
        config.insert("use_consumers".to_string(), serde_json::json!(true));
        config.insert("anonymous_consumer".to_string(), serde_json::json!("guest"));
        let plugin = HmacAuthPlugin::from_config(&config, &resources).unwrap();

        // no signature at all → anonymous
        let out = plugin.execute(base_ctx(), &HashMap::new()).await.unwrap();
        assert_eq!(
            out.context.message.get("consumer.name"),
            Some(&serde_json::json!("guest"))
        );
    }
}
