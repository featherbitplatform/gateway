//! Multi-authentication plugin (`multi-auth`).
//!
//! Chains several auth plugins and accepts the request as soon as **any** of
//! them succeeds (first success wins); the request is rejected with a 401 only
//! when *all* of them fail. This mirrors APISIX's `multi-auth`, which runs each
//! configured auth plugin's `rewrite` phase in order and short-circuits on the
//! first that authenticates.
//!
//! Sub-plugins are built at config load via [`crate::plugins::create_plugin`],
//! so every sub-config is validated up front and a bad entry fails fast.

use async_trait::async_trait;
use bytes::Bytes;
use std::collections::HashMap;
use std::sync::Arc;

use crate::context::{Context, GatewayError};
use crate::plugins::resources::PluginResources;
use crate::plugins::{create_plugin, Plugin, PluginExecutionError, PluginResult};

/// Authenticates a request by trying a list of auth sub-plugins in order and
/// accepting the first that succeeds.
///
/// Each sub-plugin is a fully-fledged [`Plugin`] instance; on success it has
/// already mutated the context (e.g. attached a consumer identity), so the
/// winning sub-plugin's output is returned verbatim. Sub-plugins run in the
/// listed order; a later sub-plugin sees the context as left by prior *failed*
/// attempts, except that the response is reset between attempts so a losing
/// plugin's rejection body never leaks onto a subsequent success. Auth plugins
/// generally mutate the context only on success (leaving request/message
/// untouched on failure), so ordering is safe.
///
/// Only auth-type plugins are meaningful here, but the set is **not**
/// hard-restricted — any registered plugin type may be listed, and non-auth
/// plugins simply run as ordinary nodes whose success ends the chain.
pub struct MultiAuthPlugin {
    /// Sub-plugins tried in order; the first `Ok` wins.
    sub_plugins: Vec<Box<dyn Plugin>>,
}

impl MultiAuthPlugin {
    /// Builds the plugin from node config.
    ///
    /// Accepted keys:
    /// - `auth_plugins` (array, required): each element is a **single-key map**
    ///   `{plugin-type: {that plugin's config}}`. Every entry is instantiated
    ///   through [`create_plugin`] at load time, so a bad sub-config (or an
    ///   unknown plugin type) fails fast here rather than at request time.
    ///   APISIX conventionally requires at least two entries; featherbit only
    ///   requires the array to be non-empty.
    ///
    /// ```yaml
    /// type: multi-auth
    /// config:
    ///   auth_plugins:
    ///     - key-auth:
    ///         use_consumers: true
    ///     - basic-auth:
    ///         use_consumers: true
    /// ```
    pub fn from_config(
        config: &HashMap<String, serde_json::Value>,
        resources: &Arc<PluginResources>,
    ) -> Result<Self, String> {
        let entries = config
            .get("auth_plugins")
            .and_then(|v| v.as_array())
            .ok_or("multi-auth plugin requires an 'auth_plugins' array")?;

        if entries.is_empty() {
            return Err("multi-auth 'auth_plugins' must not be empty".to_string());
        }

        let mut sub_plugins = Vec::with_capacity(entries.len());
        for (idx, entry) in entries.iter().enumerate() {
            let obj = entry.as_object().ok_or_else(|| {
                format!(
                    "multi-auth 'auth_plugins[{}]' must be a single-key map {{plugin-type: config}}",
                    idx
                )
            })?;
            if obj.len() != 1 {
                return Err(format!(
                    "multi-auth 'auth_plugins[{}]' must have exactly one key (the plugin type)",
                    idx
                ));
            }
            let (plugin_type, inner) = obj.iter().next().unwrap();
            let inner_config: HashMap<String, serde_json::Value> = inner
                .as_object()
                .map(|m| m.clone().into_iter().collect())
                .unwrap_or_default();
            let plugin = create_plugin(plugin_type, &inner_config, resources)
                .map_err(|e| format!("multi-auth sub-plugin '{}': {}", plugin_type, e))?;
            sub_plugins.push(plugin);
        }

        Ok(Self { sub_plugins })
    }

    /// Builds the 401 rejection (code `MULTI_AUTH_FAILED`) returned when every
    /// sub-plugin failed, carrying the context so the graph engine routes
    /// through the error port.
    fn reject(ctx: Context) -> PluginResult {
        let mut ctx = ctx;
        ctx.response.status_code = 401;
        ctx.response.body =
            Bytes::from(r#"{"error": "unauthorized", "message": "Authorization Failed"}"#);
        ctx.response.headers.insert(
            "content-type".to_string(),
            vec!["application/json".to_string()],
        );
        Err(PluginExecutionError {
            context: ctx,
            error: GatewayError {
                node_id: String::new(),
                code: "MULTI_AUTH_FAILED".to_string(),
                message: "all authentication methods failed".to_string(),
                metadata: HashMap::new(),
            },
        })
    }
}

#[async_trait]
impl Plugin for MultiAuthPlugin {
    fn plugin_type(&self) -> &str {
        "multi-auth"
    }

    async fn execute(
        &self,
        ctx: Context,
        named_inputs: &HashMap<String, serde_json::Value>,
    ) -> PluginResult {
        // Snapshot the pristine response so a failed attempt's rejection body
        // never leaks onto the request if a later attempt succeeds.
        let original_response = ctx.response.clone();
        let mut ctx = ctx;

        for sub in &self.sub_plugins {
            match sub.execute(ctx, named_inputs).await {
                Ok(output) => return Ok(output),
                Err(err) => {
                    ctx = err.context;
                    ctx.response = original_response.clone();
                }
            }
        }

        Self::reject(ctx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::{GatewayRequest, Protocol};

    fn ctx_with_key(key: Option<&str>) -> Context {
        let mut headers = HashMap::new();
        if let Some(k) = key {
            headers.insert("x-api-key".to_string(), vec![k.to_string()]);
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

    /// Two key-auth sub-plugins accepting disjoint key sets.
    fn two_key_auth_config() -> HashMap<String, serde_json::Value> {
        let mut config = HashMap::new();
        config.insert(
            "auth_plugins".to_string(),
            serde_json::json!([
                { "key-auth": { "keys": ["alpha"] } },
                { "key-auth": { "keys": ["beta"] } }
            ]),
        );
        config
    }

    #[tokio::test]
    async fn test_second_sub_plugin_succeeds() {
        let plugin =
            MultiAuthPlugin::from_config(&two_key_auth_config(), &PluginResources::empty())
                .unwrap();
        // "beta" fails the first key-auth but passes the second -> Ok.
        let out = plugin
            .execute(ctx_with_key(Some("beta")), &HashMap::new())
            .await
            .unwrap();
        // A prior failed attempt must not leave a 401 body behind.
        assert_eq!(out.context.response.status_code, 0);
    }

    #[tokio::test]
    async fn test_all_fail_rejects_401() {
        let plugin =
            MultiAuthPlugin::from_config(&two_key_auth_config(), &PluginResources::empty())
                .unwrap();
        let err = plugin
            .execute(ctx_with_key(Some("gamma")), &HashMap::new())
            .await
            .unwrap_err();
        assert_eq!(err.error.code, "MULTI_AUTH_FAILED");
        assert_eq!(err.context.response.status_code, 401);
    }

    #[test]
    fn test_requires_auth_plugins() {
        assert!(MultiAuthPlugin::from_config(&HashMap::new(), &PluginResources::empty()).is_err());
    }

    #[test]
    fn test_rejects_bad_sub_config() {
        // A sub-plugin whose config is invalid fails fast at load.
        let mut config = HashMap::new();
        config.insert(
            "auth_plugins".to_string(),
            serde_json::json!([{ "key-auth": {} }]),
        );
        assert!(MultiAuthPlugin::from_config(&config, &PluginResources::empty()).is_err());
    }
}
