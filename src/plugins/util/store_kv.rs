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

/// Integration tests against a real redis, gated on `FEATHERBIT_TEST_REDIS_URL`
/// -- the same env var CI's `redis:7`/`valkey:8` service-container matrix sets,
/// and the same skip-if-unset pattern as `src/acme/live_tests.rs` and
/// `src/stores/redis_store.rs::tests::test_ping_live`.
///
/// `PluginResources::empty()`, used by every unit test in the four `store-*`
/// plugins, has no declared stores, so none of those tests can exercise a
/// round trip against a backend -- that is what this module is for.
#[cfg(all(test, feature = "redis-store"))]
mod live_tests {
    use super::*;
    use crate::config::StoreConfig;
    use crate::context::{Context, GatewayRequest, Protocol};
    use crate::plugins::native::store_delete::StoreDeletePlugin;
    use crate::plugins::native::store_get::StoreGetPlugin;
    use crate::plugins::native::store_incr::StoreIncrPlugin;
    use crate::plugins::native::store_set::StoreSetPlugin;
    use crate::plugins::Plugin;
    use crate::stores::StoreRegistry;
    use std::collections::HashMap;
    use std::time::Duration;

    /// Skips unless a real store is configured, the same gate the session and
    /// ACME live tests use.
    fn store_url() -> Option<String> {
        std::env::var("FEATHERBIT_TEST_REDIS_URL")
            .ok()
            .filter(|s| !s.is_empty())
    }

    /// Builds `PluginResources` with one declared store named "test" pointing
    /// at `url`.
    fn resources_with_store(url: &str) -> Arc<PluginResources> {
        let store_cfg: StoreConfig =
            serde_yaml::from_str(&format!("name: test\ntype: redis\nurl: {url}\n"))
                .expect("store config parses");
        let registry = StoreRegistry::rebuild(&StoreRegistry::default(), &[store_cfg], None)
            .expect("store registry builds against a reachable url");
        let resources = PluginResources::empty();
        resources.stores.store(Arc::new(registry));
        resources
    }

    fn cfg(json: serde_json::Value) -> HashMap<String, serde_json::Value> {
        serde_json::from_value(json).unwrap()
    }

    fn test_ctx() -> Context {
        Context::new(GatewayRequest {
            method: "GET".to_string(),
            path: "/".to_string(),
            host: "example.com".to_string(),
            scheme: "http".to_string(),
            headers: HashMap::new(),
            query_params: HashMap::new(),
            body: bytes::Bytes::new(),
            remote_addr: "10.1.2.3:44321".to_string(),
            protocol: Protocol::Http1,
        })
    }

    /// A key unique to this run, so parallel tests in this module -- and
    /// leftover data from a previous, possibly crashed run against the same
    /// redis -- never collide with each other.
    fn unique_key(label: &str) -> String {
        format!("task6:{}:{}", label, uuid::Uuid::new_v4())
    }

    #[tokio::test]
    async fn test_set_then_get_round_trips() {
        let Some(url) = store_url() else {
            eprintln!("skipping test_set_then_get_round_trips: FEATHERBIT_TEST_REDIS_URL not set");
            return;
        };
        let resources = resources_with_store(&url);
        let key = unique_key("roundtrip");

        let set = StoreSetPlugin::from_config(
            &cfg(serde_json::json!({ "store": "test", "key": key, "value": "1" })),
            &resources,
        )
        .unwrap();
        let out = set.execute(test_ctx()).await.unwrap();
        assert_eq!(out.port, None);

        let get = StoreGetPlugin::from_config(
            &cfg(serde_json::json!({ "store": "test", "key": key, "name": "v" })),
            &resources,
        )
        .unwrap();
        let out = get.execute(test_ctx()).await.unwrap();
        assert_eq!(out.port, None);
        assert_eq!(out.context.message.get("v").unwrap(), "1");
    }

    #[tokio::test]
    async fn test_get_on_an_absent_key_exits_miss() {
        let Some(url) = store_url() else {
            eprintln!(
                "skipping test_get_on_an_absent_key_exits_miss: FEATHERBIT_TEST_REDIS_URL not set"
            );
            return;
        };
        let resources = resources_with_store(&url);
        let key = unique_key("absent");

        let get = StoreGetPlugin::from_config(
            &cfg(serde_json::json!({ "store": "test", "key": key, "name": "v" })),
            &resources,
        )
        .unwrap();
        let out = get.execute(test_ctx()).await.unwrap();
        assert_eq!(out.port, Some("miss"));
    }

    #[tokio::test]
    async fn test_json_true_flattens_an_object() {
        let Some(url) = store_url() else {
            eprintln!(
                "skipping test_json_true_flattens_an_object: FEATHERBIT_TEST_REDIS_URL not set"
            );
            return;
        };
        let resources = resources_with_store(&url);
        let key = unique_key("json");

        let set = StoreSetPlugin::from_config(
            &cfg(serde_json::json!({
                "store": "test",
                "key": key,
                "value": r#"{"tier":"gold"}"#,
            })),
            &resources,
        )
        .unwrap();
        set.execute(test_ctx()).await.unwrap();

        let get = StoreGetPlugin::from_config(
            &cfg(serde_json::json!({
                "store": "test",
                "key": key,
                "name": "profile",
                "json": true,
            })),
            &resources,
        )
        .unwrap();
        let out = get.execute(test_ctx()).await.unwrap();
        assert_eq!(out.context.message.get("profile.tier").unwrap(), "gold");
    }

    #[tokio::test]
    async fn test_json_true_on_invalid_json_exits_error() {
        let Some(url) = store_url() else {
            eprintln!(
                "skipping test_json_true_on_invalid_json_exits_error: FEATHERBIT_TEST_REDIS_URL not set"
            );
            return;
        };
        let resources = resources_with_store(&url);
        let key = unique_key("badjson");

        let set = StoreSetPlugin::from_config(
            &cfg(serde_json::json!({ "store": "test", "key": key, "value": "not json" })),
            &resources,
        )
        .unwrap();
        set.execute(test_ctx()).await.unwrap();

        let get = StoreGetPlugin::from_config(
            &cfg(serde_json::json!({
                "store": "test",
                "key": key,
                "name": "profile",
                "json": true,
            })),
            &resources,
        )
        .unwrap();
        let err = get.execute(test_ctx()).await.unwrap_err();
        assert_eq!(err.error.code, "STORE_VALUE_INVALID");
    }

    #[tokio::test]
    async fn test_delete_on_an_absent_key_succeeds() {
        let Some(url) = store_url() else {
            eprintln!(
                "skipping test_delete_on_an_absent_key_succeeds: FEATHERBIT_TEST_REDIS_URL not set"
            );
            return;
        };
        let resources = resources_with_store(&url);
        let key = unique_key("delete-absent");

        let delete = StoreDeletePlugin::from_config(
            &cfg(serde_json::json!({ "store": "test", "key": key })),
            &resources,
        )
        .unwrap();
        let out = delete.execute(test_ctx()).await.unwrap();
        assert_eq!(out.port, None);
    }

    /// The behavior store-incr exists for, and the one most likely to
    /// regress: the TTL is applied when the key is created and must never be
    /// refreshed by a later increment, or a client that keeps retrying would
    /// keep its own counter alive and the bound it exists to enforce would
    /// never reset.
    #[tokio::test]
    async fn test_incr_does_not_refresh_the_ttl() {
        let Some(url) = store_url() else {
            eprintln!(
                "skipping test_incr_does_not_refresh_the_ttl: FEATHERBIT_TEST_REDIS_URL not set"
            );
            return;
        };
        let resources = resources_with_store(&url);
        let key = unique_key("ttl");
        let redis_key = namespaced_key("fb", &key);

        let client = resources.stores.load().client("test").unwrap();
        let mut raw = client.conn().await.unwrap();

        let incr = StoreIncrPlugin::from_config(
            &cfg(serde_json::json!({
                "store": "test",
                "key": key,
                "name": "n",
                "ttl_seconds": 60,
            })),
            &resources,
        )
        .unwrap();

        incr.execute(test_ctx()).await.unwrap();
        let ttl1: i64 = redis::cmd("PTTL")
            .arg(&redis_key)
            .query_async(&mut raw)
            .await
            .unwrap();
        assert!(ttl1 > 0, "key must have a TTL right after creation: {ttl1}");

        tokio::time::sleep(Duration::from_millis(1100)).await;

        incr.execute(test_ctx()).await.unwrap();
        let ttl2: i64 = redis::cmd("PTTL")
            .arg(&redis_key)
            .query_async(&mut raw)
            .await
            .unwrap();

        // A weaker `ttl2 < ttl1` does not discriminate: even an unconditional
        // refresh resets the TTL to ~60000ms both times, and the two round
        // trips' own timing jitter alone makes `ttl2 < ttl1` about as likely
        // as not -- that assertion passed against a mutated, always-refresh
        // script in review. Require the drop to be close to the full sleep
        // instead: on the correct (guarded) script the TTL just keeps
        // counting down, so ttl1 - ttl2 tracks the ~1100ms sleep; on a
        // refreshing script it is reset back near 60000ms both times, so the
        // drop is near zero.
        let drop = ttl1 - ttl2;
        assert!(
            drop >= 900,
            "TTL must keep counting down by about the sleep duration, not be refreshed by a later increment: ttl1={ttl1} ttl2={ttl2} drop={drop}"
        );
    }

    /// A store outage must exit `error`, never `miss` -- the distinction the
    /// design turns on (see the module doc comment on
    /// `src/plugins/native/store_get.rs`).
    ///
    /// This does **not** point the store at a closed port, even though that
    /// was the brief's suggested mechanism. Empirically (measured on this
    /// box with `zz_diag_*` throwaway probes, since removed) it does not
    /// exercise this path quickly: `store-get` reaches the backend through
    /// `RedisStoreClient::conn()` (`src/stores/redis_store.rs`), which calls
    /// `redis::aio::ConnectionManager::new_with_config` with only
    /// `connection_timeout`/`response_timeout` overridden. `number_of_retries`
    /// (6), `factor` (100) and `max_delay` are left at the redis crate's /
    /// `backon`'s defaults, which back off up to a *jittered, 60-second-capped*
    /// delay between each of 6 retries on the very first connection failure --
    /// worst case minutes before `new_with_config` gives up and returns `Err`.
    /// `connect_timeout_ms`, the only knob `StoreConfig` exposes, bounds one
    /// attempt, not the backoff between retries, so it cannot shorten this. A
    /// raw TCP connect and a plain (non-`ConnectionManager`) redis connection
    /// both fail in ~2s against the same closed port -- confirming the delay
    /// is this retry/backoff policy, not the OS or this machine. That is a
    /// real, currently-live gap between the design's promised fast `503` and
    /// actual behavior on a fresh connection failure, worth its own follow-up;
    /// fixing it is out of this task's scope (`store_kv.rs` and
    /// `E2E_TESTBOOK.md` only), so this test does not wait on it.
    ///
    /// Instead it reaches the *same* `Err` arm in `store-get`'s `execute`
    /// (`conn.get(&key).await` failing) a different way that needs no new
    /// connection at all: writing a non-string value directly (bypassing
    /// `store-set`, which only ever writes strings) so `GET` fails with
    /// redis's own `WRONGTYPE` error against an already-healthy connection.
    /// `store-get` does not distinguish "the connection is down" from "the
    /// command itself errored" -- both are `Err(e)` from the one `conn.get()`
    /// call, mapped to the same `STORE_ERROR` by the same line of code -- so
    /// this exercises exactly the branch a real outage would take,
    /// deterministically and in milliseconds.
    #[tokio::test]
    async fn test_unreachable_store_exits_error_not_miss() {
        let Some(url) = store_url() else {
            eprintln!(
                "skipping test_unreachable_store_exits_error_not_miss: FEATHERBIT_TEST_REDIS_URL not set"
            );
            return;
        };
        let resources = resources_with_store(&url);
        let key = unique_key("backend-error");
        let redis_key = namespaced_key("fb", &key);

        // A list value: GET against it is a real backend error (WRONGTYPE),
        // not a missing key.
        let client = resources.stores.load().client("test").unwrap();
        let mut raw = client.conn().await.unwrap();
        let _: i64 = redis::cmd("LPUSH")
            .arg(&redis_key)
            .arg("x")
            .query_async(&mut raw)
            .await
            .unwrap();

        let get = StoreGetPlugin::from_config(
            &cfg(serde_json::json!({ "store": "test", "key": key, "name": "v" })),
            &resources,
        )
        .unwrap();
        let err = get.execute(test_ctx()).await.unwrap_err();
        assert_eq!(err.error.code, "STORE_ERROR");
    }
}
