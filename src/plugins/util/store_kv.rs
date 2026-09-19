//! Shared plumbing for the `store-*` nodes.
//!
//! Store resolution happens once, at policy-compile time: the node holds the
//! resulting handle and nothing reads the registry on the request path. This is
//! the pattern `limit-count` established (`src/plugins/native/limit_count.rs`).

use std::collections::HashMap;
use std::sync::Arc;

use bytes::Bytes;
use serde_json::Value;

use crate::context::{Context, GatewayError};
use crate::plugins::resources::PluginResources;
use crate::plugins::PluginExecutionError;
use crate::vars::template::Template;

/// A resolved store, held by a node for its lifetime.
///
/// `RedisStoreClient` carries its own hand-written `Debug` impl (its
/// connection manager has none), so `Arc<RedisStoreClient>` is `Debug` too and
/// this can derive rather than hand-write. The four `store-*` plugins that
/// hold one still can't derive their own `Debug`: their other fields are only
/// read inside the `#[cfg(feature = "redis-store")]` `execute` body, and a
/// headless build has no such body to read them, so a derived impl (which
/// the dead-code pass ignores) would leave them looking unused there.
#[derive(Debug)]
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

/// Prepares the response common to every store-node error before it exits
/// the node's `error` port: status + JSON body + `content-type`, the same
/// shape `limit-count` and the session plugins already use (e.g.
/// `authz_casdoor::store_error`, `limit_count.rs`'s `RATE_LIMIT_UNAVAILABLE`
/// branch).
///
/// Without this, a policy that wires `error -> client` -- structurally the
/// only alternative to leaving the port unwired, and the port model nudges
/// every port toward being wired -- would answer with whatever
/// `Context::new` left in `ctx.response` (`status_code == 0`, mapped to `200`
/// by the listener) instead of a real failure status. That is exactly the
/// fail-open the design's §7 rules out.
fn prepare_error_response(ctx: &mut Context, status: u16, code: &str, message: &str) {
    ctx.response.status_code = status;
    ctx.response.body =
        Bytes::from(serde_json::json!({ "error": code, "message": message }).to_string());
    ctx.response.headers.insert(
        "content-type".to_string(),
        vec!["application/json".to_string()],
    );
}

/// A `key` template that rendered to nothing.
///
/// `{prefix}:kv:` is still inside the namespace, so this is not a collision --
/// but it would put every request on one shared key, turning a template typo
/// into a cross-tenant leak. It fails loudly instead, with a `500`: this is a
/// policy-authoring/config fault, not an outage.
///
/// Only ever checked on the redis-backed data path (rendering a key is
/// pointless without a backend to read or write it against), so this is
/// `#[cfg(feature = "redis-store")]` like the rest of that path.
#[cfg(feature = "redis-store")]
pub fn key_invalid(
    mut ctx: Context,
    node_type: &str,
    op: &str,
    store: &str,
) -> PluginExecutionError {
    let message = format!(
        "{}: the 'key' template rendered to an empty string",
        node_type
    );
    prepare_error_response(&mut ctx, 500, "STORE_KEY_INVALID", &message);
    let mut metadata = HashMap::new();
    metadata.insert("store".to_string(), Value::String(store.to_string()));
    metadata.insert("op".to_string(), Value::String(op.to_string()));
    PluginExecutionError {
        context: ctx,
        error: GatewayError {
            node_id: String::new(),
            code: "STORE_KEY_INVALID".to_string(),
            message,
            metadata,
        },
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
    optional_seconds(config, "ttl_seconds", node_type)
}

/// Parses an optional positive duration in seconds.
///
/// Absent means "no duration"; `0` is rejected rather than read as "none",
/// because the two are too easy to confuse and omitting the field is how you
/// say it. Pure config parsing with no store dependency, so unlike `resolve`
/// this needs no `redis-store`-gated pair.
pub fn optional_seconds(
    config: &HashMap<String, Value>,
    field: &str,
    node_type: &str,
) -> Result<Option<u64>, String> {
    match config.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => {
            let n = v.as_u64().ok_or_else(|| {
                format!("{}: '{}' must be a non-negative integer", node_type, field)
            })?;
            if n == 0 {
                return Err(format!(
                    "{}: '{}' must be greater than 0; omit the field to leave it unset",
                    node_type, field
                ));
            }
            Ok(Some(n))
        }
    }
}

/// A store outage. Never conflated with a miss: see the spec's §7. Prepares a
/// `503` -- the dependency is unavailable, not the caller's fault -- so a
/// policy that wires `error -> client` cannot fail open with a `200`.
///
/// Not feature-gated: the `#[cfg(not(feature = "redis-store"))]` `execute`
/// bodies in all four plugins call this too, to report "built without the
/// redis-store feature" with the same shape.
pub fn store_error(
    mut ctx: Context,
    node_type: &str,
    op: &str,
    store: &str,
    msg: String,
) -> PluginExecutionError {
    let message = format!("{}: {} failed: {}", node_type, op, msg);
    prepare_error_response(&mut ctx, 503, "STORE_ERROR", &message);
    let mut metadata = HashMap::new();
    metadata.insert("store".to_string(), Value::String(store.to_string()));
    metadata.insert("op".to_string(), Value::String(op.to_string()));
    PluginExecutionError {
        context: ctx,
        error: GatewayError {
            node_id: String::new(),
            code: "STORE_ERROR".to_string(),
            message,
            metadata,
        },
    }
}

/// A value that exists but is not usable as configured (bad JSON, non-numeric).
/// Prepares a `500`: a data/config fault, not an outage.
///
/// Only ever raised on the redis-backed data path -- see [`key_invalid`].
#[cfg(feature = "redis-store")]
pub fn value_invalid(
    mut ctx: Context,
    node_type: &str,
    op: &str,
    store: &str,
    msg: String,
) -> PluginExecutionError {
    let message = format!("{}: {}", node_type, msg);
    prepare_error_response(&mut ctx, 500, "STORE_VALUE_INVALID", &message);
    let mut metadata = HashMap::new();
    metadata.insert("store".to_string(), Value::String(store.to_string()));
    metadata.insert("op".to_string(), Value::String(op.to_string()));
    PluginExecutionError {
        context: ctx,
        error: GatewayError {
            node_id: String::new(),
            code: "STORE_VALUE_INVALID".to_string(),
            message,
            metadata,
        },
    }
}

/// True when a redis error reflects a value/type problem rather than an
/// outage: a command applied to a key holding the wrong type (`WRONGTYPE`,
/// surfaced by the crate as an `ExtensionError` with that code), or a numeric
/// operation against a non-numeric value (`ERR value is not an integer or
/// out of range`). Both are data faults `store-incr` reports as
/// `STORE_VALUE_INVALID`, never `STORE_ERROR`.
///
/// Classifying on `code()`/`kind()` rather than matching the whole message
/// avoids depending on redis wrapping the message text a particular way; the
/// substring check is kept only for the one case (`ResponseError`'s "not an
/// integer" wording) that has no dedicated code of its own.
#[cfg(feature = "redis-store")]
pub fn is_value_type_error(e: &redis::RedisError) -> bool {
    e.code() == Some("WRONGTYPE")
        || (e.kind() == redis::ErrorKind::ResponseError && e.to_string().contains("not an integer"))
}

#[cfg(all(test, feature = "redis-store"))]
mod tests {
    use super::*;
    use crate::context::{GatewayRequest, Protocol};
    use std::collections::HashMap;

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
    /// template typo becomes a cross-tenant leak. Fail loudly instead, with a
    /// `500` (a config/authoring fault, not an outage) and a non-empty body.
    #[test]
    fn test_empty_key_has_its_own_code() {
        let e = key_invalid(test_ctx(), "store-get", "GET", "sessions");
        assert_eq!(e.error.code, "STORE_KEY_INVALID");
        assert_eq!(e.context.response.status_code, 500);
        assert!(!e.context.response.body.is_empty());
        assert_eq!(e.error.metadata.get("store").unwrap(), "sessions");
        assert_eq!(e.error.metadata.get("op").unwrap(), "GET");
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
    /// the metadata carries enough to debug it. It must also leave the
    /// response a real `503` with a non-empty body -- never the `200` an
    /// untouched `Context::new` would answer with if a policy wires
    /// `error -> client` -- which is the failure this whole fix round exists
    /// to close (spec §7: never fail open).
    #[test]
    fn test_store_error_prepares_a_503_and_carries_code_store_and_op() {
        let e = store_error(
            test_ctx(),
            "store-get",
            "GET",
            "sessions",
            "connection refused".to_string(),
        );
        assert_eq!(e.error.code, "STORE_ERROR");
        assert_eq!(e.error.metadata.get("store").unwrap(), "sessions");
        assert_eq!(e.error.metadata.get("op").unwrap(), "GET");
        assert!(
            e.error.message.contains("connection refused"),
            "{}",
            e.error.message
        );
        assert_eq!(e.context.response.status_code, 503);
        assert!(!e.context.response.body.is_empty());
        assert_eq!(
            e.context.response.headers.get("content-type").unwrap(),
            &vec!["application/json".to_string()]
        );
    }

    /// Same as above for `value_invalid`, whose status is `500`: a value/JSON
    /// fault is a data problem, not an outage.
    #[test]
    fn test_value_invalid_prepares_a_500_and_carries_code_store_and_op() {
        let e = value_invalid(
            test_ctx(),
            "store-get",
            "GET",
            "sessions",
            "expected JSON".to_string(),
        );
        assert_eq!(e.error.code, "STORE_VALUE_INVALID");
        assert_eq!(e.error.metadata.get("store").unwrap(), "sessions");
        assert_eq!(e.error.metadata.get("op").unwrap(), "GET");
        assert_eq!(e.context.response.status_code, 500);
        assert!(!e.context.response.body.is_empty());
    }

    /// `WRONGTYPE` (a key holding a list/hash) and the "not an integer"
    /// message (a key holding a non-numeric string) must both classify as a
    /// value-type error, not an outage -- see `store-incr`'s docs and the
    /// spec's §5.3.
    #[test]
    fn test_is_value_type_error_covers_wrongtype_and_not_an_integer() {
        // `make_extension_error` is how the redis crate itself builds an
        // error whose `code()` is a raw RESP error code it does not have a
        // dedicated `ErrorKind` for -- exactly WRONGTYPE's situation.
        let wrongtype = redis::make_extension_error(
            "WRONGTYPE".to_string(),
            Some("Operation against a key holding the wrong kind of value".to_string()),
        );
        assert!(is_value_type_error(&wrongtype));

        let not_an_integer = redis::RedisError::from((
            redis::ErrorKind::ResponseError,
            "value is not an integer or out of range",
        ));
        assert!(is_value_type_error(&not_an_integer));

        let outage = redis::RedisError::from((redis::ErrorKind::IoError, "connection refused"));
        assert!(!is_value_type_error(&outage));
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

    /// Every key this module writes -- through a plugin or directly via a raw
    /// redis command -- gets this TTL, so a panicking or failing test still
    /// self-cleans rather than leaking a permanent key onto a long-lived
    /// redis. 60s is ample for these tests to run and observe the key.
    const LIVE_TEST_TTL_SECONDS: u64 = 60;

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
            &cfg(serde_json::json!({ "store": "test", "key": key, "value": "1", "ttl_seconds": LIVE_TEST_TTL_SECONDS })),
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
                "ttl_seconds": LIVE_TEST_TTL_SECONDS,
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
            &cfg(serde_json::json!({ "store": "test", "key": key, "value": "not json", "ttl_seconds": LIVE_TEST_TTL_SECONDS })),
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
                "ttl_seconds": LIVE_TEST_TTL_SECONDS,
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
        // Written directly (bypassing store-set), so it needs its own expiry
        // to self-clean -- see `LIVE_TEST_TTL_SECONDS`.
        let _: bool = redis::cmd("EXPIRE")
            .arg(&redis_key)
            .arg(LIVE_TEST_TTL_SECONDS)
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

    /// `store-incr` against a key holding a non-numeric string: the docs and
    /// the spec both promise `STORE_VALUE_INVALID` for this, and it is the
    /// case the substring match on "not an integer" was originally written
    /// for -- see F6 in the final review.
    #[tokio::test]
    async fn test_incr_on_a_non_numeric_string_exits_value_invalid() {
        let Some(url) = store_url() else {
            eprintln!(
                "skipping test_incr_on_a_non_numeric_string_exits_value_invalid: FEATHERBIT_TEST_REDIS_URL not set"
            );
            return;
        };
        let resources = resources_with_store(&url);
        let key = unique_key("incr-non-numeric");

        let set = StoreSetPlugin::from_config(
            &cfg(serde_json::json!({ "store": "test", "key": key, "value": "not-a-number", "ttl_seconds": LIVE_TEST_TTL_SECONDS })),
            &resources,
        )
        .unwrap();
        set.execute(test_ctx()).await.unwrap();

        let incr = StoreIncrPlugin::from_config(
            &cfg(serde_json::json!({ "store": "test", "key": key, "name": "n" })),
            &resources,
        )
        .unwrap();
        let err = incr.execute(test_ctx()).await.unwrap_err();
        assert_eq!(err.error.code, "STORE_VALUE_INVALID");
    }

    /// `store-incr` against a key holding a list (`WRONGTYPE`): the substring
    /// match on "not an integer" missed this entirely, silently reporting a
    /// pure data fault as `STORE_ERROR` -- an outage the caller did not have.
    /// See F6 in the final review.
    #[tokio::test]
    async fn test_incr_on_a_list_value_exits_value_invalid_not_store_error() {
        let Some(url) = store_url() else {
            eprintln!(
                "skipping test_incr_on_a_list_value_exits_value_invalid_not_store_error: FEATHERBIT_TEST_REDIS_URL not set"
            );
            return;
        };
        let resources = resources_with_store(&url);
        let key = unique_key("incr-wrongtype");
        let redis_key = namespaced_key("fb", &key);

        let client = resources.stores.load().client("test").unwrap();
        let mut raw = client.conn().await.unwrap();
        let _: i64 = redis::cmd("LPUSH")
            .arg(&redis_key)
            .arg("x")
            .query_async(&mut raw)
            .await
            .unwrap();
        let _: bool = redis::cmd("EXPIRE")
            .arg(&redis_key)
            .arg(LIVE_TEST_TTL_SECONDS)
            .query_async(&mut raw)
            .await
            .unwrap();

        let incr = StoreIncrPlugin::from_config(
            &cfg(serde_json::json!({ "store": "test", "key": key, "name": "n" })),
            &resources,
        )
        .unwrap();
        let err = incr.execute(test_ctx()).await.unwrap_err();
        assert_eq!(err.error.code, "STORE_VALUE_INVALID");
    }

    /// The empty-key guard, actually executed rather than only asserted on
    /// the constructor's constant (F7 in the final review): a template that
    /// renders to nothing must exit `STORE_KEY_INVALID` with a `500`, not
    /// silently operate on `{prefix}:kv:`.
    #[tokio::test]
    async fn test_empty_rendered_key_exits_store_key_invalid() {
        let Some(url) = store_url() else {
            eprintln!(
                "skipping test_empty_rendered_key_exits_store_key_invalid: FEATHERBIT_TEST_REDIS_URL not set"
            );
            return;
        };
        let resources = resources_with_store(&url);

        let get = StoreGetPlugin::from_config(
            &cfg(serde_json::json!({ "store": "test", "key": "{{request.headers.x-absent}}", "name": "v" })),
            &resources,
        )
        .unwrap();
        let err = get.execute(test_ctx()).await.unwrap_err();
        assert_eq!(err.error.code, "STORE_KEY_INVALID");
        assert_eq!(err.context.response.status_code, 500);
        assert!(!err.context.response.body.is_empty());
    }

    /// The one behaviour `reads_response_body()` exists for: a `key`/`value`
    /// referencing the response body must force the route onto the buffered
    /// path. `PluginResources::empty()` (used by every unit test in the four
    /// plugins) has no declared stores, so `from_config` cannot succeed there
    /// and the four unit tests could only assert `Template::
    /// references_response_body()` on the side -- which cannot fail even if
    /// `reads_response_body()` itself is hardcoded wrong (F3/F4 in the final
    /// review). This constructs the real plugins against a declared store
    /// and asserts the trait method directly.
    #[tokio::test]
    async fn test_reads_response_body_reflects_a_response_body_reference() {
        let Some(url) = store_url() else {
            eprintln!(
                "skipping test_reads_response_body_reflects_a_response_body_reference: FEATHERBIT_TEST_REDIS_URL not set"
            );
            return;
        };
        let resources = resources_with_store(&url);

        let get_plain = StoreGetPlugin::from_config(
            &cfg(serde_json::json!({ "store": "test", "key": "k", "name": "n" })),
            &resources,
        )
        .unwrap();
        let get_reads = StoreGetPlugin::from_config(
            &cfg(serde_json::json!({ "store": "test", "key": "{{response.body}}", "name": "n" })),
            &resources,
        )
        .unwrap();
        assert!(!get_plain.reads_response_body());
        assert!(get_reads.reads_response_body());

        let set_plain = StoreSetPlugin::from_config(
            &cfg(serde_json::json!({ "store": "test", "key": "k", "value": "1" })),
            &resources,
        )
        .unwrap();
        let set_reads = StoreSetPlugin::from_config(
            &cfg(serde_json::json!({ "store": "test", "key": "k", "value": "{{response.body}}" })),
            &resources,
        )
        .unwrap();
        assert!(!set_plain.reads_response_body());
        assert!(set_reads.reads_response_body());

        let delete_plain = StoreDeletePlugin::from_config(
            &cfg(serde_json::json!({ "store": "test", "key": "k" })),
            &resources,
        )
        .unwrap();
        let delete_reads = StoreDeletePlugin::from_config(
            &cfg(serde_json::json!({ "store": "test", "key": "{{response.body}}" })),
            &resources,
        )
        .unwrap();
        assert!(!delete_plain.reads_response_body());
        assert!(delete_reads.reads_response_body());

        let incr_plain = StoreIncrPlugin::from_config(
            &cfg(serde_json::json!({ "store": "test", "key": "k", "name": "n" })),
            &resources,
        )
        .unwrap();
        let incr_reads = StoreIncrPlugin::from_config(
            &cfg(serde_json::json!({ "store": "test", "key": "{{response.body}}", "name": "n" })),
            &resources,
        )
        .unwrap();
        assert!(!incr_plain.reads_response_body());
        assert!(incr_reads.reads_response_body());
    }

    /// The mirror of `test_incr_does_not_refresh_the_ttl`: with
    /// `refresh_ttl: true` the expiry is pushed back out on every increment,
    /// giving "N events within `ttl_seconds` of each other" instead of
    /// "N events since the first one".
    ///
    /// The assertion is the inverse of the guarded case, and deliberately as
    /// strong: a *rise* of nearly the whole sleep, not merely `ttl2 > ttl1`,
    /// which round-trip jitter alone could satisfy.
    #[tokio::test]
    async fn test_incr_with_refresh_ttl_extends_the_expiry() {
        let Some(url) = store_url() else {
            eprintln!(
                "skipping test_incr_with_refresh_ttl_extends_the_expiry: FEATHERBIT_TEST_REDIS_URL not set"
            );
            return;
        };
        let resources = resources_with_store(&url);
        let key = unique_key("ttl-slide");
        let redis_key = namespaced_key("fb", &key);

        let client = resources.stores.load().client("test").unwrap();
        let mut raw = client.conn().await.unwrap();

        let incr = StoreIncrPlugin::from_config(
            &cfg(serde_json::json!({
                "store": "test",
                "key": key,
                "name": "n",
                "ttl_seconds": LIVE_TEST_TTL_SECONDS,
                "refresh_ttl": true,
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

        // Both samples are taken immediately after an increment, so a
        // refreshing script leaves them roughly EQUAL -- it is the *absence*
        // of the countdown that proves the refresh, not a rise. The guarded
        // (create-only) script would show a drop tracking the ~1100ms sleep,
        // exactly as `test_incr_does_not_refresh_the_ttl` asserts, so this
        // bound of 200ms fails against it while tolerating round-trip jitter.
        let drop = ttl1 - ttl2;
        assert!(
            drop <= 200,
            "refresh_ttl must re-arm the expiry on each increment, so it must not count down: ttl1={ttl1} ttl2={ttl2} drop={drop}"
        );
    }

    /// `extend_ttl_seconds` makes a read push the key's expiry out, so an
    /// entry stays alive while it is being used and disappears a fixed time
    /// after the last access.
    #[tokio::test]
    async fn test_get_with_extend_ttl_pushes_the_expiry_out() {
        let Some(url) = store_url() else {
            eprintln!(
                "skipping test_get_with_extend_ttl_pushes_the_expiry_out: FEATHERBIT_TEST_REDIS_URL not set"
            );
            return;
        };
        let resources = resources_with_store(&url);
        let key = unique_key("touch");
        let redis_key = namespaced_key("fb", &key);

        let client = resources.stores.load().client("test").unwrap();
        let mut raw = client.conn().await.unwrap();

        let set = StoreSetPlugin::from_config(
            &cfg(serde_json::json!({
                "store": "test",
                "key": key,
                "value": "alive",
                "ttl_seconds": LIVE_TEST_TTL_SECONDS,
            })),
            &resources,
        )
        .unwrap();
        set.execute(test_ctx()).await.unwrap();

        let ttl1: i64 = redis::cmd("PTTL")
            .arg(&redis_key)
            .query_async(&mut raw)
            .await
            .unwrap();

        tokio::time::sleep(Duration::from_millis(1100)).await;

        let get = StoreGetPlugin::from_config(
            &cfg(serde_json::json!({
                "store": "test",
                "key": key,
                "name": "v",
                "extend_ttl_seconds": LIVE_TEST_TTL_SECONDS,
            })),
            &resources,
        )
        .unwrap();
        let out = get.execute(test_ctx()).await.unwrap();

        // The read still returns the value -- extending must not replace GET's job.
        assert_eq!(
            out.context.message.get("v").and_then(|v| v.as_str()),
            Some("alive")
        );

        let ttl2: i64 = redis::cmd("PTTL")
            .arg(&redis_key)
            .query_async(&mut raw)
            .await
            .unwrap();

        // Same shape as the refresh_ttl assertion above: ttl1 is sampled just
        // after the write and ttl2 just after the extending read, so a working
        // GETEX leaves them roughly equal. A plain GET would let the TTL count
        // down by the ~1100ms sleep, which this bound rejects.
        let drop = ttl1 - ttl2;
        assert!(
            drop <= 200,
            "a read with extend_ttl_seconds must re-arm the expiry, so it must not count down: ttl1={ttl1} ttl2={ttl2} drop={drop}"
        );
    }

    /// Extending must not conjure a key. `GETEX` on a missing key returns nil
    /// and creates nothing, so the node still exits `miss` and the store is
    /// left untouched -- otherwise a keep-alive read would manufacture the
    /// very entries it is meant to keep warm.
    #[tokio::test]
    async fn test_get_with_extend_ttl_on_an_absent_key_still_misses() {
        let Some(url) = store_url() else {
            eprintln!(
                "skipping test_get_with_extend_ttl_on_an_absent_key_still_misses: FEATHERBIT_TEST_REDIS_URL not set"
            );
            return;
        };
        let resources = resources_with_store(&url);
        let key = unique_key("touch-absent");
        let redis_key = namespaced_key("fb", &key);

        let client = resources.stores.load().client("test").unwrap();
        let mut raw = client.conn().await.unwrap();

        let get = StoreGetPlugin::from_config(
            &cfg(serde_json::json!({
                "store": "test",
                "key": key,
                "name": "v",
                "extend_ttl_seconds": LIVE_TEST_TTL_SECONDS,
            })),
            &resources,
        )
        .unwrap();

        let out = get.execute(test_ctx()).await.unwrap();
        assert_eq!(out.port, Some("miss"));

        let exists: i64 = redis::cmd("EXISTS")
            .arg(&redis_key)
            .query_async(&mut raw)
            .await
            .unwrap();
        assert_eq!(exists, 0, "a missing key must not be created by extending");
    }
}
