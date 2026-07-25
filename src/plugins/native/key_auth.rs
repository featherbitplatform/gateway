//! API-key authentication plugin (`key-auth`).
//!
//! Checks a request header (and optionally a query parameter as fallback)
//! against a static list of valid keys and/or the shared consumer store;
//! unmatched requests are rejected with a 401 error routed through the
//! node's error port.

use async_trait::async_trait;
use bytes::Bytes;
use std::collections::HashMap;
use std::sync::Arc;

use crate::consumers::attach_consumer;
use crate::context::{Context, GatewayError};
use crate::plugins::resources::PluginResources;
use crate::plugins::{Plugin, PluginExecutionError, PluginOutput, PluginResult};

/// Authenticates requests by matching an API key against a configured list
/// and/or the consumer store.
///
/// With inline `keys`, a valid key simply lets the request continue. With
/// `use_consumers: true`, the key is resolved against the gateway's
/// `consumers:` section (their `key-auth: {key}` credentials); on a match the
/// consumer's identity is attached to the request (`consumer.*` keys in
/// `context.message` plus `X-Consumer-*` headers) for downstream nodes. Both
/// sources may be enabled together — inline keys are checked first.
pub struct KeyAuthPlugin {
    /// Exact-match list of accepted API keys.
    valid_keys: Vec<String>,
    /// Lowercased name of the header the key is read from.
    header_name: String,
    /// Optional query parameter checked when the header is absent.
    query_param: Option<String>,
    /// When true, keys are also resolved against the consumer store.
    use_consumers: bool,
    /// Consumer attached when no credential matches (instead of rejecting).
    anonymous_consumer: Option<String>,
    /// When true, the matched header/query parameter is removed before the
    /// request is forwarded upstream.
    hide_credentials: bool,
    resources: Arc<PluginResources>,
}

impl KeyAuthPlugin {
    /// Builds the plugin from node config.
    ///
    /// Accepted keys:
    /// - `keys` (array of strings): inline valid API keys.
    /// - `use_consumers` (bool, default `false`): also resolve keys against
    ///   the gateway's `consumers:` section and attach the matched consumer.
    /// - At least one of `keys` / `use_consumers` must be provided.
    /// - `header_name` (string, default `"x-api-key"`): header to read the
    ///   key from (lowercased).
    /// - `query_param` (string, optional): query parameter used as a fallback
    ///   when the header is missing; no fallback if unset.
    /// - `anonymous_consumer` (string, optional): consumer name attached when
    ///   no credential matches, instead of rejecting (APISIX semantics).
    /// - `hide_credentials` (bool, default `false`): strip the key
    ///   header/query parameter before proxying upstream.
    ///
    /// ```yaml
    /// type: key-auth
    /// config:
    ///   use_consumers: true
    ///   header_name: x-api-key
    ///   query_param: api_key
    ///   hide_credentials: true
    /// ```
    pub fn from_config(
        config: &HashMap<String, serde_json::Value>,
        resources: &Arc<PluginResources>,
    ) -> Result<Self, String> {
        let valid_keys: Vec<String> = config
            .get("keys")
            .and_then(|v| v.as_array())
            .map(|seq| {
                seq.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();

        let use_consumers = config
            .get("use_consumers")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        if valid_keys.is_empty() && !use_consumers {
            return Err("key-auth plugin requires 'keys' or 'use_consumers: true'".to_string());
        }

        let header_name = config
            .get("header_name")
            .and_then(|v| v.as_str())
            .unwrap_or("x-api-key")
            .to_lowercase();

        let query_param = config
            .get("query_param")
            .and_then(|v| v.as_str())
            .map(String::from);

        let anonymous_consumer = config
            .get("anonymous_consumer")
            .and_then(|v| v.as_str())
            .map(String::from);

        let hide_credentials = config
            .get("hide_credentials")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        Ok(Self {
            valid_keys,
            header_name,
            query_param,
            use_consumers,
            anonymous_consumer,
            hide_credentials,
            resources: resources.clone(),
        })
    }

    /// Builds the 401 rejection with a JSON error body and returns a
    /// `PluginExecutionError` (code `UNAUTHORIZED`) carrying the context so
    /// the graph engine routes through the error port.
    fn reject(ctx: Context) -> PluginResult {
        let mut ctx = ctx;
        ctx.response.status_code = 401;
        ctx.response.body =
            Bytes::from(r#"{"error": "unauthorized", "message": "Invalid or missing API key"}"#);
        ctx.response.headers.insert(
            "content-type".to_string(),
            vec!["application/json".to_string()],
        );
        Err(PluginExecutionError {
            context: ctx,
            error: GatewayError {
                node_id: String::new(),
                code: "UNAUTHORIZED".to_string(),
                message: "Invalid or missing API key".to_string(),
                metadata: HashMap::new(),
            },
        })
    }

    /// Removes the credential from the request (per `hide_credentials`).
    fn strip_credential(&self, ctx: &mut Context) {
        ctx.request.headers.remove(&self.header_name);
        if let Some(ref param) = self.query_param {
            ctx.request.query_params.remove(param);
        }
    }
}

#[async_trait]
impl Plugin for KeyAuthPlugin {
    fn plugin_type(&self) -> &str {
        "key-auth"
    }

    async fn execute(
        &self,
        mut ctx: Context,
        _named_inputs: &HashMap<String, serde_json::Value>,
    ) -> PluginResult {
        // Try header first
        let key = ctx
            .request
            .headers
            .get(&self.header_name)
            .and_then(|v| v.first())
            .cloned();

        // Fall back to query param
        let key = key.or_else(|| {
            self.query_param.as_ref().and_then(|param| {
                ctx.request
                    .query_params
                    .get(param)
                    .and_then(|v| v.first())
                    .cloned()
            })
        });

        // Inline keys first.
        if let Some(ref k) = key {
            if self.valid_keys.contains(k) {
                if self.hide_credentials {
                    self.strip_credential(&mut ctx);
                }
                return Ok(PluginOutput {
                    context: ctx,
                    named_outputs: HashMap::new(),
                });
            }
        }

        // Consumer store.
        if self.use_consumers {
            if let Some(ref k) = key {
                let store = self.resources.consumers.load();
                if let Some(consumer) = store.find_by_credential("key-auth", k) {
                    if self.hide_credentials {
                        self.strip_credential(&mut ctx);
                    }
                    attach_consumer(&mut ctx, &consumer, "key-auth");
                    return Ok(PluginOutput {
                        context: ctx,
                        named_outputs: HashMap::new(),
                    });
                }
            }
        }

        // Anonymous fallback.
        if let Some(ref name) = self.anonymous_consumer {
            let store = self.resources.consumers.load();
            if let Some(consumer) = store.get(name) {
                attach_consumer(&mut ctx, &consumer, "key-auth");
                return Ok(PluginOutput {
                    context: ctx,
                    named_outputs: HashMap::new(),
                });
            }
        }

        Self::reject(ctx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::consumers::{ConsumerConfig, ConsumerStore};
    use crate::context::{GatewayRequest, GatewayResponse, Protocol};

    fn ctx_with_key(key: Option<&str>) -> Context {
        let mut headers = HashMap::new();
        if let Some(k) = key {
            headers.insert("x-api-key".to_string(), vec![k.to_string()]);
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

    fn resources_with_consumers() -> Arc<PluginResources> {
        let resources = PluginResources::empty();
        let consumers: Vec<ConsumerConfig> = serde_json::from_value(serde_json::json!([
            {
                "name": "alice",
                "credentials": { "key-auth": { "key": "alice-key" } }
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
    async fn test_consumer_key_attaches_identity() {
        let resources = resources_with_consumers();
        let mut config = HashMap::new();
        config.insert("use_consumers".to_string(), serde_json::json!(true));
        config.insert("hide_credentials".to_string(), serde_json::json!(true));
        let plugin = KeyAuthPlugin::from_config(&config, &resources).unwrap();

        let result = plugin
            .execute(ctx_with_key(Some("alice-key")), &HashMap::new())
            .await
            .unwrap();
        let ctx = result.context;
        assert_eq!(
            ctx.message.get("consumer.name"),
            Some(&serde_json::json!("alice"))
        );
        assert_eq!(
            ctx.request.headers.get("x-consumer-username"),
            Some(&vec!["alice".to_string()])
        );
        // hide_credentials stripped the key header
        assert!(!ctx.request.headers.contains_key("x-api-key"));
    }

    #[tokio::test]
    async fn test_unknown_key_rejected_or_anonymous() {
        let resources = resources_with_consumers();
        let mut config = HashMap::new();
        config.insert("use_consumers".to_string(), serde_json::json!(true));
        let plugin = KeyAuthPlugin::from_config(&config, &resources).unwrap();
        assert!(plugin
            .execute(ctx_with_key(Some("wrong")), &HashMap::new())
            .await
            .is_err());

        let mut config = HashMap::new();
        config.insert("use_consumers".to_string(), serde_json::json!(true));
        config.insert("anonymous_consumer".to_string(), serde_json::json!("guest"));
        let plugin = KeyAuthPlugin::from_config(&config, &resources).unwrap();
        let result = plugin
            .execute(ctx_with_key(None), &HashMap::new())
            .await
            .unwrap();
        assert_eq!(
            result.context.message.get("consumer.name"),
            Some(&serde_json::json!("guest"))
        );
    }

    #[tokio::test]
    async fn test_inline_keys_still_work() {
        let mut config = HashMap::new();
        config.insert("keys".to_string(), serde_json::json!(["k1"]));
        let plugin = KeyAuthPlugin::from_config(&config, &PluginResources::empty()).unwrap();
        assert!(plugin
            .execute(ctx_with_key(Some("k1")), &HashMap::new())
            .await
            .is_ok());
        assert!(plugin
            .execute(ctx_with_key(Some("k2")), &HashMap::new())
            .await
            .is_err());
    }

    #[test]
    fn test_requires_keys_or_consumers() {
        assert!(KeyAuthPlugin::from_config(&HashMap::new(), &PluginResources::empty()).is_err());
    }
}
