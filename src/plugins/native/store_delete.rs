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
use crate::plugins::{Plugin, PluginResult};
use crate::vars::template::Template;

#[cfg(feature = "redis-store")]
use crate::plugins::PluginOutput;

#[derive(Debug)]
pub struct StoreDeletePlugin {
    store: StoreHandle,
    key: Template,
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
            return Err(store_kv::key_invalid(
                ctx,
                "store-delete",
                "DEL",
                &self.store.name,
            ));
        }
        let key = self.store.key_for(&rendered);

        let mut conn = match self.store.conn().await {
            Ok(c) => c,
            Err(e) => {
                return Err(store_kv::store_error(
                    ctx,
                    "store-delete",
                    "DEL",
                    &self.store.name,
                    e,
                ))
            }
        };

        // DEL returns how many keys it removed; 0 is not a failure.
        match conn.del::<_, u64>(&key).await {
            Ok(_) => Ok(PluginOutput::success(ctx)),
            Err(e) => Err(store_kv::store_error(
                ctx,
                "store-delete",
                "DEL",
                &self.store.name,
                e.to_string(),
            )),
        }
    }

    #[cfg(not(feature = "redis-store"))]
    async fn execute(&self, ctx: Context) -> PluginResult {
        Err(store_kv::store_error(
            ctx,
            "store-delete",
            "DEL",
            &self.store.name,
            "built without the redis-store feature".to_string(),
        ))
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
}
