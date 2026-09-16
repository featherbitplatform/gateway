//! Shared plumbing for the `store-*` nodes.
//!
//! Store resolution happens once, at policy-compile time: the node holds the
//! resulting handle and nothing reads the registry on the request path. This is
//! the pattern `limit-count` established (`src/plugins/native/limit_count.rs`).

use std::collections::HashMap;
use std::sync::Arc;

use serde_json::Value;

use crate::context::GatewayError;
use crate::plugins::resources::PluginResources;
use crate::vars::template::Template;

/// A resolved store, held by a node for its lifetime.
pub struct StoreHandle {
    /// Declared `stores:` name, carried for error messages.
    pub name: String,
    #[cfg(feature = "redis-store")]
    client: Arc<crate::stores::redis_store::RedisStoreClient>,
}

impl StoreHandle {
    /// A pooled connection to the store.
    #[cfg(feature = "redis-store")]
    pub async fn conn(&self) -> Result<redis::aio::ConnectionManager, String> {
        self.client.conn().await
    }

    /// The namespaced key this node should operate on.
    #[cfg(feature = "redis-store")]
    pub fn key_for(&self, rendered: &str) -> String {
        namespaced_key(self.client.key_prefix(), rendered)
    }
}

/// Builds the full redis key for a rendered policy key.
///
/// The `kv:` segment keeps policy-written keys from colliding with the `cnt:`,
/// session and `acme:` keys that share the same store.
#[cfg(feature = "redis-store")]
pub fn namespaced_key(prefix: &str, rendered: &str) -> String {
    format!(
        "{}:{}:{}",
        prefix,
        crate::stores::namespaces::POLICY_KV,
        rendered
    )
}

/// A `key` template that rendered to nothing.
///
/// `{prefix}:kv:` is still inside the namespace, so this is not a collision --
/// but it would put every request on one shared key, turning a template typo
/// into a cross-tenant leak. It fails loudly instead.
///
/// Only ever checked on the redis-backed data path (rendering a key is
/// pointless without a backend to read or write it against), so this is
/// `#[cfg(feature = "redis-store")]` like the rest of that path.
#[cfg(feature = "redis-store")]
pub fn key_invalid(node_type: &str) -> GatewayError {
    GatewayError {
        node_id: String::new(),
        code: "STORE_KEY_INVALID".to_string(),
        message: format!(
            "{}: the 'key' template rendered to an empty string",
            node_type
        ),
        metadata: HashMap::new(),
    }
}

/// Resolves the `store` config key to a handle, at construction time.
#[cfg(feature = "redis-store")]
pub fn resolve(
    config: &HashMap<String, Value>,
    resources: &Arc<PluginResources>,
    node_type: &str,
) -> Result<StoreHandle, String> {
    let name = config
        .get("store")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            format!(
                "{}: 'store' is required and must name a declared stores: entry",
                node_type
            )
        })?;
    let client = resources.stores.load().client(name)?;
    Ok(StoreHandle {
        name: name.to_string(),
        client,
    })
}

/// Without the `redis-store` feature there is no backend to resolve, so the
/// node fails at policy-compile time with a message that names the reason
/// rather than failing mysteriously at request time.
#[cfg(not(feature = "redis-store"))]
pub fn resolve(
    config: &HashMap<String, Value>,
    _resources: &Arc<PluginResources>,
    node_type: &str,
) -> Result<StoreHandle, String> {
    let name = config
        .get("store")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            format!(
                "{}: 'store' is required and must name a declared stores: entry",
                node_type
            )
        })?;
    Err(format!(
        "{}: store '{}': this binary was built without the redis-store feature",
        node_type, name
    ))
}

/// Parses a required, templated string field.
pub fn required_template(
    config: &HashMap<String, Value>,
    field: &str,
    node_type: &str,
) -> Result<Template, String> {
    let raw = config
        .get(field)
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| format!("{}: '{}' is required", node_type, field))?;
    let (tpl, warnings) = Template::parse(raw);
    for w in warnings {
        tracing::warn!(node_type = %node_type, field = %field, "{}", w);
    }
    Ok(tpl)
}

/// Parses the optional `ttl_seconds`. Absent means no expiry; `0` is rejected.
///
/// `store-get` (Task 2, the first caller of this module) had no use for a
/// TTL -- reading a key never sets one -- so this was test-only for a while.
/// `store-set` now calls it unconditionally from `from_config`, same as
/// `required_template`: this is pure config validation with no dependency on
/// a store actually being reachable, so unlike `resolve` it needs no
/// `redis-store`-gated pair -- one definition, always compiled, matches how
/// it is called on every feature combination.
pub fn optional_ttl(
    config: &HashMap<String, Value>,
    node_type: &str,
) -> Result<Option<u64>, String> {
    match config.get("ttl_seconds") {
        None | Some(Value::Null) => Ok(None),
        Some(v) => {
            let n = v.as_u64().ok_or_else(|| {
                format!(
                    "{}: 'ttl_seconds' must be a non-negative integer",
                    node_type
                )
            })?;
            if n == 0 {
                return Err(format!(
                    "{}: 'ttl_seconds' must be greater than 0; omit the field for no expiry",
                    node_type
                ));
            }
            Ok(Some(n))
        }
    }
}

/// A store outage. Never conflated with a miss: see the spec's §7.
pub fn store_error(node_type: &str, op: &str, store: &str, msg: String) -> GatewayError {
    let mut metadata = HashMap::new();
    metadata.insert("store".to_string(), Value::String(store.to_string()));
    metadata.insert("op".to_string(), Value::String(op.to_string()));
    GatewayError {
        node_id: String::new(),
        code: "STORE_ERROR".to_string(),
        message: format!("{}: {} failed: {}", node_type, op, msg),
        metadata,
    }
}

/// A value that exists but is not usable as configured (bad JSON, non-numeric).
///
/// Only ever raised on the redis-backed data path -- see [`key_invalid`].
#[cfg(feature = "redis-store")]
pub fn value_invalid(node_type: &str, msg: String) -> GatewayError {
    GatewayError {
        node_id: String::new(),
        code: "STORE_VALUE_INVALID".to_string(),
        message: format!("{}: {}", node_type, msg),
        metadata: HashMap::new(),
    }
}

#[cfg(all(test, feature = "redis-store"))]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn cfg(json: serde_json::Value) -> HashMap<String, serde_json::Value> {
        serde_json::from_value(json).unwrap()
    }

    /// Keys are namespaced so a policy cannot collide with session, counter or
    /// ACME keys in a store all of them share.
    #[test]
    fn test_key_for_namespaces_under_kv() {
        assert_eq!(namespaced_key("fb", "retry:abc"), "fb:kv:retry:abc");
    }

    /// `key` is templated, so its rendered value can contain anything a caller
    /// can put in a header. The rendered part is a *suffix*, so it cannot walk
    /// back out of the namespace -- assert that against hostile inputs rather
    /// than assuming it.
    #[test]
    fn test_a_rendered_key_cannot_escape_the_kv_namespace() {
        let hostile = [
            ":sess:{abc}",
            "../sess:{abc}",
            "fb:sess:{abc}",
            "fb:acme:account",
            "a\nfb:cnt:0:u1",
        ];
        for h in hostile {
            let built = namespaced_key("fb", h);
            assert!(
                built.starts_with("fb:kv:"),
                "escaped the namespace: {built}"
            );
            for ns in crate::stores::namespaces::MANAGED {
                assert!(
                    !built.starts_with(&format!("fb:{ns}:")),
                    "{built} lands in the {ns} namespace"
                );
            }
        }
    }

    /// An empty rendered key is not a collision -- `fb:kv:` is still inside the
    /// namespace -- but it silently puts every request on one shared key, so a
    /// template typo becomes a cross-tenant leak. Fail loudly instead.
    #[test]
    fn test_empty_key_has_its_own_code() {
        assert_eq!(key_invalid("store-get").code, "STORE_KEY_INVALID");
    }

    #[test]
    fn test_required_template_rejects_a_missing_key() {
        let err = required_template(&cfg(serde_json::json!({})), "key", "store-get").unwrap_err();
        assert!(
            err.contains("store-get"),
            "error must name the node type: {err}"
        );
        assert!(err.contains("key"), "error must name the field: {err}");
    }

    #[test]
    fn test_required_template_parses_a_template() {
        let t = required_template(
            &cfg(serde_json::json!({ "key": "retry:{{request.path}}" })),
            "key",
            "store-get",
        )
        .unwrap();
        assert!(!t.is_literal());
    }

    /// `0` is a config error, not "no expiry": the two readings are too easy to
    /// confuse, and omitting the field is how you say "no expiry".
    #[test]
    fn test_optional_ttl_rejects_zero() {
        let err =
            optional_ttl(&cfg(serde_json::json!({ "ttl_seconds": 0 })), "store-set").unwrap_err();
        assert!(err.contains("ttl_seconds"), "{err}");
    }

    #[test]
    fn test_optional_ttl_absent_is_none_and_present_is_some() {
        assert_eq!(
            optional_ttl(&cfg(serde_json::json!({})), "store-set").unwrap(),
            None
        );
        assert_eq!(
            optional_ttl(&cfg(serde_json::json!({ "ttl_seconds": 300 })), "store-set").unwrap(),
            Some(300)
        );
    }

    /// A store outage must be identifiable downstream, so the code is fixed and
    /// the metadata carries enough to debug it.
    #[test]
    fn test_store_error_carries_code_store_and_op() {
        let e = store_error(
            "store-get",
            "GET",
            "sessions",
            "connection refused".to_string(),
        );
        assert_eq!(e.code, "STORE_ERROR");
        assert_eq!(e.metadata.get("store").unwrap(), "sessions");
        assert_eq!(e.metadata.get("op").unwrap(), "GET");
        assert!(e.message.contains("connection refused"), "{}", e.message);
    }

    #[test]
    fn test_value_invalid_has_its_own_code() {
        let e = value_invalid("store-get", "expected JSON".to_string());
        assert_eq!(e.code, "STORE_VALUE_INVALID");
    }
}
