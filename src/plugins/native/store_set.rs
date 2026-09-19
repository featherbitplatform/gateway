//! `store-set` — writes a key into a named store.

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

pub struct StoreSetPlugin {
    store: StoreHandle,
    key: Template,
    value: Template,
    ttl_seconds: Option<u64>,
}

// `StoreHandle`, `Template` and `Option<u64>` are all `Debug`, so this could
// derive -- except `ttl_seconds` is only read inside the
// `#[cfg(feature = "redis-store")]` `execute` body, which does not exist in
// a headless (`--no-default-features`) build. A derived impl doesn't count
// as a read for the dead-code pass, so deriving would leave `ttl_seconds`
// read nowhere there and fail `-D warnings`; `#[allow(dead_code)]` is off
// the table. Reading it here, unconditionally, keeps the headless build
// clean without the attribute.
impl std::fmt::Debug for StoreSetPlugin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StoreSetPlugin")
            .field("store", &self.store)
            .field("key", &self.key)
            .field("value", &self.value)
            .field("ttl_seconds", &self.ttl_seconds)
            .finish()
    }
}

impl StoreSetPlugin {
    /// Accepted keys:
    /// - `store` (string, required): a declared `stores:` entry.
    /// - `key` (string, required, templated): the key to write.
    /// - `value` (string, required, templated): the value to write.
    /// - `ttl_seconds` (integer, optional): expiry; omit for no expiry. `0` is
    ///   a config error, not "no expiry".
    pub fn from_config(
        config: &HashMap<String, serde_json::Value>,
        resources: &Arc<PluginResources>,
    ) -> Result<Self, String> {
        let key = store_kv::required_template(config, "key", "store-set")?;
        let value = store_kv::required_template(config, "value", "store-set")?;
        let ttl_seconds = store_kv::optional_ttl(config, "store-set")?;
        Ok(Self {
            store: store_kv::resolve(config, resources, "store-set")?,
            key,
            value,
            ttl_seconds,
        })
    }
}

#[async_trait]
impl Plugin for StoreSetPlugin {
    fn plugin_type(&self) -> &str {
        "store-set"
    }

    fn reads_response_body(&self) -> bool {
        self.key.references_response_body() || self.value.references_response_body()
    }

    #[cfg(feature = "redis-store")]
    async fn execute(&self, ctx: Context) -> PluginResult {
        use redis::AsyncCommands;

        let rendered = self.key.render(&ctx).to_string();
        if rendered.is_empty() {
            return Err(store_kv::key_invalid(
                ctx,
                "store-set",
                "SET",
                &self.store.name,
            ));
        }
        let key = self.store.key_for(&rendered);
        let value = self.value.render(&ctx).to_string();

        let mut conn = match self.store.conn().await {
            Ok(c) => c,
            Err(e) => {
                return Err(store_kv::store_error(
                    ctx,
                    "store-set",
                    "SET",
                    &self.store.name,
                    e,
                ))
            }
        };

        let result = match self.ttl_seconds {
            Some(ttl) => conn.set_ex::<_, _, ()>(&key, value, ttl).await,
            None => conn.set::<_, _, ()>(&key, value).await,
        };

        match result {
            Ok(()) => Ok(PluginOutput::success(ctx)),
            Err(e) => Err(store_kv::store_error(
                ctx,
                "store-set",
                "SET",
                &self.store.name,
                e.to_string(),
            )),
        }
    }

    #[cfg(not(feature = "redis-store"))]
    async fn execute(&self, ctx: Context) -> PluginResult {
        Err(store_kv::store_error(
            ctx,
            "store-set",
            "SET",
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
    fn test_requires_a_value() {
        let r = PluginResources::empty();
        let err =
            StoreSetPlugin::from_config(&cfg(serde_json::json!({ "store": "s", "key": "k" })), &r)
                .unwrap_err();
        assert!(err.contains("value"), "{err}");
    }

    /// `0` is a config error, not "no expiry" — and the check must happen
    /// before store resolution so the message is about the real problem.
    #[test]
    fn test_rejects_zero_ttl() {
        let r = PluginResources::empty();
        let err = StoreSetPlugin::from_config(
            &cfg(serde_json::json!({ "store": "s", "key": "k", "value": "1", "ttl_seconds": 0 })),
            &r,
        )
        .unwrap_err();
        assert!(err.contains("ttl_seconds"), "{err}");
    }
}
