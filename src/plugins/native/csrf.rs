//! CSRF protection plugin (`csrf`).
//!
//! Port of APISIX's `csrf` plugin using the double-submit-cookie pattern:
//! safe methods (`GET`/`HEAD`/`OPTIONS`) pass through and receive a signed
//! token cookie; unsafe methods must send the same token in both the cookie
//! and a request header, with a valid HMAC signature and unexpired timestamp.
//! Failures are routed through the error port as 401 with code `CSRF_INVALID`.
//!
//! Token layout mirrors APISIX (`base64(json{random, expires, sign})`) but the
//! signature is `hex(HMAC-SHA256(key, random || expires))` via `ring` instead
//! of APISIX's plain SHA-256 over a Lua-formatted string, so featherbit tokens
//! only round-trip against featherbit — they are not APISIX-compatible.
//!
//! APISIX sets the cookie in its `header_filter` phase (after the upstream
//! response). featherbit's `upstream` node replaces `context.response.headers`
//! wholesale, so a cookie set before proxying would be lost. The `phase`
//! option maps APISIX's two phases onto the node graph: place a
//! `phase: request` node before the upstream (validation) and a
//! `phase: response` node after it (cookie issuance) sharing the same `key`.

use async_trait::async_trait;
use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use bytes::Bytes;
use ring::hmac;
use ring::rand::{SecureRandom, SystemRandom};
use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::context::{Context, GatewayError};
use crate::plugins::{Plugin, PluginExecutionError, PluginOutput, PluginResult};

const SAFE_METHODS: [&str; 3] = ["GET", "HEAD", "OPTIONS"];

/// Double-submit-cookie CSRF protection keyed by an HMAC secret.
///
/// In `phase: request` (default), unsafe methods must present matching
/// header and cookie tokens carrying a valid signature; safe methods pass
/// through (and get a token cookie, though a downstream `upstream` node will
/// replace it — see the module docs). In `phase: response` the node only
/// issues the token cookie, mirroring APISIX's `header_filter`.
pub struct CsrfPlugin {
    /// HMAC-SHA256 key derived from the configured secret.
    key: hmac::Key,
    /// Token lifetime in seconds; `0` disables the expiry check.
    expires: u64,
    /// Cookie and header name carrying the token (lowercased for lookup).
    name: String,
    /// Which side of the exchange this node handles.
    phase: CsrfPhase,
}

/// Which APISIX phase this node instance emulates.
#[derive(Debug, Clone, PartialEq)]
enum CsrfPhase {
    /// Validate unsafe methods (APISIX `access`).
    Request,
    /// Issue the token cookie on the response (APISIX `header_filter`).
    Response,
}

/// Hex-encodes a byte slice (lowercase).
fn hex_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push_str(&format!("{:02x}", b));
    }
    out
}

/// Decodes a lowercase/uppercase hex string; `None` on invalid input.
fn hex_decode(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok())
        .collect()
}

/// Current unix timestamp in seconds.
fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

impl CsrfPlugin {
    /// Builds the plugin from node config.
    ///
    /// Accepted keys:
    /// - `key` (string, **required**, non-empty): HMAC secret used to sign
    ///   tokens.
    /// - `expires` (integer seconds, default `7200`): token lifetime; `0`
    ///   disables the expiry check and makes the cookie a session cookie.
    /// - `name` (string, default `"featherbit-csrf-token"`): cookie **and**
    ///   request-header name carrying the token.
    /// - `phase` (string, default `request`): `request` validates unsafe
    ///   methods; `response` only issues the token cookie (place it after the
    ///   upstream node).
    ///
    /// ```yaml
    /// type: csrf
    /// config:
    ///   key: edd1c9f034335f136f87ad84b625c8f1
    ///   expires: 3600
    ///   name: featherbit-csrf-token
    /// ```
    pub fn from_config(config: &HashMap<String, serde_json::Value>) -> Result<Self, String> {
        let secret = config
            .get("key")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| "csrf: key is required and must be a non-empty string".to_string())?;

        let expires = match config.get("expires") {
            None => 7200,
            Some(v) => v
                .as_u64()
                .ok_or_else(|| "csrf: expires must be a non-negative integer".to_string())?,
        };

        let name = config
            .get("name")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .unwrap_or("featherbit-csrf-token")
            .to_lowercase();

        let phase = match config.get("phase").and_then(|v| v.as_str()) {
            Some("response") => CsrfPhase::Response,
            _ => CsrfPhase::Request,
        };

        Ok(Self {
            key: hmac::Key::new(hmac::HMAC_SHA256, secret.as_bytes()),
            expires,
            name,
            phase,
        })
    }

    /// Computes the token signature: `hex(HMAC-SHA256(key, random || expires))`.
    fn sign(&self, random: &str, expires: u64) -> String {
        let tag = hmac::sign(&self.key, format!("{}{}", random, expires).as_bytes());
        hex_encode(tag.as_ref())
    }

    /// Builds a token issued at timestamp `ts`:
    /// `base64(json{random, expires: ts, sign})`.
    fn token_at(&self, ts: u64) -> String {
        let mut random_bytes = [0u8; 16];
        // On the vanishingly unlikely RNG failure the buffer stays zeroed;
        // the token remains valid, just not random.
        let _ = SystemRandom::new().fill(&mut random_bytes);
        let random = hex_encode(&random_bytes);
        let token = serde_json::json!({
            "random": random,
            "expires": ts,
            "sign": self.sign(&random, ts),
        });
        BASE64.encode(token.to_string())
    }

    /// Generates a fresh token for the current time.
    fn gen_token(&self) -> String {
        self.token_at(now())
    }

    /// Verifies a token's structure, expiry, and signature.
    fn check_token(&self, token: &str) -> bool {
        let Ok(decoded) = BASE64.decode(token) else {
            return false;
        };
        let Ok(parsed) = serde_json::from_slice::<serde_json::Value>(&decoded) else {
            return false;
        };
        let Some(random) = parsed.get("random").and_then(|v| v.as_str()) else {
            return false;
        };
        let Some(expires) = parsed.get("expires").and_then(|v| v.as_u64()) else {
            return false;
        };
        if self.expires > 0 && now().saturating_sub(expires) > self.expires {
            return false;
        }
        let Some(sign) = parsed.get("sign").and_then(|v| v.as_str()) else {
            return false;
        };
        let Some(sign_bytes) = hex_decode(sign) else {
            return false;
        };
        hmac::verify(
            &self.key,
            format!("{}{}", random, expires).as_bytes(),
            &sign_bytes,
        )
        .is_ok()
    }

    /// Appends the token `Set-Cookie` header to the response.
    ///
    /// Deviation: uses `Max-Age` instead of APISIX's `Expires` date (omitted
    /// when `expires` is `0`, yielding a session cookie).
    fn set_cookie(&self, ctx: &mut Context) {
        let max_age = if self.expires > 0 {
            format!(";Max-Age={}", self.expires)
        } else {
            String::new()
        };
        let cookie = format!(
            "{}={};path=/;SameSite=Lax{}",
            self.name,
            self.gen_token(),
            max_age
        );
        ctx.response
            .headers
            .entry("set-cookie".to_string())
            .or_default()
            .push(cookie);
    }

    /// Builds the 401 rejection routed through the error port with code
    /// `CSRF_INVALID`.
    fn reject(&self, mut ctx: Context, msg: &str) -> PluginResult {
        ctx.response.status_code = 401;
        ctx.response.body = Bytes::from(serde_json::json!({ "error_msg": msg }).to_string());
        ctx.response.headers.insert(
            "content-type".to_string(),
            vec!["application/json".to_string()],
        );
        Err(PluginExecutionError {
            context: ctx,
            error: GatewayError {
                node_id: String::new(),
                code: "CSRF_INVALID".to_string(),
                message: msg.to_string(),
                metadata: HashMap::new(),
            },
        })
    }
}

#[async_trait]
impl Plugin for CsrfPlugin {
    fn plugin_type(&self) -> &str {
        "csrf"
    }

    async fn execute(
        &self,
        mut ctx: Context,
        _named_inputs: &HashMap<String, serde_json::Value>,
    ) -> PluginResult {
        if self.phase == CsrfPhase::Response {
            // Cookie issuance only (APISIX header_filter).
            self.set_cookie(&mut ctx);
            return Ok(PluginOutput {
                context: ctx,
                named_outputs: HashMap::new(),
            });
        }

        if SAFE_METHODS.contains(&ctx.request.method.as_str()) {
            self.set_cookie(&mut ctx);
            return Ok(PluginOutput {
                context: ctx,
                named_outputs: HashMap::new(),
            });
        }

        let header_token = ctx
            .request
            .headers
            .get(&self.name)
            .and_then(|v| v.first())
            .cloned()
            .unwrap_or_default();
        if header_token.is_empty() {
            return self.reject(ctx, "no csrf token in headers");
        }

        let cookie_token =
            crate::vars::resolve(&ctx, &format!("cookie_{}", self.name)).map(|v| v.into_owned());
        let Some(cookie_token) = cookie_token else {
            return self.reject(ctx, "no csrf cookie");
        };

        if header_token != cookie_token {
            return self.reject(ctx, "csrf token mismatch");
        }

        if !self.check_token(&cookie_token) {
            return self.reject(ctx, "Failed to verify the csrf token signature");
        }

        self.set_cookie(&mut ctx);
        Ok(PluginOutput {
            context: ctx,
            named_outputs: HashMap::new(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::{GatewayRequest, GatewayResponse, Protocol};

    const NAME: &str = "featherbit-csrf-token";

    fn test_context(method: &str) -> Context {
        Context {
            request: GatewayRequest {
                method: method.to_string(),
                path: "/test".to_string(),
                host: "localhost".to_string(),
                scheme: "http".to_string(),
                headers: HashMap::new(),
                query_params: HashMap::new(),
                body: Bytes::new(),
                remote_addr: "127.0.0.1:12345".to_string(),
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

    fn with_tokens(method: &str, header_token: &str, cookie_token: &str) -> Context {
        let mut ctx = test_context(method);
        ctx.request
            .headers
            .insert(NAME.to_string(), vec![header_token.to_string()]);
        ctx.request.headers.insert(
            "cookie".to_string(),
            vec![format!("{}={}", NAME, cookie_token)],
        );
        ctx
    }

    fn plugin(json: serde_json::Value) -> CsrfPlugin {
        CsrfPlugin::from_config(&serde_json::from_value(json).unwrap()).unwrap()
    }

    #[test]
    fn test_config_requires_key() {
        let empty: HashMap<String, serde_json::Value> = HashMap::new();
        assert!(CsrfPlugin::from_config(&empty).is_err());
        assert!(CsrfPlugin::from_config(
            &serde_json::from_value(serde_json::json!({"key": ""})).unwrap()
        )
        .is_err());
        assert!(CsrfPlugin::from_config(
            &serde_json::from_value(serde_json::json!({"key": "secret", "expires": -1})).unwrap()
        )
        .is_err());
        assert!(CsrfPlugin::from_config(
            &serde_json::from_value(serde_json::json!({"key": "secret"})).unwrap()
        )
        .is_ok());
    }

    #[tokio::test]
    async fn test_safe_method_passes_and_sets_cookie() {
        let p = plugin(serde_json::json!({"key": "secret"}));
        let out = p
            .execute(test_context("GET"), &HashMap::new())
            .await
            .unwrap();
        let cookies = out.context.response.headers.get("set-cookie").unwrap();
        assert_eq!(cookies.len(), 1);
        assert!(cookies[0].starts_with(&format!("{}=", NAME)));
        assert!(cookies[0].contains("SameSite=Lax"));
        assert!(cookies[0].contains("Max-Age=7200"));
    }

    #[tokio::test]
    async fn test_response_phase_only_sets_cookie() {
        let p = plugin(serde_json::json!({"key": "secret", "phase": "response"}));
        // even for an unsafe method, response phase never validates
        let out = p
            .execute(test_context("POST"), &HashMap::new())
            .await
            .unwrap();
        assert!(out.context.response.headers.contains_key("set-cookie"));
    }

    #[tokio::test]
    async fn test_unsafe_method_without_tokens_rejected() {
        let p = plugin(serde_json::json!({"key": "secret"}));

        let err = p
            .execute(test_context("POST"), &HashMap::new())
            .await
            .unwrap_err();
        assert_eq!(err.error.code, "CSRF_INVALID");
        assert_eq!(err.context.response.status_code, 401);
        let body: serde_json::Value = serde_json::from_slice(&err.context.response.body).unwrap();
        assert_eq!(body["error_msg"], "no csrf token in headers");

        // header token present but no cookie
        let mut ctx = test_context("POST");
        ctx.request
            .headers
            .insert(NAME.to_string(), vec!["sometoken".to_string()]);
        let err = p.execute(ctx, &HashMap::new()).await.unwrap_err();
        let body: serde_json::Value = serde_json::from_slice(&err.context.response.body).unwrap();
        assert_eq!(body["error_msg"], "no csrf cookie");
    }

    #[tokio::test]
    async fn test_valid_token_round_trip() {
        let p = plugin(serde_json::json!({"key": "secret"}));
        let token = p.gen_token();
        let out = p
            .execute(with_tokens("POST", &token, &token), &HashMap::new())
            .await
            .unwrap();
        // a fresh cookie is issued after successful validation
        assert!(out.context.response.headers.contains_key("set-cookie"));
    }

    #[tokio::test]
    async fn test_token_mismatch_and_tampering_rejected() {
        let p = plugin(serde_json::json!({"key": "secret"}));
        let token = p.gen_token();
        let other = p.gen_token();

        // header != cookie
        let err = p
            .execute(with_tokens("POST", &token, &other), &HashMap::new())
            .await
            .unwrap_err();
        let body: serde_json::Value = serde_json::from_slice(&err.context.response.body).unwrap();
        assert_eq!(body["error_msg"], "csrf token mismatch");

        // token signed with a different key
        let wrong_key = plugin(serde_json::json!({"key": "other-secret"}));
        let forged = wrong_key.gen_token();
        let err = p
            .execute(with_tokens("POST", &forged, &forged), &HashMap::new())
            .await
            .unwrap_err();
        let body: serde_json::Value = serde_json::from_slice(&err.context.response.body).unwrap();
        assert_eq!(
            body["error_msg"],
            "Failed to verify the csrf token signature"
        );

        // garbage token
        assert!(p
            .execute(with_tokens("POST", "nonsense", "nonsense"), &HashMap::new())
            .await
            .is_err());
    }

    #[tokio::test]
    async fn test_expiry() {
        let p = plugin(serde_json::json!({"key": "secret", "expires": 10}));
        let stale = p.token_at(now() - 60);
        assert!(p
            .execute(with_tokens("POST", &stale, &stale), &HashMap::new())
            .await
            .is_err());

        // expires = 0 disables the expiry check (APISIX parity)
        let no_expiry = plugin(serde_json::json!({"key": "secret", "expires": 0}));
        let ancient = no_expiry.token_at(now() - 1_000_000);
        assert!(no_expiry
            .execute(with_tokens("POST", &ancient, &ancient), &HashMap::new())
            .await
            .is_ok());
        // session cookie: no Max-Age
        let out = no_expiry
            .execute(test_context("GET"), &HashMap::new())
            .await
            .unwrap();
        assert!(!out.context.response.headers.get("set-cookie").unwrap()[0].contains("Max-Age"));
    }

    #[test]
    fn test_hex_round_trip() {
        assert_eq!(hex_encode(&[0x00, 0xff, 0x2a]), "00ff2a");
        assert_eq!(hex_decode("00ff2a"), Some(vec![0x00, 0xff, 0x2a]));
        assert_eq!(hex_decode("0"), None);
        assert_eq!(hex_decode("zz"), None);
    }
}
