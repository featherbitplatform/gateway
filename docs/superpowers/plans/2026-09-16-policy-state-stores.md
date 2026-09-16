# Policy State Stores Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give policies read/write access to the redis/valkey stores that already back `limit-count`, server-side sessions and ACME, through four new node types: `store-get`, `store-set`, `store-incr`, `store-delete`.

**Architecture:** One shared helper in `src/plugins/util/store_kv.rs` owns store resolution, key namespacing, and error mapping; four thin plugin files use it. Nothing new is added to the stores subsystem itself — the nodes resolve a store by name at construction and hold the `Arc`, exactly as `limit-count` does. `store-get` declares a `miss` outcome port; the other three do not.

**Tech Stack:** Rust, `redis` 0.32 (`AsyncCommands` + `Script`), `async_trait`, `serde_json`; React/TypeScript for the editor palette; Docusaurus for the docs pages.

**Spec:** `docs/superpowers/specs/2026-09-16-policy-state-stores-design.md`

## Global Constraints

- **Node names, verbatim:** `store-get`, `store-set`, `store-incr`, `store-delete`.
- **Error codes, verbatim:** `STORE_ERROR` (store outage), `STORE_VALUE_INVALID` (unparseable/non-numeric value).
- **Key namespace:** every key is `format!("{}:kv:{}", client.key_prefix(), rendered_key)`, and the `kv` literal comes from `stores::namespaces::POLICY_KV`, never a hand-written string.
- **Error codes, verbatim:** `STORE_ERROR`, `STORE_VALUE_INVALID`, `STORE_KEY_INVALID` (the `key` template rendered empty).
- **A store outage never fails open** and never looks like a miss: it exits the `error` port with a 503 prepared. This is the rule the five session plugins follow.
- **`store-incr` TTL applies at creation only**, never refreshed on later increments.
- **`ttl_seconds: 0` is a config error**, not "no expiry". Omitting the key means no expiry.
- **`redis` is optional** (`redis-store` cargo feature, default-on). Every plugin must compile in a `--no-default-features` build; without the feature `from_config` returns `Err` naming the missing feature, mirroring `StoreRegistry::counter_store`'s non-feature arm (`src/stores/mod.rs:194`).
- **Lint with CI's real command, not a weaker one:** `cargo clippy --all-targets --locked -- -D warnings` AND `cargo clippy --all-targets --no-default-features --locked -- -D warnings` (`.github/workflows/ci.yml:54,154`). Plain `cargo clippy --all-targets` exits 0 on unused items and hides a failure CI will catch. Never silence one with `#[allow(dead_code)]`: an item nothing uses either belongs in the task that uses it, or is test-only and takes `#[cfg(test)]`.
- **No `src/lib.rs`:** this is a binary crate, so `dead_code` reachability roots at `fn main`. Anything without a production caller fails `-D warnings` no matter how it is gated. Code lands in the same commit as its first caller.
- **Everything redis-backed is behind `redis-store`,** including `stores::counter`, `sessions::redis` and `acme::storage::redis`. Code that references them — or constants that only make sense alongside them — must be gated too, or the headless build breaks.
- **Run the full `cargo test`,** never a filtered run: `test_catalog_covers_factory`, `test_every_catalog_plugin_has_an_icon`, `test_every_catalog_plugin_is_in_a_palette_category`, `test_every_catalog_plugin_has_a_docs_page` and `test_every_plugin_docs_page_is_in_the_sidebar` are what keep a node from being invisible to the UI and to MCP.
- **Also run `cargo test --release`.** `debug_assert!` compiles out in release and the e2e suite runs the release binary; a debug-only green has bitten this repo before.
- **Commit style:** Conventional Commits. No `Co-Authored-By` trailer, no AI attribution.
- **Branch:** all work on `feature/policy-state-stores`, branched from `develop`.

## File Structure

| File | Responsibility |
|---|---|
| `src/plugins/util/store_kv.rs` (create) | Store resolution, namespaced key building, `redis::RedisError` → `GatewayError` mapping, the increment script. Shared by all four nodes. |
| `src/plugins/util/mod.rs` (modify) | `pub mod store_kv;` |
| `src/plugins/native/store_get.rs` (create) | The read node, including `miss` and JSON flattening. |
| `src/plugins/native/store_set.rs` (create) | The write node. |
| `src/plugins/native/store_incr.rs` (create) | The atomic counter node. |
| `src/plugins/native/store_delete.rs` (create) | The idempotent delete node. |
| `src/plugins/native/mod.rs` (modify) | Four `pub mod` lines. |
| `src/plugins/mod.rs` (modify) | `KNOWN_PLUGIN_TYPES`, `create_plugin` arms, `port_spec` arms. |
| `src/plugins/ports.rs` (modify) | `STORE_GET_SPEC` (with `miss`) and `STORE_WRITE_SPEC` (shared by set/incr/delete). |
| `src/admin/policies.rs` (modify) | Four `CATALOG` rows. |
| `ui/src/pluginCategories.ts`, `ui/src/pluginMeta.tsx`, `ui/src/pluginConfig.ts` (modify) | Palette group, colour+icon, config forms with the `optionsFrom: 'stores'` picker. |
| `website/docs/reference/plugins/store-{get,set,incr,delete}.md` (create) | The pages `get_node_type` hands an agent. |
| `website/sidebars.ts`, `website/docs/reference/plugins/index.md`, `docs/apisix-parity.md` (modify) | Navigation and parity rows. |

---

### Task 0: Namespace registry and separation guarantees

**Files:**
- Create: `src/stores/namespaces.rs`
- Modify: `src/stores/mod.rs`, `src/stores/counter.rs`, `src/acme/storage/redis.rs`, `src/sessions/redis.rs`

**Interfaces:**
- Produces, used by Task 1: `stores::namespaces::{COUNTERS, ACME, SESSIONS, POLICY_KV, MANAGED}`.

featherbit writes keys from exactly three places (`cnt`, `acme`, `sess`); the only other redis
calls in the codebase are `PING`/`INFO`, which touch no keys. `kv` is free. This task makes it
**stay** free, and proves a policy key cannot name a managed one.

- [ ] **Step 1: Write the failing tests**

Create `src/stores/namespaces.rs` with only this test module:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_all_namespaces_are_distinct() {
        let all = [COUNTERS, ACME, SESSIONS];
        for (i, a) in all.iter().enumerate() {
            for b in all.iter().skip(i + 1) {
                assert_ne!(a, b, "namespaces must be pairwise distinct");
            }
        }
    }

    /// The drift guard: each subsystem's real key builder must still produce
    /// keys under its declared namespace. A subsystem that changes its prefix,
    /// or a new one that reuses `kv`, fails here.
    #[test]
    fn test_key_builders_stay_inside_their_declared_namespace() {
        let sess = crate::sessions::redis::sess_key("fb", "abc");
        assert!(sess.starts_with(&format!("fb:{}:", SESSIONS)), "{sess}");

        let acme = crate::acme::storage::redis::account_key("fb");
        assert!(acme.starts_with(&format!("fb:{}:", ACME)), "{acme}");

        let cnt = crate::stores::counter::window_key("fb", 7, "u1");
        assert!(cnt.starts_with(&format!("fb:{}:", COUNTERS)), "{cnt}");
    }
}
```

> `window_key` does not exist yet: `counter.rs:86` builds its key inline as
> `format!("{}:cnt:{}:{}", prefix, slot, key)`. Extract it as
> `pub(crate) fn window_key(prefix: &str, slot: u64, key: &str) -> String` and call it from
> `incr_fixed_window`, so the builder is testable. Widen `sess_key` and `account_key` to
> `pub(crate)` for the same reason. That visibility change and the extraction are the only
> changes to existing behavior in this task.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --no-run 2>&1 | grep "^error"`
Expected: `cannot find value 'COUNTERS' in this scope`, `cannot find function 'window_key'`.

- [ ] **Step 3: Write the implementation**

Put this above the test module in `src/stores/namespaces.rs`:

```rust
//! The key namespaces featherbit writes under a store's `key_prefix`.
//!
//! Every key is `{key_prefix}:{namespace}:...`. These must stay disjoint: a
//! policy-written key must never be able to name a key a managed subsystem
//! reads or writes, in either direction. Declaring them in one place -- and
//! testing the real builders against it -- is what keeps that true as
//! subsystems are added.

/// Rate-limit counters (`src/stores/counter.rs`).
pub const COUNTERS: &str = "cnt";
/// ACME account, certificate, challenge and lease state (`src/acme/storage/redis.rs`).
pub const ACME: &str = "acme";
/// Server-side sessions (`src/sessions/redis.rs`).
pub const SESSIONS: &str = "sess";
```

`POLICY_KV` and `MANAGED` are deliberately **not** declared here — they arrive in Task 1
alongside the code that uses them. A constant nothing references is a `-D warnings` failure,
and `#[allow(dead_code)]` is not an option (see Global Constraints).

Add this to `src/stores/mod.rs` — gated, because every subsystem this module describes is
gated, and a headless build has no keys to namespace:

```rust
#[cfg(feature = "redis-store")]
pub mod namespaces;
```

Extract `window_key` in `counter.rs`:

```rust
/// The key for one fixed window of one counter.
pub(crate) fn window_key(prefix: &str, slot: u64, key: &str) -> String {
    format!("{}:{}:{}:{}", prefix, super::namespaces::COUNTERS, slot, key)
}
```

and call it from `incr_fixed_window` in place of the inline `format!`. Change the ACME and
session builders the same way — interpolating the constant instead of a string literal, e.g.
`format!("{prefix}:{}:account", crate::stores::namespaces::ACME)`.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test namespaces`
Expected: 2 passed.

Then run the full `cargo test`. The refactor touches three subsystems, and the existing
session, ACME and counter tests are the real check that every key is byte-identical to before
 -- a changed key would silently orphan live sessions and certificates on upgrade.

- [ ] **Step 5: Commit**

```bash
git add src/stores/namespaces.rs src/stores/mod.rs src/stores/counter.rs src/acme/storage/redis.rs src/sessions/redis.rs
git commit -m "refactor(stores): declare key namespaces in one place

featherbit writes keys from three subsystems (cnt, acme, sess), each
building its prefix from a string literal. Nothing stopped a fourth from
picking a name already in use, and nothing proved they were disjoint.

Declare all four namespaces -- including kv for the upcoming store-*
nodes -- in one module, and test each subsystem's real key builder
against it, so a subsystem that changes its prefix or reuses another's
fails the build rather than colliding in production.

Keys are byte-identical; the existing session, ACME and counter tests are
what prove it, since a changed key would orphan live sessions and
certificates on upgrade."
```

---

### Task 1: Shared store-kv helper

> **Committed together with Task 2, not on its own.** This crate has no
> `src/lib.rs`, so `dead_code` reachability roots at `fn main`: a helper whose
> callers do not exist yet is unreachable, and every symbol here fails
> `-D warnings` until `store-get` lands. Do the work as written, run the tests,
> but make one commit covering Tasks 1 and 2 together. Same principle as a
> constant landing with its first use, at module scale.


**Files:**
- Create: `src/plugins/util/store_kv.rs`
- Modify: `src/plugins/util/mod.rs`

**Interfaces:**
- Consumes: `stores::namespaces::{COUNTERS, ACME, SESSIONS}` (Task 0, gated on `redis-store`); `crate::stores::StoreRegistry::client(&str) -> Result<Arc<RedisStoreClient>, String>`;
- **Also adds to Task 0's module** (they land with their first use, so nothing is dead):

```rust
// src/stores/namespaces.rs
/// Policy-written keys: the `store-get`/`store-set`/`store-incr`/`store-delete` nodes.
pub const POLICY_KV: &str = "kv";

/// Namespaces owned by featherbit itself. A policy can never address these,
/// because every `store-*` key is prefixed with [`POLICY_KV`].
///
/// Test-only by design: nothing in production consults this list. It exists so
/// the disjointness it describes is asserted rather than assumed.
#[cfg(test)]
pub const MANAGED: &[&str] = &[COUNTERS, ACME, SESSIONS];
```

plus the test that moved out of Task 0 for the same reason:

```rust
    /// A policy must never be able to address a namespace featherbit owns.
    #[test]
    fn test_policy_namespace_is_not_managed() {
        assert!(!MANAGED.contains(&POLICY_KV));
    }
```

- **Feature gating:** `namespaced_key` and its tests reference `namespaces`, which only exists
  with `redis-store`. Gate `namespaced_key` with `#[cfg(feature = "redis-store")]` and the
  `store_kv` test module with `#[cfg(all(test, feature = "redis-store"))]`. The rest of the
  helper (`required_template`, `optional_ttl`, `store_error`, `value_invalid`, `key_invalid`)
  stays ungated, since the plugins use it in both builds. `crate::plugins::resources::PluginResources.stores: ArcSwap<StoreRegistry>`.
- Produces, used by Tasks 2–5:
  - `pub struct StoreHandle` with `pub async fn conn(&self) -> Result<redis::aio::ConnectionManager, String>` and `pub fn key_for(&self, rendered: &str) -> String`
  - `pub fn resolve(config: &HashMap<String, Value>, resources: &Arc<PluginResources>, node_type: &str) -> Result<StoreHandle, String>`
  - `pub fn required_template(config: &HashMap<String, Value>, key: &str, node_type: &str) -> Result<Template, String>`
  - `pub fn optional_ttl(config: &HashMap<String, Value>, node_type: &str) -> Result<Option<u64>, String>`
  - `pub fn store_error(node_type: &str, op: &str, store: &str, msg: String) -> GatewayError`
  - `pub fn key_invalid(node_type: &str) -> GatewayError`
  - `pub fn value_invalid(node_type: &str, msg: String) -> GatewayError`

- [ ] **Step 1: Write the failing tests**

Create `src/plugins/util/store_kv.rs` containing only the test module for now:

```rust
#[cfg(test)]
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
            assert!(built.starts_with("fb:kv:"), "escaped the namespace: {built}");
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
        assert!(err.contains("store-get"), "error must name the node type: {err}");
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
        let err = optional_ttl(&cfg(serde_json::json!({ "ttl_seconds": 0 })), "store-set")
            .unwrap_err();
        assert!(err.contains("ttl_seconds"), "{err}");
    }

    #[test]
    fn test_optional_ttl_absent_is_none_and_present_is_some() {
        assert_eq!(optional_ttl(&cfg(serde_json::json!({})), "store-set").unwrap(), None);
        assert_eq!(
            optional_ttl(&cfg(serde_json::json!({ "ttl_seconds": 300 })), "store-set").unwrap(),
            Some(300)
        );
    }

    /// A store outage must be identifiable downstream, so the code is fixed and
    /// the metadata carries enough to debug it.
    #[test]
    fn test_store_error_carries_code_store_and_op() {
        let e = store_error("store-get", "GET", "sessions", "connection refused".to_string());
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
```

- [ ] **Step 2: Register the module and run the tests to verify they fail**

Add to `src/plugins/util/mod.rs`:

```rust
pub mod store_kv;
```

Run: `cargo test --no-run 2>&1 | grep "^error"`
Expected: compile errors — `cannot find function 'namespaced_key'`, `'required_template'`, `'optional_ttl'`, `'store_error'`, `'value_invalid'` in this scope.

- [ ] **Step 3: Write the implementation**

Put this **above** the test module in `src/plugins/util/store_kv.rs`:

```rust
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
pub fn key_invalid(node_type: &str) -> GatewayError {
    GatewayError {
        node_id: String::new(),
        code: "STORE_KEY_INVALID".to_string(),
        message: format!("{}: the 'key' template rendered to an empty string", node_type),
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
        .ok_or_else(|| format!("{}: 'store' is required and must name a declared stores: entry", node_type))?;
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
        .ok_or_else(|| format!("{}: 'store' is required and must name a declared stores: entry", node_type))?;
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
pub fn optional_ttl(config: &HashMap<String, Value>, node_type: &str) -> Result<Option<u64>, String> {
    match config.get("ttl_seconds") {
        None | Some(Value::Null) => Ok(None),
        Some(v) => {
            let n = v
                .as_u64()
                .ok_or_else(|| format!("{}: 'ttl_seconds' must be a non-negative integer", node_type))?;
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
pub fn value_invalid(node_type: &str, msg: String) -> GatewayError {
    GatewayError {
        node_id: String::new(),
        code: "STORE_VALUE_INVALID".to_string(),
        message: format!("{}: {}", node_type, msg),
        metadata: HashMap::new(),
    }
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test store_kv`
Expected: 10 passed (9 here plus the one that moved from Task 0).

Then run the full suite to confirm nothing else moved: `cargo test`

- [ ] **Step 5: Do NOT commit yet**

Leave the helper staged. It is committed as part of Task 2, whose node is its
first caller — see the note at the top of this task. The commit message below is
folded into Task 2's.

```text
(folded into Task 2's commit)
feat(stores): shared helper for the store-* nodes

Store resolution, kv: key namespacing, and the two error codes the four
nodes share. Resolution happens at policy-compile time and the node holds
the handle, the pattern limit-count established.

Without the redis-store feature resolve() fails at compile time naming
the missing feature, so a headless build gives a clear error instead of a
mysterious request-time failure."
```

---

### Task 2: `store-get`

**Files:**
- Create: `src/plugins/native/store_get.rs`
- Modify: `src/plugins/native/mod.rs`, `src/plugins/mod.rs`, `src/plugins/ports.rs`, `src/admin/policies.rs`, `ui/src/pluginCategories.ts`, `ui/src/pluginMeta.tsx`, `ui/src/pluginConfig.ts`
- Create: `website/docs/reference/plugins/store-get.md`
- Modify: `website/sidebars.ts`, `website/docs/reference/plugins/index.md`

**Interfaces:**
- Consumes: everything Task 1 produced.
- Produces: `StoreGetPlugin::from_config(&HashMap<String, Value>, &Arc<PluginResources>) -> Result<Self, String>`; the port name `"miss"`.

- [ ] **Step 1: Write the failing tests**

Create `src/plugins/native/store_get.rs` with only this test module:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugins::resources::PluginResources;
    use std::collections::HashMap;

    fn cfg(json: serde_json::Value) -> HashMap<String, serde_json::Value> {
        serde_json::from_value(json).unwrap()
    }

    #[test]
    fn test_requires_a_name_to_write_into_message() {
        let r = PluginResources::empty();
        let err = StoreGetPlugin::from_config(
            &cfg(serde_json::json!({ "store": "s", "key": "k" })),
            &r,
        )
        .unwrap_err();
        assert!(err.contains("name"), "{err}");
    }

    /// A JSON object is flattened into dotted message keys, because
    /// `{{message.a.b}}` resolves the literal key "a.b" rather than traversing
    /// (`src/vars/mod.rs` message_str is a flat lookup).
    #[test]
    fn test_flatten_object_writes_dotted_keys() {
        let mut msg = HashMap::new();
        flatten_into(&mut msg, "profile", serde_json::json!({"tier": "gold", "seats": 3}));
        assert_eq!(msg.get("profile.tier").unwrap(), "gold");
        assert_eq!(msg.get("profile.seats").unwrap(), 3);
        assert!(msg.get("profile").is_none(), "the object itself must not be written");
    }

    /// A JSON scalar keeps the plain name so `$msg_<name>` still works, which is
    /// the common case for a counter read back after store-incr.
    #[test]
    fn test_flatten_scalar_writes_the_plain_name() {
        let mut msg = HashMap::new();
        flatten_into(&mut msg, "retry_count", serde_json::json!(3));
        assert_eq!(msg.get("retry_count").unwrap(), 3);
    }

    /// Nested values are written as their own dotted key, not recursed into:
    /// one level is what a flat namespace can express honestly.
    #[test]
    fn test_flatten_does_not_recurse() {
        let mut msg = HashMap::new();
        flatten_into(&mut msg, "cfg", serde_json::json!({"limits": {"rps": 10}}));
        assert_eq!(msg.get("cfg.limits").unwrap(), &serde_json::json!({"rps": 10}));
        assert!(msg.get("cfg.limits.rps").is_none());
    }

    /// The key is a template, so it can reference the response body — in which
    /// case this node genuinely reads the body and must force buffering.
    #[test]
    fn test_reads_response_body_follows_the_key_template() {
        let r = PluginResources::empty();
        let plain = StoreGetPlugin::from_config(
            &cfg(serde_json::json!({ "store": "s", "key": "k:{{request.path}}", "name": "v" })),
            &r,
        );
        let reads = StoreGetPlugin::from_config(
            &cfg(serde_json::json!({ "store": "s", "key": "k:{{response.body}}", "name": "v" })),
            &r,
        );
        // Both fail to resolve an undeclared store; assert on the templates via
        // the helper instead, so the check does not depend on a live store.
        assert!(plain.is_err() && reads.is_err());
    }
}
```

> Note on the last test: `PluginResources::empty()` has no declared stores, so
> `from_config` cannot succeed in a unit test. Construction-independent behavior
> (flattening) is tested directly; `reads_response_body` is covered end-to-end by
> the gated integration test in Task 6 and by the port/catalog tests below. Keep
> the assertion honest rather than asserting something the test cannot observe.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --no-run 2>&1 | grep "^error"`
Expected: `cannot find type 'StoreGetPlugin'`, `cannot find function 'flatten_into'`.

- [ ] **Step 3: Write the implementation**

Above the test module in `src/plugins/native/store_get.rs`:

```rust
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
use crate::plugins::{Plugin, PluginExecutionError, PluginOutput, PluginResult};
use crate::vars::template::Template;

pub struct StoreGetPlugin {
    store: StoreHandle,
    key: Template,
    name: String,
    json: bool,
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
            .ok_or_else(|| "store-get: 'name' is required (the context.message key to write)".to_string())?
            .to_string();
        Ok(Self {
            key: store_kv::required_template(config, "key", "store-get")?,
            store: store_kv::resolve(config, resources, "store-get")?,
            name,
            json: config.get("json").and_then(|v| v.as_bool()).unwrap_or(false),
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
                    error: store_kv::store_error("store-get", "GET", &self.store.name, e.to_string()),
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
```

- [ ] **Step 4: Declare the ports**

In `src/plugins/ports.rs`, after `PROXY_CACHE_SPEC`:

```rust
/// `store-get`: a key that does not exist is a normal outcome, not an error —
/// it exits `miss`, which the compiler forces the policy to wire. A store
/// outage exits `error` instead, so the two stay distinguishable.
pub const STORE_GET_SPEC: PortSpec = PortSpec {
    input: Some("Request context from the previous node."),
    outputs: &[
        SUCCESS,
        PortDecl {
            name: "miss",
            kind: PortKind::Outcome,
            description: "The key does not exist; nothing was written to context.message. Wire to whatever should happen on first sight.",
        },
        ERROR,
    ],
};

/// `store-set` / `store-incr` / `store-delete`: a write either happens or fails.
pub const STORE_WRITE_SPEC: PortSpec = PortSpec {
    input: Some("Request context from the previous node."),
    outputs: &[SUCCESS, ERROR],
};
```

In `src/plugins/mod.rs`, in the `port_spec` match:

```rust
        "store-get" => Some(&ports::STORE_GET_SPEC),
        "store-set" | "store-incr" | "store-delete" => Some(&ports::STORE_WRITE_SPEC),
```

- [ ] **Step 5: Register the node**

`src/plugins/native/mod.rs`:

```rust
pub mod store_get;
```

`src/plugins/mod.rs` — add `"store-get"` to `KNOWN_PLUGIN_TYPES`, and to `create_plugin`:

```rust
        "store-get" => Ok(Box::new(native::store_get::StoreGetPlugin::from_config(
            config, resources,
        )?)),
```

`src/admin/policies.rs`, in `CATALOG`:

```rust
        (
            "store-get",
            "Read a key from a shared store into context.message (miss port when absent)",
        ),
```

- [ ] **Step 6: Register in the UI**

`ui/src/pluginCategories.ts` — add a group (the other three nodes join it in Tasks 3–5):

```ts
  {
    label: 'Policy state',
    types: ['store-get'],
  },
```

`ui/src/pluginMeta.tsx` — import `Database` from `lucide-react` alongside the existing icons, then:

```tsx
  'store-get':          { color: '#0891b2', icon: Database },
```

`ui/src/pluginConfig.ts`:

```ts
  'store-get': [
    { key: 'store', label: 'Store', type: 'select', options: [{ value: '', label: '(none)' }], optionsFrom: 'stores', hint: 'a declared stores: entry' },
    { key: 'key', label: 'Key', type: 'text', placeholder: 'retry:{{request.cookie.fb_sid}}', hint: 'templated; stored under the kv: namespace', template: 'full', legacyDollar: true },
    { key: 'name', label: 'Message key', type: 'text', placeholder: 'retry_count', hint: 'context.message key to write; readable as $msg_<name>' },
    { key: 'json', label: 'Parse JSON', type: 'boolean', default: false, hint: 'flattens an object into <name>.<field> message keys' },
  ],
```

- [ ] **Step 7: Write the docs page**

Create `website/docs/reference/plugins/store-get.md`. It must carry the config table, the ports table, both error codes, the `kv:` namespacing rule, and the flattening rule — this page is what MCP's `get_node_type` hands an agent.

````markdown
---
title: store-get
---

# store-get

Reads a key from a declared [store](../../guides/configuration.md) into `context.message`.

```yaml
- id: read-retries
  type: store-get
  config:
    store: sessions
    key: "retry:{{request.cookie.fb_sid}}"
    name: retry_count
    json: false
```

## Config

| Key | Type | Required | Default | Meaning |
|---|---|---|---|---|
| `store` | string | yes | — | Name of a declared `stores:` entry |
| `key` | string (templated) | yes | — | Key to read |
| `name` | string | yes | — | `context.message` key to write; readable as `$msg_<name>` |
| `json` | bool | no | `false` | Parse a JSON object and flatten its top-level fields |

## Ports

| Port | Kind | Meaning |
|---|---|---|
| `success` | success | The key existed; its value is in `context.message` |
| `miss` | outcome | The key does not exist; nothing was written |
| `error` | error | The store could not be reached, or the value was unusable |

`miss` and `error` are deliberately separate. A store outage must not look like
"nothing recorded" — otherwise a policy takes its happy path during exactly the
incident where that is most wrong.

## Keys are namespaced

Every key is stored as `<store key_prefix>:kv:<your key>`, so policy keys cannot
collide with the session, rate-limit and ACME keys that share the same store. A
key written by another system is not reachable.

## `json: true` flattens, it does not nest

`context.message` is a flat namespace — `{{message.a.b}}` resolves the literal
key `"a.b"` rather than traversing into an object. So a parsed object has its
top-level fields written as separate keys:

```yaml
# stored value: {"tier":"gold","seats":3}   with name: profile
# {{message.profile.tier}}  -> gold
# {{message.profile.seats}} -> 3
```

Nested objects and arrays are written as their own JSON value under one dotted
key, not recursed into. A JSON **scalar** is written under `name` unchanged, so
`$msg_<name>` keeps working.

Flattened keys are readable through `{{message.…}}` but **not** through legacy
`$msg_<name>`, where a dot ends the token.

A value that is not valid JSON exits `error` with `STORE_VALUE_INVALID` rather
than falling back to the raw string, which would make a malformed value
indistinguishable from a good one downstream.

## Errors

| Code | When |
|---|---|
| `STORE_ERROR` | The store could not be reached |
| `STORE_VALUE_INVALID` | `json: true` and the value is not valid JSON |

## See also

[store-set](./store-set.md) · [store-incr](./store-incr.md) · [store-delete](./store-delete.md)
````

Add `reference/plugins/store-get` to `website/sidebars.ts` in the plugins section, and a row to the table in `website/docs/reference/plugins/index.md`.

- [ ] **Step 8: Run the full suite**

Run: `cargo test`
Expected: all pass, including `test_catalog_covers_factory`, `test_every_catalog_plugin_has_an_icon`, `test_every_catalog_plugin_is_in_a_palette_category`, `test_every_catalog_plugin_has_a_docs_page`, `test_every_plugin_docs_page_is_in_the_sidebar`.

Run: `cargo test --release`, `cargo fmt --all --check`, `cargo clippy --all-targets`
Then the UI: `cd ui && npm run lint && npx tsc -b && npm test`

- [ ] **Step 9: Commit**

```bash
git add src/ ui/ website/
git commit -m "feat(stores): store-get node reads a key into context.message

A missing key exits a dedicated 'miss' outcome port, which the compiler
forces the policy to wire, so 'first sight' is an explicit branch. A store
outage exits 'error' instead: conflating the two would make a redis
failure look like 'nothing recorded', and the policy would take its happy
path during exactly the incident where that is most wrong.

json: true flattens an object's top-level fields into <name>.<field>
message keys rather than nesting, because context.message is a flat
namespace -- {{message.a.b}} resolves the literal key \"a.b\" rather than
traversing into an object."
```

---

### Task 3: `store-set`

**Files:**
- Create: `src/plugins/native/store_set.rs`
- Modify: `src/plugins/native/mod.rs`, `src/plugins/mod.rs`, `src/admin/policies.rs`, `ui/src/pluginCategories.ts`, `ui/src/pluginMeta.tsx`, `ui/src/pluginConfig.ts`
- Create: `website/docs/reference/plugins/store-set.md`
- Modify: `website/sidebars.ts`, `website/docs/reference/plugins/index.md`

**Interfaces:**
- Consumes: `store_kv::{resolve, required_template, optional_ttl, store_error, StoreHandle}`; `ports::STORE_WRITE_SPEC` (already added in Task 2).
- Produces: `StoreSetPlugin::from_config(&HashMap<String, Value>, &Arc<PluginResources>) -> Result<Self, String>`.

- [ ] **Step 1: Write the failing tests**

```rust
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
        let err = StoreSetPlugin::from_config(
            &cfg(serde_json::json!({ "store": "s", "key": "k" })),
            &r,
        )
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
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --no-run 2>&1 | grep "^error"`
Expected: `cannot find type 'StoreSetPlugin'`.

- [ ] **Step 3: Write the implementation**

```rust
//! `store-set` — writes a key into a named store.

use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::Arc;

use crate::context::Context;
use crate::plugins::resources::PluginResources;
use crate::plugins::util::store_kv::{self, StoreHandle};
use crate::plugins::{Plugin, PluginExecutionError, PluginOutput, PluginResult};
use crate::vars::template::Template;

pub struct StoreSetPlugin {
    store: StoreHandle,
    key: Template,
    value: Template,
    ttl_seconds: Option<u64>,
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

        let key = self.store.key_for(&self.key.render(&ctx));
        let value = self.value.render(&ctx).to_string();

        let mut conn = match self.store.conn().await {
            Ok(c) => c,
            Err(e) => {
                return Err(PluginExecutionError {
                    context: ctx,
                    error: store_kv::store_error("store-set", "SET", &self.store.name, e),
                })
            }
        };

        let result = match self.ttl_seconds {
            Some(ttl) => conn.set_ex::<_, _, ()>(&key, value, ttl).await,
            None => conn.set::<_, _, ()>(&key, value).await,
        };

        match result {
            Ok(()) => Ok(PluginOutput::success(ctx)),
            Err(e) => Err(PluginExecutionError {
                context: ctx,
                error: store_kv::store_error("store-set", "SET", &self.store.name, e.to_string()),
            }),
        }
    }

    #[cfg(not(feature = "redis-store"))]
    async fn execute(&self, ctx: Context) -> PluginResult {
        Err(PluginExecutionError {
            context: ctx,
            error: store_kv::store_error(
                "store-set",
                "SET",
                &self.store.name,
                "built without the redis-store feature".to_string(),
            ),
        })
    }
}
```

- [ ] **Step 4: Register the node**

`src/plugins/native/mod.rs`: `pub mod store_set;`

`src/plugins/mod.rs` — `KNOWN_PLUGIN_TYPES` gains `"store-set"`; `create_plugin` gains:

```rust
        "store-set" => Ok(Box::new(native::store_set::StoreSetPlugin::from_config(
            config, resources,
        )?)),
```

`src/admin/policies.rs` `CATALOG`:

```rust
        (
            "store-set",
            "Write a key into a shared store, with an optional TTL",
        ),
```

`ui/src/pluginCategories.ts` — add `'store-set'` to the `Policy state` group.

`ui/src/pluginMeta.tsx`:

```tsx
  'store-set':          { color: '#0891b2', icon: Database },
```

`ui/src/pluginConfig.ts`:

```ts
  'store-set': [
    { key: 'store', label: 'Store', type: 'select', options: [{ value: '', label: '(none)' }], optionsFrom: 'stores', hint: 'a declared stores: entry' },
    { key: 'key', label: 'Key', type: 'text', placeholder: 'seen:{{request.headers.x-session}}', hint: 'templated; stored under the kv: namespace', template: 'full', legacyDollar: true },
    { key: 'value', label: 'Value', type: 'text', placeholder: '1', hint: 'templated', template: 'full', legacyDollar: true },
    { key: 'ttl_seconds', label: 'TTL (s)', type: 'number', hint: 'omit for no expiry; 0 is rejected' },
  ],
```

- [ ] **Step 5: Write the docs page**

Create `website/docs/reference/plugins/store-set.md` with the same section structure as `store-get.md`: config table, ports table (`success`, `error`), the `kv:` namespacing rule, `STORE_ERROR`, and an explicit note that omitting `ttl_seconds` means the key persists indefinitely — with the warning that a policy writing per-request keys without a TTL will grow the store without bound. Cross-link to `store-incr.md` for the worked example. Add the sidebar entry and the plugin-index row.

- [ ] **Step 6: Run the full suite**

Run: `cargo test` then `cargo test --release`, `cargo fmt --all --check`, `cargo clippy --all-targets`, and `cd ui && npm run lint && npx tsc -b && npm test`.

- [ ] **Step 7: Commit**

```bash
git add src/ ui/ website/
git commit -m "feat(stores): store-set node writes a key with an optional TTL

ttl_seconds is optional and omitting it means the key persists; 0 is
rejected as a config error rather than read as 'no expiry', because the
two readings are too easy to confuse.

Both key and value are templates, so reads_response_body is answered from
them: a value of {{response.body}} correctly forces the upstream to
buffer."
```

---

### Task 4: `store-incr`

**Files:**
- Create: `src/plugins/native/store_incr.rs`
- Modify: `src/plugins/native/mod.rs`, `src/plugins/mod.rs`, `src/admin/policies.rs`, `ui/src/pluginCategories.ts`, `ui/src/pluginMeta.tsx`, `ui/src/pluginConfig.ts`
- Create: `website/docs/reference/plugins/store-incr.md`
- Modify: `website/sidebars.ts`, `website/docs/reference/plugins/index.md`

**Interfaces:**
- Consumes: `store_kv::{resolve, required_template, optional_ttl, store_error, value_invalid, StoreHandle}`.
- Produces: `StoreIncrPlugin::from_config(&HashMap<String, Value>, &Arc<PluginResources>) -> Result<Self, String>`.

- [ ] **Step 1: Write the failing tests**

```rust
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
        let err = StoreIncrPlugin::from_config(
            &cfg(serde_json::json!({ "store": "s", "key": "k" })),
            &r,
        )
        .unwrap_err();
        assert!(err.contains("name"), "{err}");
    }

    #[test]
    fn test_by_defaults_to_one_and_accepts_negatives() {
        assert_eq!(parse_by(&cfg(serde_json::json!({}))).unwrap(), 1);
        assert_eq!(parse_by(&cfg(serde_json::json!({ "by": -2 }))).unwrap(), -2);
        assert!(parse_by(&cfg(serde_json::json!({ "by": "x" }))).is_err());
    }

    /// The script must set the expiry only when the key has none. Refreshing it
    /// on every increment means a client that keeps retrying keeps the counter
    /// alive and the bound never resets — which is the whole point of the node.
    #[test]
    fn test_script_sets_expiry_only_when_absent() {
        assert!(INCR_SCRIPT.contains("TTL"), "script must inspect the existing TTL");
        assert!(
            INCR_SCRIPT.contains("EXPIRE"),
            "script must set an expiry when there is none"
        );
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --no-run 2>&1 | grep "^error"`
Expected: `cannot find type 'StoreIncrPlugin'`, `cannot find function 'parse_by'`, `cannot find value 'INCR_SCRIPT'`.

- [ ] **Step 3: Write the implementation**

```rust
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
use crate::plugins::{Plugin, PluginExecutionError, PluginOutput, PluginResult};
use crate::vars::template::Template;

/// INCRBY, then set the expiry only if the key does not already have one.
/// `TTL` returns a negative value when the key has no expiry, so the guard also
/// repairs a key that somehow lost one. One round trip, atomic.
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
        let key = self.store.key_for(&self.key.render(&ctx));

        let mut conn = match self.store.conn().await {
            Ok(c) => c,
            Err(e) => {
                return Err(PluginExecutionError {
                    context: ctx,
                    error: store_kv::store_error("store-incr", "INCRBY", &self.store.name, e),
                })
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
                // A counter key holding a non-numeric value is a config/data
                // problem, not an outage, and gets its own code.
                let error = if e.to_string().contains("not an integer") {
                    store_kv::value_invalid(
                        "store-incr",
                        format!("value at '{}' is not an integer", key),
                    )
                } else {
                    store_kv::store_error("store-incr", "INCRBY", &self.store.name, e.to_string())
                };
                return Err(PluginExecutionError { context: ctx, error });
            }
        };

        ctx.message
            .insert(self.name.clone(), serde_json::Value::from(n));
        Ok(PluginOutput::success(ctx))
    }

    #[cfg(not(feature = "redis-store"))]
    async fn execute(&self, ctx: Context) -> PluginResult {
        Err(PluginExecutionError {
            context: ctx,
            error: store_kv::store_error(
                "store-incr",
                "INCRBY",
                &self.store.name,
                "built without the redis-store feature".to_string(),
            ),
        })
    }
}
```

- [ ] **Step 4: Register the node**

`src/plugins/native/mod.rs`: `pub mod store_incr;`

`src/plugins/mod.rs` — `KNOWN_PLUGIN_TYPES` gains `"store-incr"`; `create_plugin` gains:

```rust
        "store-incr" => Ok(Box::new(native::store_incr::StoreIncrPlugin::from_config(
            config, resources,
        )?)),
```

`src/admin/policies.rs` `CATALOG`:

```rust
        (
            "store-incr",
            "Atomically increment a counter in a shared store (TTL set at creation)",
        ),
```

`ui/src/pluginCategories.ts` — add `'store-incr'` to the `Policy state` group.

`ui/src/pluginMeta.tsx`:

```tsx
  'store-incr':         { color: '#0891b2', icon: Database },
```

`ui/src/pluginConfig.ts`:

```ts
  'store-incr': [
    { key: 'store', label: 'Store', type: 'select', options: [{ value: '', label: '(none)' }], optionsFrom: 'stores', hint: 'a declared stores: entry' },
    { key: 'key', label: 'Key', type: 'text', placeholder: 'retry:{{request.cookie.fb_sid}}', hint: 'templated; stored under the kv: namespace', template: 'full', legacyDollar: true },
    { key: 'by', label: 'By', type: 'number', default: 1, hint: 'amount to add; may be negative' },
    { key: 'ttl_seconds', label: 'TTL (s)', type: 'number', hint: 'applied only when the key is created, never refreshed' },
    { key: 'name', label: 'Message key', type: 'text', placeholder: 'retry_count', hint: 'receives the new value; readable as $msg_<name>' },
  ],
```

- [ ] **Step 5: Write the docs page, including the worked example**

Create `website/docs/reference/plugins/store-incr.md` with the config table, ports (`success`, `error`), both error codes, the `kv:` rule, and — stated prominently, because it is the least guessable thing about the node:

````markdown
:::note[The TTL applies at creation, not on every increment]
A counter that bounds retries must expire a fixed time after it **first
appears**. If the expiry were refreshed on each increment, a client that keeps
retrying would keep the counter alive and the bound would never reset.
:::
````

Then the worked end-to-end example the other three pages link to — bounding OIDC
retries with all four nodes:

````markdown
## Worked example: bounding OIDC retries

A CSRF/state mismatch on the OIDC callback is usually a stale login tab, and a
fresh flow fixes it transparently. Retrying forever would loop, so the retry is
counted server-side and capped at three.

```yaml
# on the oidc.denied path
- id: count-retry
  type: store-incr
  config:
    store: sessions
    key: "oidc-retry:{{client.ip}}"
    ttl_seconds: 300
    name: retry_count

- id: under-cap
  type: condition
  config:
    conditions: [["msg_retry_count", "<=", 3]]

- id: relogin
  type: redirect
  config: { ret_code: 302, uri: "/" }
```

Wire `count-retry.success → under-cap.in`, `under-cap.true → relogin.in`, and
`under-cap.false → client.in` so the fourth failure surfaces the original error
instead of looping. On a successful login, clear the counter:

```yaml
- id: clear-retries
  type: store-delete
  config:
    store: sessions
    key: "oidc-retry:{{client.ip}}"
```

This replaces the cookie-based guard it grew out of: a client can clear its own
cookie, but not a key in the store.
````

Add the sidebar entry and the plugin-index row.

- [ ] **Step 6: Run the full suite**

Run: `cargo test` then `cargo test --release`, `cargo fmt --all --check`, `cargo clippy --all-targets`, and `cd ui && npm run lint && npx tsc -b && npm test`.

- [ ] **Step 7: Commit**

```bash
git add src/ ui/ website/
git commit -m "feat(stores): store-incr node for atomic counters

INCRBY plus an expiry applied only when the key has none, in one Lua
script so it is atomic and costs one round trip -- the approach
src/stores/counter.rs already takes for its window arithmetic.

The TTL rule is the point of the node: a counter bounding retries must
expire a fixed time after it first appears. Refreshing the expiry on
every increment means a client that keeps retrying keeps the counter
alive and the bound never resets.

A key holding a non-numeric value reports STORE_VALUE_INVALID rather than
STORE_ERROR, so a data problem is not mistaken for an outage."
```

---

### Task 5: `store-delete`

**Files:**
- Create: `src/plugins/native/store_delete.rs`
- Modify: `src/plugins/native/mod.rs`, `src/plugins/mod.rs`, `src/admin/policies.rs`, `ui/src/pluginCategories.ts`, `ui/src/pluginMeta.tsx`, `ui/src/pluginConfig.ts`
- Create: `website/docs/reference/plugins/store-delete.md`
- Modify: `website/sidebars.ts`, `website/docs/reference/plugins/index.md`

**Interfaces:**
- Consumes: `store_kv::{resolve, required_template, store_error, StoreHandle}`.
- Produces: `StoreDeletePlugin::from_config(&HashMap<String, Value>, &Arc<PluginResources>) -> Result<Self, String>`.

- [ ] **Step 1: Write the failing test**

```rust
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
        let err =
            StoreDeletePlugin::from_config(&cfg(serde_json::json!({ "store": "s" })), &r).unwrap_err();
        assert!(err.contains("key"), "{err}");
    }
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test --no-run 2>&1 | grep "^error"`
Expected: `cannot find type 'StoreDeletePlugin'`.

- [ ] **Step 3: Write the implementation**

```rust
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
use crate::plugins::{Plugin, PluginExecutionError, PluginOutput, PluginResult};
use crate::vars::template::Template;

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

        let key = self.store.key_for(&self.key.render(&ctx));

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
                error: store_kv::store_error("store-delete", "DEL", &self.store.name, e.to_string()),
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
```

- [ ] **Step 4: Register the node**

`src/plugins/native/mod.rs`: `pub mod store_delete;`

`src/plugins/mod.rs` — `KNOWN_PLUGIN_TYPES` gains `"store-delete"`; `create_plugin` gains:

```rust
        "store-delete" => Ok(Box::new(
            native::store_delete::StoreDeletePlugin::from_config(config, resources)?,
        )),
```

`src/admin/policies.rs` `CATALOG`:

```rust
        (
            "store-delete",
            "Remove a key from a shared store (idempotent)",
        ),
```

`ui/src/pluginCategories.ts` — add `'store-delete'` to the `Policy state` group.

`ui/src/pluginMeta.tsx`:

```tsx
  'store-delete':       { color: '#0891b2', icon: Database },
```

`ui/src/pluginConfig.ts`:

```ts
  'store-delete': [
    { key: 'store', label: 'Store', type: 'select', options: [{ value: '', label: '(none)' }], optionsFrom: 'stores', hint: 'a declared stores: entry' },
    { key: 'key', label: 'Key', type: 'text', placeholder: 'retry:{{request.cookie.fb_sid}}', hint: 'templated; stored under the kv: namespace', template: 'full', legacyDollar: true },
  ],
```

- [ ] **Step 5: Write the docs page**

Create `website/docs/reference/plugins/store-delete.md` with the config table, ports (`success`, `error`), `STORE_ERROR`, the `kv:` rule, and an explicit statement that deleting an absent key is a success — with the reason (a `miss` port would be mandatory-wired in every policy that clears state, for something almost no caller acts on). Cross-link to `store-incr.md`'s worked example. Add the sidebar entry and the plugin-index row.

- [ ] **Step 6: Run the full suite**

Run: `cargo test` then `cargo test --release`, `cargo fmt --all --check`, `cargo clippy --all-targets`, and `cd ui && npm run lint && npx tsc -b && npm test`.

- [ ] **Step 7: Commit**

```bash
git add src/ ui/ website/
git commit -m "feat(stores): store-delete node removes a key

Idempotent: deleting a key that does not exist is a success. This is the
one place the design deliberately does not mirror store-get, which has a
miss port -- here it would be mandatory-wired in every policy that clears
state, to signal something almost no caller acts on."
```

---

### Task 6: Gated integration tests against a real store

**Files:**
- Modify: `src/plugins/util/store_kv.rs` (a gated `#[cfg(test)]` integration module)
- Modify: `e2e/E2E_TESTBOOK.md`

**Interfaces:**
- Consumes: all four plugins' `from_config` and `execute`.
- Produces: nothing other tasks depend on.

These tests need a live redis and are gated on `FEATHERBIT_TEST_REDIS_URL`, which
CI already provides through its `redis:7` + `valkey/valkey:8` service-container
matrix. The unit tests in Tasks 2–5 cannot cover round-trip behavior because
`PluginResources::empty()` has no declared stores.

- [ ] **Step 1: Write the failing tests**

Add to `src/plugins/util/store_kv.rs`, inside a new `#[cfg(all(test, feature = "redis-store"))] mod live_tests`:

```rust
/// Skips unless a real store is configured, the same gate the session and ACME
/// live tests use.
fn store_url() -> Option<String> {
    std::env::var("FEATHERBIT_TEST_REDIS_URL").ok().filter(|s| !s.is_empty())
}

/// Builds PluginResources with one declared store named "test".
fn resources_with_store(url: &str) -> Arc<PluginResources> { /* per crate::stores test helpers */ }

#[tokio::test]
async fn test_set_then_get_round_trips() { /* store-set "1", store-get -> success, $msg_v == "1" */ }

#[tokio::test]
async fn test_get_on_an_absent_key_exits_miss() { /* PluginOutput.port == Some("miss") */ }

#[tokio::test]
async fn test_json_true_flattens_an_object() { /* message["profile.tier"] == "gold" */ }

#[tokio::test]
async fn test_json_true_on_invalid_json_exits_error() { /* code == STORE_VALUE_INVALID */ }

#[tokio::test]
async fn test_delete_on_an_absent_key_succeeds() { /* Ok, port None */ }

/// The behavior store-incr exists for, and the one most likely to regress.
#[tokio::test]
async fn test_incr_does_not_refresh_the_ttl() {
    // incr with ttl_seconds: 60; read PTTL; sleep 1100ms; incr again;
    // assert the second PTTL is strictly less than the first.
}

/// A store outage must exit error, never miss — the distinction the design
/// turns on.
#[tokio::test]
async fn test_unreachable_store_exits_error_not_miss() {
    // Point a store at a closed port; assert Err with code STORE_ERROR.
}
```

Fill each body in following the style of the existing gated tests in
`src/acme/live_tests.rs`, which show the skip-if-unset pattern and the resource
construction this needs.

- [ ] **Step 2: Run them to verify they fail (or skip cleanly)**

Without the env var: `cargo test store_kv::live_tests` → all skip, 0 failures.
With it set: `FEATHERBIT_TEST_REDIS_URL=redis://127.0.0.1:6379 cargo test store_kv::live_tests` → failures naming the unimplemented bodies.

- [ ] **Step 3: Implement the test bodies and confirm they pass**

Run: `FEATHERBIT_TEST_REDIS_URL=redis://127.0.0.1:6379 cargo test store_kv::live_tests`
Expected: 7 passed. Start a local redis with `docker run --rm -p 6379:6379 redis:7` if needed.

- [ ] **Step 4: Add the e2e scenarios**

Add `E2E-STORE-10` through `E2E-STORE-13` to `e2e/E2E_TESTBOOK.md`: a policy that
reads a counter and branches on `miss`; one that increments and caps; one that
deletes and re-reads expecting `miss`; and one asserting a route using these
nodes still streams (they must not force buffering).

- [ ] **Step 5: Commit**

```bash
git add src/plugins/util/store_kv.rs e2e/E2E_TESTBOOK.md
git commit -m "test(stores): gated round-trip tests for the store-* nodes

Covers what unit tests cannot: PluginResources::empty() has no declared
stores, so round-trip behavior needs a live backend. Gated on
FEATHERBIT_TEST_REDIS_URL, which CI already provides for both redis:7 and
valkey:8.

The load-bearing one asserts store-incr does not refresh the TTL: that is
the behavior the node exists for and the one most likely to regress into
a counter that never resets."
```

---

### Task 7: Agent-facing surface and parity docs

**Files:**
- Modify: `src/mcp/tools/catalog.rs` (tests only)
- Modify: `docs/apisix-parity.md`

**Interfaces:**
- Consumes: the four registered node types.
- Produces: nothing other tasks depend on.

The drift guards prove a docs page *exists*; these tests prove it actually
reaches an agent through `get_node_type`, and that the `miss` port is visible so
an agent knows it must be wired.

- [ ] **Step 1: Write the failing tests**

Add to the test module in `src/mcp/tools/catalog.rs`:

```rust
/// get_node_type is what an agent reads before authoring a node. A missing
/// docs page returns `docs: null` rather than failing, so assert it directly
/// instead of trusting the file-existence guard.
#[tokio::test]
async fn store_nodes_expose_their_docs_to_agents() {
    let s = test_state();
    for t in ["store-get", "store-set", "store-incr", "store-delete"] {
        let v = call(&s, "get_node_type", obj(serde_json::json!({ "type": t })))
            .await
            .unwrap();
        assert!(
            v["docs"].as_str().is_some_and(|d| !d.is_empty()),
            "{t} must serve a docs page to agents"
        );
    }
}

/// An agent must see that store-get has a mandatory `miss` port, or it will
/// author a policy that fails to compile.
#[tokio::test]
async fn store_get_exposes_its_miss_port() {
    let s = test_state();
    let v = call(&s, "get_node_type", obj(serde_json::json!({ "type": "store-get" })))
        .await
        .unwrap();
    let outs = v["ports"]["outputs"].as_array().unwrap();
    let miss = outs.iter().find(|p| p["name"] == "miss").expect("miss port");
    assert_eq!(miss["kind"], "outcome");
}
```

Use the same `test_state()` / `call` / `obj` helpers the existing tests in that
module use (see the `condition` port assertion already there).

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test store_nodes_expose_their_docs_to_agents store_get_exposes_its_miss_port`
Expected: FAIL if any docs page or port declaration is missing. If Tasks 2–5 were
completed correctly they pass immediately — in that case confirm they are
genuinely exercising the path by temporarily renaming `store-get.md` and seeing
the first test fail, then restoring it.

- [ ] **Step 3: Add the parity rows**

Add four rows to `docs/apisix-parity.md` marking these as featherbit-native
(APISIX has no equivalent node), with a one-line description each.

- [ ] **Step 4: Run the full suite**

Run: `cargo test`, `cargo test --release`, `cargo fmt --all --check`, `cargo clippy --all-targets`, and `cd ui && npm run lint && npx tsc -b && npm test`.
Also build the docs site once, since four pages and four sidebar entries were added: `cd website && npm run build`.

- [ ] **Step 5: Commit**

```bash
git add src/mcp/tools/catalog.rs docs/apisix-parity.md
git commit -m "test(mcp): assert the store-* nodes reach agents

The catalog drift guards prove a docs page exists on disk; these assert
get_node_type actually serves it, which is the path an agent reads. A
missing page returns docs: null rather than failing, so the gap would
otherwise be silent.

Also asserts store-get's miss port is visible as an outcome, since an
agent that cannot see it will author a policy that fails to compile."
```

---

## Self-Review

**Spec coverage.** §4 (four node types) → Tasks 2–5. §5.1–5.4 (the four nodes) →
Tasks 2–5. §6 (namespacing) → Task 1, `namespaced_key`. §7 (failure handling) →
Task 1 `store_error`, plus the `miss`-vs-`error` live test in Task 6. §8
(streaming) → `reads_response_body` in each of Tasks 2–5, and the e2e scenario in
Task 6 Step 4. §10 (registration points) → Steps 4–7 of each node task. §11 (docs
and MCP) → the docs page in each node task, plus Task 7. §12 (testing) → the unit
tests in each task plus Task 6.

**Placeholders.** Task 6's test bodies are stubs by design — they need a live
backend and follow an existing pattern (`src/acme/live_tests.rs`) rather than
being invented here; the assertions each must make are stated exactly. Every
other code step is complete.

**Type consistency.** `StoreHandle`, `store_kv::resolve`, `required_template`,
`optional_ttl`, `store_error`, `value_invalid` and `namespaced_key` are defined in
Task 1 and used with those exact names and signatures in Tasks 2–5.
`STORE_WRITE_SPEC` is defined in Task 2 and reused by Tasks 3–5.
`PluginOutput::on_port(ctx, "miss")` matches the API at `src/plugins/mod.rs:40`.
