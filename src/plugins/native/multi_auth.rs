//! Multi-authentication plugin (`multi-auth`).
//!
//! Chains several auth plugins and accepts the request as soon as **any** of
//! them succeeds (first success wins); the request is rejected with a 401,
//! exiting on the `denied` port, only when *all* of them fail (or exit on a
//! sub-plugin's own alternate outcome port). This mirrors APISIX's
//! `multi-auth`, which runs each configured auth plugin's `rewrite` phase in
//! order and short-circuits on the first that authenticates.
//!
//! Sub-plugins are built at config load via [`crate::plugins::create_plugin`],
//! so every sub-config is validated up front and a bad entry fails fast.

use async_trait::async_trait;
use bytes::Bytes;
use std::collections::HashMap;
use std::sync::Arc;

use crate::context::Context;
use crate::plugins::resources::PluginResources;
use crate::plugins::{create_plugin, Plugin, PluginOutput, PluginResult};

/// Authenticates a request by trying a list of auth sub-plugins in order and
/// accepting the first that succeeds.
///
/// Each sub-plugin is a fully-fledged [`Plugin`] instance; on a **clean
/// success** (no named exit port) it has already mutated the context (e.g.
/// attached a consumer identity), so the winning sub-plugin's output is
/// returned verbatim. Anything else a sub-plugin returns — a raw `Err`, or an
/// `Ok` on an alternate outcome port such as a credential-auth plugin's
/// `denied` — is treated as a failed attempt, not a match: `multi-auth` has
/// no way to fan a single request out to more than one downstream route, so
/// only a plain success can end the chain early. Sub-plugins run in the
/// listed order; a later sub-plugin sees the context as left by prior
/// *failed* attempts, except that the response is reset between attempts so
/// a losing plugin's rejection body never leaks onto a subsequent success.
/// Auth plugins generally mutate the context only on success (leaving
/// request/message untouched on failure), so ordering is safe.
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

    /// Builds the 401 rejection returned when every sub-plugin failed, and
    /// exits on the `denied` port.
    fn reject(ctx: Context) -> PluginResult {
        let mut ctx = ctx;
        ctx.response.status_code = 401;
        ctx.response.body =
            Bytes::from(r#"{"error": "unauthorized", "message": "Authorization Failed"}"#);
        ctx.response.headers.insert(
            "content-type".to_string(),
            vec!["application/json".to_string()],
        );
        Ok(PluginOutput::on_port(ctx, "denied"))
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
    ) -> PluginResult {
        // Snapshot the pristine response so a failed attempt's rejection body
        // never leaks onto the request if a later attempt succeeds.
        let original_response = ctx.response.clone();
        let mut ctx = ctx;

        for sub in &self.sub_plugins {
            match sub.execute(ctx).await {
                // A clean success ends the chain.
                Ok(output) if output.port.is_none() => return Ok(output),
                // Anything else — a deliberate alternate outcome (e.g. a
                // credential-auth sub-plugin's `denied`) or a raw `Err` — is
                // treated as a failed attempt: reset the response (so a
                // losing rejection body never leaks onto a later success)
                // and try the next sub-plugin.
                Ok(output) => {
                    ctx = output.context;
                    ctx.response = original_response.clone();
                }
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
    use base64::Engine;
    use crate::consumers::{ConsumerConfig, ConsumerStore};
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
        // "beta" fails the first key-auth (which now exits Ok on the `denied`
        // port, not Err) but passes the second -> a clean success.
        let out = plugin
            .execute(ctx_with_key(Some("beta")))
            .await
            .unwrap();
        assert_eq!(out.port, None);
        // A prior failed attempt must not leave a 401 body behind.
        assert_eq!(out.context.response.status_code, 0);
    }

    /// Pins the short-circuit: once the first sub-plugin returns a clean
    /// success, the loop must return immediately and never even invoke later
    /// sub-plugins. Proven by giving the second sub-plugin a mutation the
    /// first cannot produce (attaching a consumer identity via the consumer
    /// store) and asserting it never lands on the context.
    #[tokio::test]
    async fn test_first_sub_plugin_short_circuits_the_chain() {
        let resources = PluginResources::empty();
        let consumers: Vec<ConsumerConfig> = serde_json::from_value(serde_json::json!([
            {
                "name": "eve",
                "credentials": { "key-auth": { "key": "alpha" } }
            }
        ]))
        .unwrap();
        resources
            .consumers
            .store(Arc::new(ConsumerStore::from_config(&consumers).unwrap()));

        let mut config = HashMap::new();
        config.insert(
            "auth_plugins".to_string(),
            serde_json::json!([
                // Matches "alpha" via its inline key list -- no consumer attach.
                { "key-auth": { "keys": ["alpha"] } },
                // Would ALSO match "alpha" (via the consumer store above) and
                // attach the "eve" identity, if it ever ran.
                { "key-auth": { "use_consumers": true } },
            ]),
        );
        let plugin = MultiAuthPlugin::from_config(&config, &resources).unwrap();

        let out = plugin.execute(ctx_with_key(Some("alpha"))).await.unwrap();
        assert_eq!(out.port, None);
        // If the second sub-plugin had run, this would be `Some("eve")`.
        assert_eq!(out.context.message.get("consumer.name"), None);
    }

    /// A sub-plugin's genuine infrastructure failure (not a deliberate denial)
    /// must be swallowed as a failed attempt, same as a deliberate `denied`,
    /// so the chain continues to the next sub-plugin instead of aborting the
    /// whole node with an `Err`. Uses a real `ldap-auth` sub-plugin pointed at
    /// a closed port so its bind attempt genuinely errors (connection
    /// refused) rather than being rejected up front for a missing/malformed
    /// credential.
    #[tokio::test]
    async fn test_mid_chain_infra_failure_is_absorbed_not_propagated() {
        let mut config = HashMap::new();
        config.insert(
            "auth_plugins".to_string(),
            serde_json::json!([
                {
                    "ldap-auth": {
                        "base_dn": "dc=example,dc=org",
                        "ldap_uri": "ldap://127.0.0.1:1",
                        "timeout_ms": 200,
                    }
                },
                { "key-auth": { "keys": ["nomatch"] } },
            ]),
        );
        let plugin = MultiAuthPlugin::from_config(&config, &PluginResources::empty()).unwrap();

        // A well-formed Basic credential so ldap-auth gets past its own
        // up-front validation and actually attempts the (failing) network
        // bind, instead of rejecting before ever touching the network.
        let mut headers = HashMap::new();
        headers.insert(
            "authorization".to_string(),
            vec![format!(
                "Basic {}",
                base64::engine::general_purpose::STANDARD.encode("alice:secret")
            )],
        );
        let ctx = Context::new(GatewayRequest {
            method: "GET".into(),
            path: "/".into(),
            host: "h".into(),
            scheme: "http".into(),
            headers,
            query_params: HashMap::new(),
            body: Bytes::new(),
            remote_addr: "1.2.3.4:5".into(),
            protocol: Protocol::Http1,
        });

        // The ldap-auth sub-plugin's connection error is absorbed as a failed
        // attempt (not propagated as multi-auth's own `Err`); key-auth then
        // also denies (no matching key); every sub-plugin is exhausted, so
        // the result is multi-auth's own `denied` 401 -- not an `Err`.
        let out = plugin.execute(ctx).await.unwrap();
        assert_eq!(out.port, Some("denied"));
        assert_eq!(out.context.response.status_code, 401);
    }

    #[tokio::test]
    async fn test_all_fail_rejects_401() {
        let plugin =
            MultiAuthPlugin::from_config(&two_key_auth_config(), &PluginResources::empty())
                .unwrap();
        let out = plugin
            .execute(ctx_with_key(Some("gamma")))
            .await
            .unwrap();
        assert_eq!(out.port, Some("denied"));
        assert_eq!(out.context.response.status_code, 401);
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
