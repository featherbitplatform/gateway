//! JWE decryption plugin (`jwe-decrypt`).
//!
//! Reads a JWE-encrypted token from a request header, decrypts it, and forwards
//! the plaintext into another request header before the request is proxied
//! upstream. This is a **faithful subset** of APISIX's `jwe-decrypt` plugin: it
//! implements the `dir` (direct) key-management algorithm with `A256GCM`
//! content encryption only — the exact scheme APISIX supports via its
//! `resty.aes` 256-bit GCM cipher. Other JWE algorithms (RSA-OAEP, ECDH-ES,
//! key-wrap variants, other content ciphers) are **not** supported; see the
//! Deviations section of the docs page.
//!
//! The symmetric key is either configured inline (`key`, base64) or resolved
//! per request from the consumer store (`use_consumers`) using the `kid`
//! carried in the JWE protected header. Malformed tokens and decryption
//! failures are rejected with a 401 (`JWE_INVALID`) routed through the error
//! port.

use async_trait::async_trait;
use base64::Engine;
use bytes::Bytes;
use ring::aead::{Aad, LessSafeKey, Nonce, UnboundKey, AES_256_GCM};
use std::collections::HashMap;
use std::sync::Arc;

use crate::context::{Context, GatewayError};
use crate::plugins::resources::PluginResources;
use crate::plugins::{Plugin, PluginExecutionError, PluginOutput, PluginResult};

/// The consumer-store auth type under which jwe-decrypt credentials are indexed.
const AUTH_TYPE: &str = "jwe-decrypt";

/// Decodes a base64url (RFC 4648 §5, no padding) string, tolerating any
/// trailing `=` padding some encoders emit.
fn b64url_decode(s: &str) -> Option<Vec<u8>> {
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(s.trim_end_matches('='))
        .ok()
}

/// Encodes bytes as base64url without padding. Test-only: used to build JWE
/// fixtures.
#[cfg(test)]
fn b64url_encode(b: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b)
}

/// Decodes a configured/consumer key that may be either standard or url-safe
/// base64 (with or without padding).
fn decode_config_key(s: &str) -> Option<Vec<u8>> {
    base64::engine::general_purpose::STANDARD
        .decode(s)
        .ok()
        .or_else(|| {
            base64::engine::general_purpose::STANDARD_NO_PAD
                .decode(s)
                .ok()
        })
        .or_else(|| b64url_decode(s))
}

/// AES-256-GCM decryption for the `dir`+`A256GCM` JWE scheme. `aad` is the
/// ASCII bytes of the base64url-encoded protected header (RFC 7516 §5.1).
fn aes256gcm_decrypt(
    key: &[u8],
    iv: &[u8],
    ciphertext: &[u8],
    tag: &[u8],
    aad: &[u8],
) -> Option<Vec<u8>> {
    if key.len() != 32 || iv.len() != 12 {
        return None;
    }
    let unbound = UnboundKey::new(&AES_256_GCM, key).ok()?;
    let sealing = LessSafeKey::new(unbound);
    let nonce = Nonce::try_assume_unique_for_key(iv).ok()?;
    // ring expects ciphertext followed by the authentication tag in one buffer.
    let mut in_out = Vec::with_capacity(ciphertext.len() + tag.len());
    in_out.extend_from_slice(ciphertext);
    in_out.extend_from_slice(tag);
    let plaintext = sealing
        .open_in_place(nonce, Aad::from(aad), &mut in_out)
        .ok()?;
    Some(plaintext.to_vec())
}

/// Decrypts a JWE `dir`+`A256GCM` token and forwards the plaintext downstream.
///
/// On success the decrypted plaintext replaces the value of `forward_header`
/// in the outbound request; the request otherwise continues unchanged through
/// the **success** port. On any failure (missing token when `strict`,
/// malformed compact serialization, unsupported algorithm, unknown `kid`, or a
/// failed AEAD check) the request is rejected with a 401 JSON response and a
/// `JWE_INVALID` error routed through the **error** port.
pub struct JweDecryptPlugin {
    /// Lowercased header the encrypted token is read from.
    header: String,
    /// Lowercased header the decrypted plaintext is written to.
    forward_header: String,
    /// When true, a missing token is rejected; when false the request passes
    /// through untouched.
    strict: bool,
    /// Inline symmetric key (32 raw bytes) when configured; takes precedence
    /// over the consumer store.
    inline_key: Option<Vec<u8>>,
    /// When true, resolve the key from the consumer store using the token `kid`.
    #[allow(dead_code)] // parsed config; consumer-store lookup not yet wired
    use_consumers: bool,
    resources: Arc<PluginResources>,
}

impl JweDecryptPlugin {
    /// Builds the plugin from node config.
    ///
    /// Accepted keys:
    /// - `header` (string, default `"Authorization"`): header carrying the JWE
    ///   token; matched case-insensitively (an optional `Bearer ` prefix is
    ///   stripped).
    /// - `forward_header` (string, default `"Authorization"`): header the
    ///   decrypted plaintext is written to before proxying.
    /// - `strict` (bool, default `true`): when true a missing token is
    ///   rejected; when false the request passes through unchanged.
    /// - `key` (string, optional): inline symmetric key, base64-encoded, which
    ///   must decode to exactly 32 bytes (AES-256). Used for every request.
    /// - `use_consumers` (bool, default `false`): resolve the key per request
    ///   from the gateway `consumers:` section — the token's `kid` selects the
    ///   consumer's `jwe-decrypt` credential (`{key: <kid>, secret: <32-byte
    ///   key>, is_base64_encoded: <bool>}`).
    /// - At least one of `key` / `use_consumers` must be provided.
    /// - `alg` (string, default `"dir"`) / `enc` (string, default `"A256GCM"`):
    ///   only these values are supported; any other value is a **config error**
    ///   (fail-fast at load), since full JWE is not implemented.
    ///
    /// ```yaml
    /// type: jwe-decrypt
    /// config:
    ///   header: Authorization
    ///   forward_header: Authorization
    ///   strict: true
    ///   use_consumers: true
    /// ```
    pub fn from_config(
        config: &HashMap<String, serde_json::Value>,
        resources: &Arc<PluginResources>,
    ) -> Result<Self, String> {
        // Reject unsupported JWE schemes at load time (faithful-subset guard).
        if let Some(alg) = config.get("alg").and_then(|v| v.as_str()) {
            if alg != "dir" {
                return Err(format!(
                    "jwe-decrypt only supports alg 'dir' (got '{}'); RSA/ECDH/key-wrap are not implemented",
                    alg
                ));
            }
        }
        if let Some(enc) = config.get("enc").and_then(|v| v.as_str()) {
            if enc != "A256GCM" {
                return Err(format!(
                    "jwe-decrypt only supports enc 'A256GCM' (got '{}')",
                    enc
                ));
            }
        }

        let header = config
            .get("header")
            .and_then(|v| v.as_str())
            .unwrap_or("Authorization")
            .to_lowercase();

        let forward_header = config
            .get("forward_header")
            .and_then(|v| v.as_str())
            .unwrap_or("Authorization")
            .to_lowercase();

        let strict = config
            .get("strict")
            .and_then(|v| v.as_bool())
            .unwrap_or(true);

        let inline_key = match config.get("key").and_then(|v| v.as_str()) {
            Some(k) => {
                let bytes = decode_config_key(k)
                    .ok_or_else(|| "jwe-decrypt 'key' must be valid base64".to_string())?;
                if bytes.len() != 32 {
                    return Err(format!(
                        "jwe-decrypt 'key' must decode to 32 bytes for AES-256 (got {})",
                        bytes.len()
                    ));
                }
                Some(bytes)
            }
            None => None,
        };

        let use_consumers = config
            .get("use_consumers")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        if inline_key.is_none() && !use_consumers {
            return Err("jwe-decrypt plugin requires 'key' or 'use_consumers: true'".to_string());
        }

        Ok(Self {
            header,
            forward_header,
            strict,
            inline_key,
            use_consumers,
            resources: resources.clone(),
        })
    }

    /// Builds the 401 rejection (code `JWE_INVALID`) carrying the context so the
    /// graph engine routes through the error port.
    fn reject(ctx: Context, message: &str) -> PluginResult {
        let mut ctx = ctx;
        ctx.response.status_code = 401;
        ctx.response.body = Bytes::from(format!(
            r#"{{"error": "unauthorized", "message": "{}"}}"#,
            message
        ));
        ctx.response.headers.insert(
            "content-type".to_string(),
            vec!["application/json".to_string()],
        );
        Err(PluginExecutionError {
            context: ctx,
            error: GatewayError {
                node_id: String::new(),
                code: "JWE_INVALID".to_string(),
                message: message.to_string(),
                metadata: HashMap::new(),
            },
        })
    }

    /// Resolves the 32-byte AES key for the given token `kid` from the consumer
    /// store. Returns the raw key bytes, or `None` with a reason.
    fn consumer_key(&self, kid: Option<&str>) -> Result<Vec<u8>, &'static str> {
        let kid = kid.ok_or("missing kid in JWE token")?;
        let store = self.resources.consumers.load();
        let consumer = store
            .find_by_credential(AUTH_TYPE, kid)
            .ok_or("invalid kid in JWE token")?;
        let cred = consumer
            .credentials
            .get(AUTH_TYPE)
            .ok_or("consumer has no jwe-decrypt credential")?;
        let secret = cred
            .get("secret")
            .and_then(|v| v.as_str())
            .ok_or("consumer jwe-decrypt credential missing 'secret'")?;
        let is_b64 = cred
            .get("is_base64_encoded")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let bytes = if is_b64 {
            b64url_decode(secret).ok_or("consumer secret is not valid base64url")?
        } else {
            secret.as_bytes().to_vec()
        };
        if bytes.len() != 32 {
            return Err("consumer secret must be 32 bytes for AES-256");
        }
        Ok(bytes)
    }
}

#[async_trait]
impl Plugin for JweDecryptPlugin {
    fn plugin_type(&self) -> &str {
        "jwe-decrypt"
    }

    async fn execute(
        &self,
        mut ctx: Context,
        _named_inputs: &HashMap<String, serde_json::Value>,
    ) -> PluginResult {
        // Fetch the token, stripping a Bearer prefix (case-insensitive).
        let raw = ctx
            .request
            .headers
            .get(&self.header)
            .and_then(|v| v.first())
            .cloned();
        let token = match raw {
            Some(t) => {
                let lower = t.to_ascii_lowercase();
                if lower.starts_with("bearer ") {
                    t[7..].to_string()
                } else {
                    t
                }
            }
            None => {
                if self.strict {
                    return Self::reject(ctx, "missing JWE token in request");
                }
                return Ok(PluginOutput {
                    context: ctx,
                    named_outputs: HashMap::new(),
                });
            }
        };

        // Parse the 5-part compact serialization:
        // protected . encrypted_key . iv . ciphertext . tag
        let parts: Vec<&str> = token.split('.').collect();
        if parts.len() != 5 {
            return Self::reject(ctx, "malformed JWE token");
        }
        let (protected_b64, enc_key, iv_b64, ct_b64, tag_b64) =
            (parts[0], parts[1], parts[2], parts[3], parts[4]);

        // dir: the encrypted_key segment must be empty.
        if !enc_key.is_empty() {
            return Self::reject(
                ctx,
                "unsupported JWE: encrypted key present (only dir is supported)",
            );
        }

        let header_bytes = match b64url_decode(protected_b64) {
            Some(b) => b,
            None => return Self::reject(ctx, "malformed JWE protected header"),
        };
        let header_obj: serde_json::Value = match serde_json::from_slice(&header_bytes) {
            Ok(v) => v,
            Err(_) => return Self::reject(ctx, "malformed JWE protected header"),
        };

        // Enforce the supported scheme.
        if header_obj.get("alg").and_then(|v| v.as_str()) != Some("dir")
            || header_obj.get("enc").and_then(|v| v.as_str()) != Some("A256GCM")
        {
            return Self::reject(
                ctx,
                "unsupported JWE alg/enc (only dir + A256GCM supported)",
            );
        }

        let iv = match b64url_decode(iv_b64) {
            Some(v) => v,
            None => return Self::reject(ctx, "malformed JWE iv"),
        };
        let ciphertext = match b64url_decode(ct_b64) {
            Some(v) => v,
            None => return Self::reject(ctx, "malformed JWE ciphertext"),
        };
        let tag = match b64url_decode(tag_b64) {
            Some(v) => v,
            None => return Self::reject(ctx, "malformed JWE tag"),
        };

        // Resolve the symmetric key.
        let key = if let Some(ref k) = self.inline_key {
            k.clone()
        } else {
            match self.consumer_key(header_obj.get("kid").and_then(|v| v.as_str())) {
                Ok(k) => k,
                Err(reason) => return Self::reject(ctx, reason),
            }
        };

        // AAD is the ASCII of the encoded protected header (RFC 7516 §5.1).
        match aes256gcm_decrypt(&key, &iv, &ciphertext, &tag, protected_b64.as_bytes()) {
            Some(plaintext) => {
                let value = String::from_utf8_lossy(&plaintext).into_owned();
                ctx.request
                    .headers
                    .insert(self.forward_header.clone(), vec![value]);
                Ok(PluginOutput {
                    context: ctx,
                    named_outputs: HashMap::new(),
                })
            }
            None => Self::reject(ctx, "failed to decrypt JWE token"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::consumers::{ConsumerConfig, ConsumerStore};
    use crate::context::{GatewayRequest, Protocol};

    fn ctx_with_header(name: &str, value: Option<&str>) -> Context {
        let mut headers = HashMap::new();
        if let Some(v) = value {
            headers.insert(name.to_string(), vec![v.to_string()]);
        }
        Context::new(GatewayRequest {
            method: "GET".into(),
            path: "/".into(),
            host: "h".into(),
            scheme: "http".into(),
            headers,
            query_params: HashMap::new(),
            body: Bytes::new(),
            remote_addr: "1.2.3.4:5".into(),
            protocol: Protocol::Http1,
        })
    }

    /// Encrypts `plaintext` into a compact `dir`+`A256GCM` JWE with an optional
    /// `kid`, using the same primitives the plugin decrypts with.
    fn make_jwe(key: &[u8; 32], iv: &[u8; 12], plaintext: &[u8], kid: Option<&str>) -> String {
        let header_json = match kid {
            Some(k) => format!(r#"{{"alg":"dir","enc":"A256GCM","kid":"{}"}}"#, k),
            None => r#"{"alg":"dir","enc":"A256GCM"}"#.to_string(),
        };
        let protected_b64 = b64url_encode(header_json.as_bytes());

        let unbound = UnboundKey::new(&AES_256_GCM, key).unwrap();
        let sealing = LessSafeKey::new(unbound);
        let nonce = Nonce::try_assume_unique_for_key(iv).unwrap();
        let mut in_out = plaintext.to_vec();
        sealing
            .seal_in_place_append_tag(nonce, Aad::from(protected_b64.as_bytes()), &mut in_out)
            .unwrap();
        let (ct, tag) = in_out.split_at(plaintext.len());

        format!(
            "{}..{}.{}.{}",
            protected_b64,
            b64url_encode(iv),
            b64url_encode(ct),
            b64url_encode(tag)
        )
    }

    #[test]
    fn test_requires_key_or_consumers() {
        assert!(JweDecryptPlugin::from_config(&HashMap::new(), &PluginResources::empty()).is_err());
    }

    #[test]
    fn test_rejects_unsupported_alg_at_load() {
        let mut cfg = HashMap::new();
        cfg.insert(
            "key".to_string(),
            serde_json::json!(base64::engine::general_purpose::STANDARD.encode([0u8; 32])),
        );
        cfg.insert("alg".to_string(), serde_json::json!("RSA-OAEP"));
        assert!(JweDecryptPlugin::from_config(&cfg, &PluginResources::empty()).is_err());
    }

    #[tokio::test]
    async fn test_roundtrip_inline_key() {
        let key = [7u8; 32];
        let iv = [3u8; 12];
        let token = make_jwe(&key, &iv, b"hello-plaintext", None);

        let mut cfg = HashMap::new();
        cfg.insert(
            "key".to_string(),
            serde_json::json!(base64::engine::general_purpose::STANDARD.encode(key)),
        );
        cfg.insert(
            "forward_header".to_string(),
            serde_json::json!("x-decrypted"),
        );
        let plugin = JweDecryptPlugin::from_config(&cfg, &PluginResources::empty()).unwrap();

        let ctx = ctx_with_header("authorization", Some(&format!("Bearer {}", token)));
        let out = plugin.execute(ctx, &HashMap::new()).await.unwrap();
        assert_eq!(
            out.context.request.headers.get("x-decrypted"),
            Some(&vec!["hello-plaintext".to_string()])
        );
    }

    #[tokio::test]
    async fn test_roundtrip_consumer_key() {
        let iv = [1u8; 12];
        // Use a raw 32-char secret so the is_base64_encoded=false path is exercised.
        // Fixture: exactly 32 bytes, not a credential.
        let raw_secret = "0123456789abcdef0123456789abcdef"; // nosemgrep: generic.secrets.security.detected-generic-secret.detected-generic-secret
        let key32: [u8; 32] = raw_secret.as_bytes().try_into().unwrap();
        let token = make_jwe(&key32, &iv, b"consumer-body", Some("kid-1"));

        let resources = PluginResources::empty();
        let consumers: Vec<ConsumerConfig> = serde_json::from_value(serde_json::json!([
            {
                "name": "c1",
                "credentials": { "jwe-decrypt": { "key": "kid-1", "secret": raw_secret } }
            }
        ]))
        .unwrap();
        resources
            .consumers
            .store(Arc::new(ConsumerStore::from_config(&consumers).unwrap()));

        let mut cfg = HashMap::new();
        cfg.insert("use_consumers".to_string(), serde_json::json!(true));
        cfg.insert(
            "forward_header".to_string(),
            serde_json::json!("x-decrypted"),
        );
        let plugin = JweDecryptPlugin::from_config(&cfg, &resources).unwrap();

        let ctx = ctx_with_header("authorization", Some(&token));
        let out = plugin.execute(ctx, &HashMap::new()).await.unwrap();
        assert_eq!(
            out.context.request.headers.get("x-decrypted"),
            Some(&vec!["consumer-body".to_string()])
        );
    }

    #[tokio::test]
    async fn test_malformed_token_rejected() {
        let key = [7u8; 32];
        let mut cfg = HashMap::new();
        cfg.insert(
            "key".to_string(),
            serde_json::json!(base64::engine::general_purpose::STANDARD.encode(key)),
        );
        let plugin = JweDecryptPlugin::from_config(&cfg, &PluginResources::empty()).unwrap();

        // Only 4 segments -> malformed.
        let ctx = ctx_with_header("authorization", Some("not.a.valid.jwe"));
        let err = plugin.execute(ctx, &HashMap::new()).await.unwrap_err();
        assert_eq!(err.error.code, "JWE_INVALID");
        assert_eq!(err.context.response.status_code, 401);
    }

    #[tokio::test]
    async fn test_tampered_ciphertext_fails_aead() {
        let key = [5u8; 32];
        let iv = [2u8; 12];
        let token = make_jwe(&key, &iv, b"secret", None);
        // Corrupt the ciphertext segment.
        let mut parts: Vec<&str> = token.split('.').collect();
        let bad_ct = "AAAAAAAA";
        parts[3] = bad_ct;
        let tampered = parts.join(".");

        let mut cfg = HashMap::new();
        cfg.insert(
            "key".to_string(),
            serde_json::json!(base64::engine::general_purpose::STANDARD.encode(key)),
        );
        let plugin = JweDecryptPlugin::from_config(&cfg, &PluginResources::empty()).unwrap();
        let ctx = ctx_with_header("authorization", Some(&tampered));
        assert!(plugin.execute(ctx, &HashMap::new()).await.is_err());
    }

    #[tokio::test]
    async fn test_missing_token_non_strict_passthrough() {
        let key = [7u8; 32];
        let mut cfg = HashMap::new();
        cfg.insert(
            "key".to_string(),
            serde_json::json!(base64::engine::general_purpose::STANDARD.encode(key)),
        );
        cfg.insert("strict".to_string(), serde_json::json!(false));
        let plugin = JweDecryptPlugin::from_config(&cfg, &PluginResources::empty()).unwrap();
        let ctx = ctx_with_header("authorization", None);
        assert!(plugin.execute(ctx, &HashMap::new()).await.is_ok());
    }
}
