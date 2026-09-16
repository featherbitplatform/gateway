//! `store-delete` — removes a key from a named store.
//!
//! Deleting a key that does not exist is a success. This is the one place the
//! design deliberately does not mirror `store-get`: a `miss` port here would be
//! mandatory-wired in every policy that clears state, to signal something
//! almost no caller acts on.

use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::Arc;

use crate::context::Context;
use crate::plugins::resources::PluginResources;
use crate::plugins::util::store_kv::{self, StoreHandle};
use crate::plugins::{Plugin, PluginExecutionError, PluginResult};
use crate::vars::template::Template;

#[cfg(feature = "redis-store")]
use crate::plugins::PluginOutput;

pub struct StoreDeletePlugin {
    store: StoreHandle,
    key: Template,
}

// `StoreHandle` does not derive `Debug` (its redis client does not), so this
// is written by hand rather than derived; the store's declared name is enough
// to identify an instance in a panic message.
impl std::fmt::Debug for StoreDeletePlugin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StoreDeletePlugin")
            .field("store", &self.store.name)
            .field("key", &self.key)
            .finish()
    }
}

impl StoreDeletePlugin {
    /// Accepted keys:
    /// - `store` (string, required): a declared `stores:` entry.
    /// - `key` (string, required, templated): the key to delete.
    pub fn from_config(
        config: &HashMap<String, serde_json::Value>,
        resources: &Arc<PluginResources>,
    ) -> Result<Self, String> {
        let key = store_kv::required_template(config, "key", "store-delete")?;
        Ok(Self {
            store: store_kv::resolve(config, resources, "store-delete")?,
            key,
        })
    }
}

#[async_trait]
impl Plugin for StoreDeletePlugin {
    fn plugin_type(&self) -> &str {
        "store-delete"
    }

    fn reads_response_body(&self) -> bool {
        self.key.references_response_body()
    }

    #[cfg(feature = "redis-store")]
    async fn execute(&self, ctx: Context) -> PluginResult {
        use redis::AsyncCommands;

        let rendered = self.key.render(&ctx).to_string();
        if rendered.is_empty() {
            return Err(PluginExecutionError {
                context: ctx,
                error: store_kv::key_invalid("store-delete"),
            });
        }
        let key = self.store.key_for(&rendered);

        let mut conn = match self.store.conn().await {
            Ok(c) => c,
            Err(e) => {
                return Err(PluginExecutionError {
                    context: ctx,
                    error: store_kv::store_error("store-delete", "DEL", &self.store.name, e),
                })
            }
        };

        // DEL returns how many keys it removed; 0 is not a failure.
        match conn.del::<_, u64>(&key).await {
            Ok(_) => Ok(PluginOutput::success(ctx)),
            Err(e) => Err(PluginExecutionError {
                context: ctx,
                error: store_kv::store_error(
                    "store-delete",
                    "DEL",
                    &self.store.name,
                    e.to_string(),
                ),
            }),
        }
    }

    #[cfg(not(feature = "redis-store"))]
    async fn execute(&self, ctx: Context) -> PluginResult {
        Err(PluginExecutionError {
            context: ctx,
            error: store_kv::store_error(
                "store-delete",
                "DEL",
                &self.store.name,
                "built without the redis-store feature".to_string(),
            ),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugins::resources::PluginResources;
    use std::collections::HashMap;

    fn cfg(json: serde_json::Value) -> HashMap<String, serde_json::Value> {
        serde_json::from_value(json).unwrap()
    }

    #[test]
    fn test_requires_a_key() {
        let r = PluginResources::empty();
        let err = StoreDeletePlugin::from_config(&cfg(serde_json::json!({ "store": "s" })), &r)
            .unwrap_err();
        assert!(err.contains("key"), "{err}");
    }

    /// `key` is a template, so a key referencing the response body makes this
    /// node a body reader. Asserted on the parsed template, which needs no
    /// live store.
    #[test]
    fn test_a_key_referencing_the_response_body_is_detected() {
        let (plain, _) = Template::parse("k:{{request.path}}");
        let (reads, _) = Template::parse("k:{{response.body}}");
        assert!(!plain.references_response_body());
        assert!(reads.references_response_body());
    }
}
