//! JWT authentication plugin (`jwt-auth`).
//!
//! Validates HMAC-signed JWTs (HS256/HS384/HS512) taken from a configurable
//! header, enforcing expiry, and exposes the verified claims to downstream
//! nodes via `context.message`. Invalid or missing tokens are rejected with a
//! 401 error routed through the node's error port.
//!
//! Two modes, usable together:
//! - **inline secret** (`secret` set): every token is verified with one
//!   shared secret and algorithm, exactly as before.
//! - **consumer mode** (`use_consumers: true`): the token's `key` claim
//!   identifies a consumer; the token is then verified with *that consumer's*
//!   `jwt-auth: {key, secret, algorithm}` credential and, on success, the
//!   consumer's identity is attached to the request.

use async_trait::async_trait;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use bytes::Bytes;
use jsonwebtoken::{decode, Algorithm, DecodingKey, Validation};
use std::collections::HashMap;
use std::sync::Arc;

use crate::consumers::attach_consumer;
use crate::context::{Context, GatewayError};
use crate::plugins::resources::PluginResources;
use crate::plugins::{Plugin, PluginExecutionError, PluginOutput, PluginResult};

/// Parses a supported HMAC algorithm name, defaulting to HS256 when absent.
///
/// Returns an error for any unsupported value — an auth plugin must not
/// silently verify with a different algorithm than the one requested.
fn parse_algorithm(name: Option<&str>) -> Result<Algorithm, String> {
    match name {
        None | Some("HS256") => Ok(Algorithm::HS256),
        Some("HS384") => Ok(Algorithm::HS384),
        Some("HS512") => Ok(Algorithm::HS512),
        Some(other) => Err(format!(
            "Unknown jwt-auth algorithm '{}' — supported: HS256, HS384, HS512",
            other
        )),
    }
}

/// Authenticates requests by verifying a JWT signature and expiry (`exp`).
///
/// The token is read from the configured header (with an optional
/// `Bearer ` prefix stripped). On success the plugin writes into
/// `context.message`:
/// - `"jwt_claims"`: the full decoded claims object
/// - `"user_id"`: the `sub` claim, when present (convenience copy)
///
/// In consumer mode the matched consumer's identity is also attached
/// (`consumer.*` message keys + `X-Consumer-*` headers).
///
/// On failure the request is rejected with a 401 JSON response and a
/// `JWT_INVALID` error routed through the error port.
pub struct JwtAuthPlugin {
    /// Shared HMAC secret used to verify token signatures (inline mode).
    /// `None` when only consumer mode is configured.
    secret: Option<String>,
    /// HMAC algorithm the token must be signed with in inline mode.
    algorithm: Algorithm,
    /// Lowercased name of the request header carrying the token.
    header_name: String,
    /// When true, tokens are also resolved against the consumer store via
    /// their `key` claim and verified with the consumer's own secret.
    use_consumers: bool,
    resources: Arc<PluginResources>,
}

impl JwtAuthPlugin {
    /// Builds the plugin from node config.
    ///
    /// Accepted keys:
    /// - `secret` (string, optional): HMAC secret for inline signature
    ///   verification. When set, every token is verified with it.
    /// - `use_consumers` (bool, default `false`): resolve the token's `key`
    ///   claim against the gateway's `consumers:` section
    ///   (`jwt-auth: {key, secret, algorithm}`) and verify with the matched
    ///   consumer's secret/algorithm, attaching the consumer on success.
    /// - At least one of `secret` / `use_consumers` must be provided.
    /// - `algorithm` (string, default `"HS256"`): one of `HS256`, `HS384`,
    ///   `HS512` used for inline verification; any other value is a config
    ///   error — an auth plugin must not silently verify with a different
    ///   algorithm than the one requested.
    /// - `header_name` (string, default `"authorization"`): header to read
    ///   the token from (compared case-insensitively via lowercasing).
    ///
    /// ```yaml
    /// type: jwt-auth
    /// config:
    ///   use_consumers: true
    ///   header_name: authorization
    /// ```
    pub fn from_config(
        config: &HashMap<String, serde_json::Value>,
        resources: &Arc<PluginResources>,
    ) -> Result<Self, String> {
        let secret = config
            .get("secret")
            .and_then(|v| v.as_str())
            .map(String::from);

        let use_consumers = config
            .get("use_consumers")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        if secret.is_none() && !use_consumers {
            return Err("jwt-auth plugin requires 'secret' or 'use_consumers: true'".to_string());
        }

        let algorithm = parse_algorithm(config.get("algorithm").and_then(|v| v.as_str()))?;

        let header_name = config
            .get("header_name")
            .and_then(|v| v.as_str())
            .unwrap_or("authorization")
            .to_lowercase();

        Ok(Self {
            secret,
            algorithm,
            header_name,
            use_consumers,
            resources: resources.clone(),
        })
    }

    /// Builds the 401 rejection with a JSON error body and returns a
    /// `PluginExecutionError` (code `JWT_INVALID`) carrying the context so the
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
                code: "JWT_INVALID".to_string(),
                message: message.to_string(),
                metadata: HashMap::new(),
            },
        })
    }

    /// Verifies `token` with `secret`/`algorithm` and, on success, writes the
    /// claims into `context.message`. Returns the decoded claims on success.
    fn verify(
        token: &str,
        secret: &str,
        algorithm: Algorithm,
        ctx: &mut Context,
    ) -> Result<HashMap<String, serde_json::Value>, String> {
        let key = DecodingKey::from_secret(secret.as_bytes());
        let mut validation = Validation::new(algorithm);
        validation.validate_exp = true;

        match decode::<HashMap<String, serde_json::Value>>(token, &key, &validation) {
            Ok(token_data) => {
                ctx.message.insert(
                    "jwt_claims".to_string(),
                    serde_json::to_value(&token_data.claims).unwrap_or_default(),
                );
                if let Some(sub) = token_data.claims.get("sub") {
                    ctx.message.insert("user_id".to_string(), sub.clone());
                }
                Ok(token_data.claims)
            }
            Err(e) => Err(format!("Invalid JWT: {}", e)),
        }
    }

    /// Reads the `key` claim from an *unverified* token payload.
    ///
    /// The payload segment is base64url-decoded and parsed as JSON purely to
    /// discover which consumer to look up; the signature is verified only
    /// afterwards with that consumer's secret, so no trust is placed in this
    /// value.
    fn peek_key_claim(token: &str) -> Option<String> {
        let payload = token.split('.').nth(1)?;
        let decoded = URL_SAFE_NO_PAD.decode(payload).ok()?;
        let claims: serde_json::Value = serde_json::from_slice(&decoded).ok()?;
        claims.get("key")?.as_str().map(String::from)
    }
}

#[async_trait]
impl Plugin for JwtAuthPlugin {
    fn plugin_type(&self) -> &str {
        "jwt-auth"
    }

    async fn execute(
        &self,
        mut ctx: Context,
        _named_inputs: &HashMap<String, serde_json::Value>,
    ) -> PluginResult {
        let token = ctx
            .request
            .headers
            .get(&self.header_name)
            .and_then(|v| v.first())
            .map(|v| {
                v.strip_prefix("Bearer ")
                    .map(String::from)
                    .unwrap_or_else(|| v.clone())
            });

        let token = match token {
            Some(t) => t,
            None => return Self::reject(ctx, "Missing authorization token"),
        };

        // Inline secret mode first.
        if let Some(ref secret) = self.secret {
            match Self::verify(&token, secret, self.algorithm, &mut ctx) {
                Ok(_) => {
                    return Ok(PluginOutput {
                        context: ctx,
                        named_outputs: HashMap::new(),
                    })
                }
                // With consumers also enabled, fall through and try them;
                // otherwise reject now.
                Err(e) if !self.use_consumers => return Self::reject(ctx, &e),
                Err(_) => {}
            }
        }

        // Consumer mode: the `key` claim selects the consumer, whose stored
        // secret/algorithm then verifies the signature.
        if self.use_consumers {
            let key_claim = match Self::peek_key_claim(&token) {
                Some(k) => k,
                None => return Self::reject(ctx, "JWT missing 'key' claim"),
            };
            let store = self.resources.consumers.load();
            if let Some(consumer) = store.find_by_credential("jwt-auth", &key_claim) {
                let cred = consumer.credentials.get("jwt-auth");
                let secret = cred.and_then(|c| c.get("secret")).and_then(|v| v.as_str());
                let algorithm = parse_algorithm(
                    cred.and_then(|c| c.get("algorithm"))
                        .and_then(|v| v.as_str()),
                );
                match (secret, algorithm) {
                    (Some(secret), Ok(algorithm)) => {
                        match Self::verify(&token, secret, algorithm, &mut ctx) {
                            Ok(_) => {
                                attach_consumer(&mut ctx, &consumer, "jwt-auth");
                                return Ok(PluginOutput {
                                    context: ctx,
                                    named_outputs: HashMap::new(),
                                });
                            }
                            Err(e) => return Self::reject(ctx, &e),
                        }
                    }
                    _ => return Self::reject(ctx, "Consumer has no valid jwt-auth secret"),
                }
            }
            return Self::reject(ctx, "Unknown JWT key");
        }

        Self::reject(ctx, "Invalid JWT")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::consumers::{ConsumerConfig, ConsumerStore};
    use crate::context::{GatewayRequest, GatewayResponse, Protocol};
    use jsonwebtoken::{encode, EncodingKey, Header};

    fn config(pairs: &[(&str, &str)]) -> HashMap<String, serde_json::Value> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), serde_json::Value::String(v.to_string())))
            .collect()
    }

    fn ctx_with_token(token: Option<&str>) -> Context {
        let mut headers = HashMap::new();
        if let Some(t) = token {
            headers.insert("authorization".to_string(), vec![format!("Bearer {}", t)]);
        }
        Context {
            request: GatewayRequest {
                method: "GET".to_string(),
                path: "/".to_string(),
                host: "h".to_string(),
                scheme: "http".to_string(),
                headers,
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

    /// Signs `claims` with `secret` and `alg`.
    fn make_token(claims: serde_json::Value, secret: &str, alg: Algorithm) -> String {
        encode(
            &Header::new(alg),
            &claims,
            &EncodingKey::from_secret(secret.as_bytes()),
        )
        .unwrap()
    }

    #[test]
    fn test_requires_secret_or_consumers() {
        assert!(JwtAuthPlugin::from_config(&HashMap::new(), &PluginResources::empty()).is_err());
    }

    #[test]
    fn test_accepts_supported_algorithms() {
        for alg in ["HS256", "HS384", "HS512"] {
            let cfg = config(&[("secret", "s3cret"), ("algorithm", alg)]);
            assert!(
                JwtAuthPlugin::from_config(&cfg, &PluginResources::empty()).is_ok(),
                "{alg} should be accepted"
            );
        }
    }

    #[test]
    fn test_rejects_unknown_algorithm() {
        // An unsupported algorithm must fail at config load, not silently
        // verify with HS256.
        for alg in ["RS256", "ES256", "none"] {
            let cfg = config(&[("secret", "s3cret"), ("algorithm", alg)]);
            assert!(
                JwtAuthPlugin::from_config(&cfg, &PluginResources::empty()).is_err(),
                "{alg} should be rejected"
            );
        }
    }

    #[tokio::test]
    async fn test_inline_secret_still_works() {
        let cfg = config(&[("secret", "s3cret")]);
        let plugin = JwtAuthPlugin::from_config(&cfg, &PluginResources::empty()).unwrap();

        let token = make_token(
            serde_json::json!({ "sub": "u1", "exp": 9999999999u64 }),
            "s3cret",
            Algorithm::HS256,
        );
        let out = plugin
            .execute(ctx_with_token(Some(&token)), &HashMap::new())
            .await
            .unwrap();
        assert_eq!(
            out.context.message.get("user_id"),
            Some(&serde_json::json!("u1"))
        );

        // wrong secret rejected
        let forged = make_token(
            serde_json::json!({ "sub": "u1", "exp": 9999999999u64 }),
            "other",
            Algorithm::HS256,
        );
        assert!(plugin
            .execute(ctx_with_token(Some(&forged)), &HashMap::new())
            .await
            .is_err());
    }

    fn resources_with_consumers() -> Arc<PluginResources> {
        let resources = PluginResources::empty();
        let consumers: Vec<ConsumerConfig> = serde_json::from_value(serde_json::json!([
            {
                "name": "alice",
                "credentials": {
                    "jwt-auth": { "key": "alice-key", "secret": "alice-secret", "algorithm": "HS256" }
                }
            }
        ]))
        .unwrap();
        resources
            .consumers
            .store(Arc::new(ConsumerStore::from_config(&consumers).unwrap()));
        resources
    }

    #[tokio::test]
    async fn test_consumer_mode_verifies_and_attaches() {
        let resources = resources_with_consumers();
        let mut cfg = HashMap::new();
        cfg.insert("use_consumers".to_string(), serde_json::json!(true));
        let plugin = JwtAuthPlugin::from_config(&cfg, &resources).unwrap();

        let token = make_token(
            serde_json::json!({ "key": "alice-key", "sub": "alice", "exp": 9999999999u64 }),
            "alice-secret",
            Algorithm::HS256,
        );
        let out = plugin
            .execute(ctx_with_token(Some(&token)), &HashMap::new())
            .await
            .unwrap();
        assert_eq!(
            out.context.message.get("consumer.name"),
            Some(&serde_json::json!("alice"))
        );

        // right key claim but token signed with the wrong secret is rejected
        let forged = make_token(
            serde_json::json!({ "key": "alice-key", "exp": 9999999999u64 }),
            "wrong-secret",
            Algorithm::HS256,
        );
        assert!(plugin
            .execute(ctx_with_token(Some(&forged)), &HashMap::new())
            .await
            .is_err());

        // unknown key claim is rejected
        let unknown = make_token(
            serde_json::json!({ "key": "nobody", "exp": 9999999999u64 }),
            "x",
            Algorithm::HS256,
        );
        assert!(plugin
            .execute(ctx_with_token(Some(&unknown)), &HashMap::new())
            .await
            .is_err());
    }
}
