//! `store-incr` — atomically increments a counter in a named store.
//!
//! The TTL is applied when the key is **created** and never refreshed. A
//! counter that bounds retries must expire a fixed time after it first appears;
//! refreshing on every increment means a client that keeps retrying keeps the
//! counter alive and the bound never resets.

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

/// INCRBY, then set the expiry only if the key does not already have one.
/// `TTL` returns a negative value when the key has no expiry, so the guard also
/// repairs a key that somehow lost one. One round trip, atomic.
///
/// Only ever loaded into a `redis::Script` on the redis-backed data path (see
/// `StoreIncrPlugin::script`), so this is `#[cfg(feature = "redis-store")]`
/// like the rest of that path -- without the feature there is no consumer.
#[cfg(feature = "redis-store")]
const INCR_SCRIPT: &str = r#"
local n = redis.call('INCRBY', KEYS[1], ARGV[1])
if tonumber(ARGV[2]) > 0 and redis.call('TTL', KEYS[1]) < 0 then
  redis.call('EXPIRE', KEYS[1], ARGV[2])
end
return n
"#;

pub struct StoreIncrPlugin {
    store: StoreHandle,
    key: Template,
    by: i64,
    ttl_seconds: Option<u64>,
    name: String,
    #[cfg(feature = "redis-store")]
    script: redis::Script,
}

// `StoreHandle`, `Template` and `redis::Script` are all `Debug`, so this
// could derive -- except `by`/`ttl_seconds`/`name` are only read inside the
// `#[cfg(feature = "redis-store")]` `execute` body, which does not exist in
// a headless (`--no-default-features`) build. A derived impl doesn't count
// as a read for the dead-code pass, so deriving would leave those three
// fields read nowhere there and fail `-D warnings`; `#[allow(dead_code)]` is
// off the table. Reading them here, unconditionally, keeps the headless
// build clean without the attribute.
impl std::fmt::Debug for StoreIncrPlugin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StoreIncrPlugin")
            .field("store", &self.store)
            .field("key", &self.key)
            .field("by", &self.by)
            .field("ttl_seconds", &self.ttl_seconds)
            .field("name", &self.name)
            .finish()
    }
}

/// Parses the optional `by` amount, which defaults to 1 and may be negative.
fn parse_by(config: &HashMap<String, serde_json::Value>) -> Result<i64, String> {
    match config.get("by") {
        None | Some(serde_json::Value::Null) => Ok(1),
        Some(v) => v
            .as_i64()
            .ok_or_else(|| "store-incr: 'by' must be an integer".to_string()),
    }
}

impl StoreIncrPlugin {
    /// Accepted keys:
    /// - `store` (string, required): a declared `stores:` entry.
    /// - `key` (string, required, templated): the counter key.
    /// - `by` (integer, default `1`): amount to add; may be negative.
    /// - `ttl_seconds` (integer, optional): expiry applied **only when the key
    ///   is created**.
    /// - `name` (string, required): `context.message` key receiving the new value.
    pub fn from_config(
        config: &HashMap<String, serde_json::Value>,
        resources: &Arc<PluginResources>,
    ) -> Result<Self, String> {
        let name = config
            .get("name")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                "store-incr: 'name' is required (the context.message key receiving the new value)"
                    .to_string()
            })?
            .to_string();
        let key = store_kv::required_template(config, "key", "store-incr")?;
        let by = parse_by(config)?;
        let ttl_seconds = store_kv::optional_ttl(config, "store-incr")?;
        Ok(Self {
            store: store_kv::resolve(config, resources, "store-incr")?,
            key,
            by,
            ttl_seconds,
            name,
            #[cfg(feature = "redis-store")]
            script: redis::Script::new(INCR_SCRIPT),
        })
    }
}

#[async_trait]
impl Plugin for StoreIncrPlugin {
    fn plugin_type(&self) -> &str {
        "store-incr"
    }

    fn reads_response_body(&self) -> bool {
        self.key.references_response_body()
    }

    #[cfg(feature = "redis-store")]
    async fn execute(&self, mut ctx: Context) -> PluginResult {
        let rendered = self.key.render(&ctx).to_string();
        if rendered.is_empty() {
            return Err(store_kv::key_invalid(
                ctx,
                "store-incr",
                "INCRBY",
                &self.store.name,
            ));
        }
        let key = self.store.key_for(&rendered);

        let mut conn = match self.store.conn().await {
            Ok(c) => c,
            Err(e) => {
                return Err(store_kv::store_error(
                    ctx,
                    "store-incr",
                    "INCRBY",
                    &self.store.name,
                    e,
                ))
            }
        };

        let n: i64 = match self
            .script
            .key(key.as_str())
            .arg(self.by)
            .arg(self.ttl_seconds.unwrap_or(0))
            .invoke_async(&mut conn)
            .await
        {
            Ok(n) => n,
            Err(e) => {
                // A counter key holding a non-numeric value, or one holding
                // the wrong Redis type entirely (WRONGTYPE, e.g. a list), is
                // a config/data problem, not an outage, and gets its own
                // code. The rendered (un-namespaced) key is used in the
                // message, matching `store-get`.
                return Err(if store_kv::is_value_type_error(&e) {
                    store_kv::value_invalid(
                        ctx,
                        "store-incr",
                        "INCRBY",
                        &self.store.name,
                        format!("value at '{}' is not usable as an integer", rendered),
                    )
                } else {
                    store_kv::store_error(
                        ctx,
                        "store-incr",
                        "INCRBY",
                        &self.store.name,
                        e.to_string(),
                    )
                });
            }
        };

        ctx.message
            .insert(self.name.clone(), serde_json::Value::from(n));
        Ok(PluginOutput::success(ctx))
    }

    #[cfg(not(feature = "redis-store"))]
    async fn execute(&self, ctx: Context) -> PluginResult {
        Err(store_kv::store_error(
            ctx,
            "store-incr",
            "INCRBY",
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
    fn test_requires_a_name() {
        let r = PluginResources::empty();
        let err =
            StoreIncrPlugin::from_config(&cfg(serde_json::json!({ "store": "s", "key": "k" })), &r)
                .unwrap_err();
        assert!(err.contains("name"), "{err}");
    }

    #[test]
    fn test_by_defaults_to_one_and_accepts_negatives() {
        assert_eq!(parse_by(&cfg(serde_json::json!({}))).unwrap(), 1);
        assert_eq!(parse_by(&cfg(serde_json::json!({ "by": -2 }))).unwrap(), -2);
        assert!(parse_by(&cfg(serde_json::json!({ "by": "x" }))).is_err());
    }
}
