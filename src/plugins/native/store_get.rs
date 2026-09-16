//! `store-get` — reads a key from a named store into `context.message`.
//!
//! A missing key is not an error: it exits the dedicated `miss` outcome port,
//! which the compiler forces the policy to wire. A store *outage* exits `error`
//! instead — conflating the two would make a redis failure look exactly like
//! "nothing recorded", and a policy would take its happy path during precisely
//! the incident where that is most wrong.

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

pub struct StoreGetPlugin {
    store: StoreHandle,
    key: Template,
    name: String,
    json: bool,
}

// `StoreHandle` does not derive `Debug` (its redis client does not), so this
// is written by hand rather than derived; the store's declared name is enough
// to identify an instance in a panic message.
impl std::fmt::Debug for StoreGetPlugin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StoreGetPlugin")
            .field("store", &self.store.name)
            .field("key", &self.key)
            .field("name", &self.name)
            .field("json", &self.json)
            .finish()
    }
}

impl StoreGetPlugin {
    /// Accepted keys:
    /// - `store` (string, required): a declared `stores:` entry.
    /// - `key` (string, required, templated): the key to read.
    /// - `name` (string, required): `context.message` key to write.
    /// - `json` (bool, default `false`): parse a JSON object and flatten its
    ///   top-level fields into `message` as `<name>.<field>`.
    pub fn from_config(
        config: &HashMap<String, serde_json::Value>,
        resources: &Arc<PluginResources>,
    ) -> Result<Self, String> {
        let name = config
            .get("name")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                "store-get: 'name' is required (the context.message key to write)".to_string()
            })?
            .to_string();
        Ok(Self {
            key: store_kv::required_template(config, "key", "store-get")?,
            store: store_kv::resolve(config, resources, "store-get")?,
            name,
            json: config
                .get("json")
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
        })
    }
}

/// Writes `value` into `message` under `name`.
///
/// An object is flattened one level into `<name>.<field>` keys, because
/// `context.message` is a flat namespace: `message_str` does a plain
/// `get(key)` and `{{message.a.b}}` resolves the literal key `"a.b"` rather
/// than traversing. Anything else — scalar or array — is written under `name`
/// unchanged, so `$msg_<name>` keeps working for the counter case.
///
/// Only reachable from the redis-backed `execute` path (there is no value to
/// flatten without a store to have read it from), so this is
/// `#[cfg(feature = "redis-store")]` like the rest of that path.
#[cfg(feature = "redis-store")]
fn flatten_into(
    message: &mut HashMap<String, serde_json::Value>,
    name: &str,
    value: serde_json::Value,
) {
    match value {
        serde_json::Value::Object(map) => {
            for (k, v) in map {
                message.insert(format!("{}.{}", name, k), v);
            }
        }
        other => {
            message.insert(name.to_string(), other);
        }
    }
}

#[async_trait]
impl Plugin for StoreGetPlugin {
    fn plugin_type(&self) -> &str {
        "store-get"
    }

    fn reads_response_body(&self) -> bool {
        self.key.references_response_body()
    }

    #[cfg(feature = "redis-store")]
    async fn execute(&self, mut ctx: Context) -> PluginResult {
        use redis::AsyncCommands;

        let rendered = self.key.render(&ctx).to_string();
        if rendered.is_empty() {
            return Err(PluginExecutionError {
                context: ctx,
                error: store_kv::key_invalid("store-get"),
            });
        }
        let key = self.store.key_for(&rendered);

        let mut conn = match self.store.conn().await {
            Ok(c) => c,
            Err(e) => {
                return Err(PluginExecutionError {
                    context: ctx,
                    error: store_kv::store_error("store-get", "GET", &self.store.name, e),
                })
            }
        };

        let raw: Option<String> = match conn.get(&key).await {
            Ok(v) => v,
            Err(e) => {
                return Err(PluginExecutionError {
                    context: ctx,
                    error: store_kv::store_error(
                        "store-get",
                        "GET",
                        &self.store.name,
                        e.to_string(),
                    ),
                })
            }
        };

        let Some(raw) = raw else {
            return Ok(PluginOutput::on_port(ctx, "miss"));
        };

        if self.json {
            match serde_json::from_str::<serde_json::Value>(&raw) {
                Ok(v) => flatten_into(&mut ctx.message, &self.name, v),
                Err(e) => {
                    return Err(PluginExecutionError {
                        context: ctx,
                        error: store_kv::value_invalid(
                            "store-get",
                            format!("value at '{}' is not valid JSON: {}", rendered, e),
                        ),
                    })
                }
            }
        } else {
            ctx.message
                .insert(self.name.clone(), serde_json::Value::String(raw));
        }

        Ok(PluginOutput::success(ctx))
    }

    #[cfg(not(feature = "redis-store"))]
    async fn execute(&self, ctx: Context) -> PluginResult {
        Err(PluginExecutionError {
            context: ctx,
            error: store_kv::store_error(
                "store-get",
                "GET",
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
    use crate::vars::template::Template;
    use std::collections::HashMap;

    fn cfg(json: serde_json::Value) -> HashMap<String, serde_json::Value> {
        serde_json::from_value(json).unwrap()
    }

    #[test]
    fn test_requires_a_name_to_write_into_message() {
        let r = PluginResources::empty();
        let err =
            StoreGetPlugin::from_config(&cfg(serde_json::json!({ "store": "s", "key": "k" })), &r)
                .unwrap_err();
        assert!(err.contains("name"), "{err}");
    }

    /// A JSON object is flattened into dotted message keys, because
    /// `{{message.a.b}}` resolves the literal key "a.b" rather than traversing
    /// (`src/vars/mod.rs` message_str is a flat lookup).
    ///
    /// `flatten_into` only exists on the redis-backed data path (see its
    /// doc comment), hence the same feature gate here.
    #[cfg(feature = "redis-store")]
    #[test]
    fn test_flatten_object_writes_dotted_keys() {
        let mut msg = HashMap::new();
        flatten_into(
            &mut msg,
            "profile",
            serde_json::json!({"tier": "gold", "seats": 3}),
        );
        assert_eq!(msg.get("profile.tier").unwrap(), "gold");
        assert_eq!(msg.get("profile.seats").unwrap(), 3);
        assert!(
            !msg.contains_key("profile"),
            "the object itself must not be written"
        );
    }

    /// A JSON scalar keeps the plain name so `$msg_<name>` still works, which is
    /// the common case for a counter read back after store-incr.
    #[cfg(feature = "redis-store")]
    #[test]
    fn test_flatten_scalar_writes_the_plain_name() {
        let mut msg = HashMap::new();
        flatten_into(&mut msg, "retry_count", serde_json::json!(3));
        assert_eq!(msg.get("retry_count").unwrap(), 3);
    }

    /// Nested values are written as their own dotted key, not recursed into:
    /// one level is what a flat namespace can express honestly.
    #[cfg(feature = "redis-store")]
    #[test]
    fn test_flatten_does_not_recurse() {
        let mut msg = HashMap::new();
        flatten_into(&mut msg, "cfg", serde_json::json!({"limits": {"rps": 10}}));
        assert_eq!(
            msg.get("cfg.limits").unwrap(),
            &serde_json::json!({"rps": 10})
        );
        assert!(!msg.contains_key("cfg.limits.rps"));
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
