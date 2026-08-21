# Shared Stores + Redis Rate-Limit Counters Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a named, shared `stores:` resource (Redis/Valkey connections) to `gateway.yaml`, with Admin API CRUD + ping, etcd sync, and a Redis-backed `CounterStore` giving `limit-count` and the `workflow` limit-count action cluster-accurate rate limiting (`policy: redis`).

**Architecture:** A new `src/stores/` module owns config validation, a `StoreRegistry` (one lazily-connecting client per named store, reused across reloads when config is unchanged), and the Redis fixed-window counter. The registry lives in `PluginResources` behind an `ArcSwap`, is rebuilt inside `SharedState::compile_routes` (candidate swapped in for the duration of the compile, restored on failure — the last-good invariant holds because the registry is only *read* at plugin-construction time, never on the request path). Plugins resolve stores by name at config time, so a bad reference fails compile, not a request.

**Tech Stack:** Rust (single crate `featherbit`, edition 2021), axum 0.8 admin API, `redis` crate (tokio + rustls/ring) behind a new default-on `redis-store` cargo feature, prometheus, serde/serde_yaml.

**Spec:** `docs/superpowers/specs/2026-08-21-session-storage-design.md` (this plan implements §1 "The `stores:` resource", "Cluster readiness", §3 "Redis CounterStore", and the stores/ping part of §4; sessions are Plan 2, UI/e2e/CI are Plan 3).

## Global Constraints

- Branch: `feature/session-storage` (already exists; all commits go there).
- Conventional Commits (`feat:`, `fix:`, `docs:`, `test:`, `refactor:`, optional scope). **No Co-Authored-By trailer** (project rule).
- All fallible config-path code returns `Result<_, String>` — no custom error enums (house style, see `src/state.rs`).
- Every Prometheus metric name is prefixed `gateway_` (house style; the spec's `counter_store_errors_total` ships as `gateway_counter_store_errors_total` — Task 10 amends the spec).
- The dependency tree is **ring-only**: after adding the `redis` crate, `cargo tree -i aws-lc-sys` must print nothing (Task 3 verifies).
- Security invariant: `gateway.yaml` values are stored **raw**; `${ENV_VAR:-default}` placeholders resolve only at point of use (here: when a store client is built). The Admin API must never serve resolved secrets.
- `cargo build`/`cargo test` with default features requires `ui/dist` to exist (rust-embed). If missing: `cd ui && npm ci && npm run build` once.
- Per task, before the commit step: `cargo fmt` and `cargo clippy --all-targets -- -D warnings` must be clean (CI enforces both).
- Cluster-readiness (spec): per-key hash tags are Plan 2 (session keys); in this plan the counter key is single-key so no hash tag is needed, but `topology`/`urls` config fields must be accepted-and-rejected exactly as specified in Task 1.
- Spec deviation locked in here: **no `pool_size` field.** The client is one auto-reconnecting multiplexed connection (`redis::aio::ConnectionManager`), which pipelines concurrent commands; a pool adds nothing for RESP. Task 10 amends the spec.

---

### Task 1: `StoreConfig` model + `GatewayConfig.stores`

**Files:**
- Modify: `src/config/gateway.rs` (struct after `PluginConfigDef`, field in `GatewayConfig` at :26-46, test at the bottom)
- Modify: `src/config/mod.rs:23-26` (re-export)
- Modify: `src/config_store/etcd.rs:306-312` and `:530-536` (exhaustive `GatewayConfig` literals — minimal `stores: Vec::new()` fix; the real etcd family is Task 7)

**Interfaces:**
- Produces: `pub struct StoreConfig { name: String, store_type: String, description: Option<String>, url: String, password: Option<String>, key_prefix: String, topology: Option<String>, urls: Option<Vec<String>>, connect_timeout_ms: u64, tls: Option<StoreTlsConfig> }`, `pub struct StoreTlsConfig { ca_cert_path: Option<String> }`, `GatewayConfig.stores: Vec<StoreConfig>` — every later task consumes these exact names.

- [ ] **Step 1: Write the failing round-trip test**

At the bottom of the `#[cfg(test)] mod tests` in `src/config/gateway.rs` (after `test_plugin_configs_default_empty_and_roundtrip`, ~line 302):

```rust
    /// Old configs stay valid; a store declaration round-trips through YAML
    /// with placeholders preserved verbatim; optionals are omitted from output.
    #[test]
    fn test_stores_default_empty_and_roundtrip() {
        let gw: GatewayConfig = serde_yaml::from_str("{}").unwrap();
        assert!(gw.stores.is_empty());

        let yaml = r#"
stores:
  - name: sessions-redis
    type: valkey
    description: "Shared session/counter backend"
    url: ${REDIS_URL:-redis://127.0.0.1:6379}
    password: ${REDIS_PASSWORD:-}
    key_prefix: fb
    connect_timeout_ms: 1500
    tls:
      ca_cert_path: /etc/ssl/redis-ca.pem
"#;
        let gw: GatewayConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(gw.stores.len(), 1);
        let s = &gw.stores[0];
        assert_eq!(s.name, "sessions-redis");
        assert_eq!(s.store_type, "valkey");
        assert_eq!(s.url, "${REDIS_URL:-redis://127.0.0.1:6379}");
        assert_eq!(s.password.as_deref(), Some("${REDIS_PASSWORD:-}"));
        assert_eq!(s.key_prefix, "fb");
        assert_eq!(s.connect_timeout_ms, 1500);
        assert_eq!(
            s.tls.as_ref().unwrap().ca_cert_path.as_deref(),
            Some("/etc/ssl/redis-ca.pem")
        );
        assert!(s.topology.is_none());
        assert!(s.urls.is_none());

        // Defaults apply when omitted.
        let gw: GatewayConfig = serde_yaml::from_str(
            "stores:\n  - name: s1\n    type: redis\n    url: redis://localhost\n",
        )
        .unwrap();
        assert_eq!(gw.stores[0].key_prefix, "fb");
        assert_eq!(gw.stores[0].connect_timeout_ms, 2000);

        // Round-trip: raw placeholder survives serialization, no null noise.
        let out = serde_yaml::to_string(&gw).unwrap();
        assert!(out.contains("redis://localhost"), "{out}");
        assert!(!out.contains("description"), "{out}");
        assert!(!out.contains("topology"), "{out}");
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test test_stores_default_empty_and_roundtrip -- --exact`
Expected: COMPILE FAIL — `no field `stores` on type `GatewayConfig``

- [ ] **Step 3: Add the structs and field**

In `src/config/gateway.rs`, add to `GatewayConfig` (after the `plugin_configs` field):

```rust
    /// Named shared stores (redis/valkey connections) referenced by plugin
    /// config (`store: <name>`); clients are built at config-apply time
    /// (src/stores/), so `${ENV_VAR}` placeholders never leave this struct
    /// resolved.
    #[serde(default)]
    pub stores: Vec<StoreConfig>,
```

After `PluginConfigDef` (~line 214), add:

```rust
/// A named shared store: a redis/valkey connection referenced by name from
/// plugin config. Declared under top-level `stores:`. `url` and `password`
/// support `${ENV_VAR:-default}` placeholders, resolved only when the client
/// is built — the stored config (and everything the Admin API serves) keeps
/// the raw placeholder.
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct StoreConfig {
    /// Unique name, referenced by plugin config (`store: <name>`).
    pub name: String,
    /// Backend type (YAML key: `type`): `redis` or `valkey` — aliases for the
    /// same RESP backend.
    #[serde(rename = "type")]
    pub store_type: String,
    /// Optional human-readable description (shown in the UI library).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Connection URL (`redis://` or `rediss://`).
    pub url: String,
    /// Optional password; overrides any password embedded in `url`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password: Option<String>,
    /// Namespace prefix for every key this store writes.
    #[serde(default = "default_store_key_prefix")]
    pub key_prefix: String,
    /// Reserved for HA topologies (`sentinel`/`cluster`); v1 accepts only
    /// `standalone` (the default) and rejects anything else at config load.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub topology: Option<String>,
    /// Reserved for HA topologies (sentinel/cluster endpoint lists); rejected
    /// at config load in v1.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub urls: Option<Vec<String>>,
    /// Connect/response timeout applied to the client and to `ping`.
    #[serde(default = "default_store_connect_timeout_ms")]
    pub connect_timeout_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tls: Option<StoreTlsConfig>,
}

fn default_store_key_prefix() -> String {
    "fb".to_string()
}

fn default_store_connect_timeout_ms() -> u64 {
    2000
}

/// TLS options for a `rediss://` store.
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct StoreTlsConfig {
    /// PEM CA bundle for a private CA.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ca_cert_path: Option<String>,
}
```

In `src/config/mod.rs`, extend the re-export list:

```rust
pub use gateway::{
    EdgeConfig, GatewayConfig, MatchRule, NodeConfig, PluginConfigDef, PolicyConfig, Position,
    RouteConfig, StoreConfig, StoreTlsConfig, SupernodeConfig,
};
```

- [ ] **Step 4: Fix the exhaustive struct literals the compiler now flags**

`cargo build` will error on the `GatewayConfig { ... }` literals in `src/config_store/etcd.rs` (`gateway_from_kvs` ~:306-312 and the test module ~:530-536). Add `stores: Vec::new(),` to each. (The real etcd `stores/` key family is Task 7 — this is only a compile fix; do not add parsing arms yet.)

- [ ] **Step 5: Run test to verify it passes**

Run: `cargo test test_stores_default_empty_and_roundtrip -- --exact`
Expected: PASS

- [ ] **Step 6: Full check + commit**

Run: `cargo test && cargo fmt && cargo clippy --all-targets -- -D warnings`
Expected: all green (existing tests unaffected — the field defaults to empty).

```bash
git add src/config/gateway.rs src/config/mod.rs src/config_store/etcd.rs
git commit -m "feat(config): add stores resource model to gateway config"
```

---

### Task 2: `src/stores/` module — validation, `StoreRegistry`, compile-time wiring

**Files:**
- Create: `src/stores/mod.rs`
- Modify: `src/main.rs:30-49` (module declaration, alphabetical: after `mod state;`)
- Modify: `Cargo.toml:9-13` (declare the feature — empty for now, the dep lands in Task 3)
- Modify: `src/plugins/resources.rs:24-50` (new `stores` field)
- Modify: `src/state.rs:156-216` (`compile_routes` builds + swaps the registry)

**Interfaces:**
- Consumes: `StoreConfig` from Task 1.
- Produces (used by Tasks 3-9):
  - `pub fn validate_stores(stores: &[StoreConfig]) -> Result<(), String>`
  - `pub struct StoreRegistry` with `impl Default`
  - `pub fn StoreRegistry::rebuild(prev: &StoreRegistry, stores: &[StoreConfig]) -> Result<StoreRegistry, String>`
  - `pub fn StoreRegistry::counter_store(&self, name: &str) -> Result<Arc<dyn CounterStore>, String>`
  - `PluginResources.stores: ArcSwap<StoreRegistry>`

- [ ] **Step 1: Declare the feature (no dep yet)**

In `Cargo.toml` `[features]`:

```toml
[features]
# The embedded admin web UI. Headless build (no UI assets, no serving code):
# `cargo build --release --no-default-features`.
default = ["ui", "redis-store"]
ui = ["dep:rust-embed", "dep:mime_guess"]
# Redis/Valkey client for the `stores:` resource (sessions, distributed rate
# limiting). Off = declaring a store fails config load with a clear error.
redis-store = []
```

- [ ] **Step 2: Write the failing validation + registry tests**

Create `src/stores/mod.rs`:

```rust
//! Named shared stores (`stores:` in gateway.yaml): redis/valkey connections
//! referenced by name from plugin config (`store: <name>`).
//!
//! [`StoreRegistry`] holds one client per named store. It is rebuilt by
//! [`crate::state::SharedState::compile_routes`] on every config (re)compile,
//! reusing the previous client when a store's *resolved* config is unchanged
//! (so an unrelated reload does not drop connections). The registry lives in
//! [`crate::plugins::resources::PluginResources`] behind an `ArcSwap` and is
//! only read at plugin-construction time — never on the request path — so a
//! bad `store:` reference fails policy compilation, not a request.
//!
//! Backend code is gated behind the default-on `redis-store` cargo feature;
//! a headless `--no-default-features` build rejects any declared store at
//! config load with a descriptive error.

use std::collections::HashMap;
use std::sync::Arc;

use crate::config::StoreConfig;
use crate::ratelimit::CounterStore;

#[cfg(feature = "redis-store")]
pub mod redis_store;

/// Validates the `stores:` section: unique non-empty names, known types,
/// and v1 topology restrictions (standalone only). Runs before any client
/// is built, in every compile path (file, etcd, Admin API, dry-run).
pub fn validate_stores(stores: &[StoreConfig]) -> Result<(), String> {
    let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
    for s in stores {
        if s.name.trim().is_empty() {
            return Err("store with empty name".to_string());
        }
        if !seen.insert(s.name.as_str()) {
            return Err(format!("Duplicate store name '{}'", s.name));
        }
        match s.store_type.as_str() {
            "redis" | "valkey" => {}
            other => {
                return Err(format!(
                    "store '{}': unknown type '{}' — supported: redis, valkey",
                    s.name, other
                ))
            }
        }
        if let Some(t) = s.topology.as_deref() {
            if t != "standalone" {
                return Err(format!(
                    "store '{}': topology '{}' is not yet supported (v1 supports standalone only)",
                    s.name, t
                ));
            }
        }
        if s.urls.is_some() {
            return Err(format!(
                "store '{}': 'urls' requires a sentinel/cluster topology, which is not yet supported — use 'url'",
                s.name
            ));
        }
        if s.url.trim().is_empty() {
            return Err(format!("store '{}': url must not be empty", s.name));
        }
    }
    Ok(())
}

/// One client per named store, plus the counter backend built on it.
/// Read-only after construction; replaced wholesale via the `ArcSwap` in
/// `PluginResources` when config changes.
#[derive(Default)]
pub struct StoreRegistry {
    #[cfg(feature = "redis-store")]
    clients: HashMap<String, Arc<redis_store::RedisStoreClient>>,
    #[cfg(feature = "redis-store")]
    counters: HashMap<String, Arc<dyn CounterStore>>,
    // Keeps the struct non-empty (and the imports used) in headless builds.
    #[cfg(not(feature = "redis-store"))]
    _headless: std::marker::PhantomData<(HashMap<(), ()>, fn() -> Arc<dyn CounterStore>)>,
}

impl StoreRegistry {
    /// Builds a registry for `stores`, reusing `prev`'s client wherever a
    /// store's resolved config fingerprint is unchanged, so unrelated config
    /// reloads keep established connections.
    #[cfg(feature = "redis-store")]
    pub fn rebuild(prev: &StoreRegistry, stores: &[StoreConfig]) -> Result<StoreRegistry, String> {
        let mut clients = HashMap::new();
        let counters = HashMap::new();
        for cfg in stores {
            let fingerprint = redis_store::RedisStoreClient::fingerprint_of(cfg);
            let client = match prev.clients.get(&cfg.name) {
                Some(existing) if existing.fingerprint() == fingerprint => existing.clone(),
                _ => Arc::new(redis_store::RedisStoreClient::build(cfg)?),
            };
            clients.insert(cfg.name.clone(), client);
        }
        Ok(StoreRegistry { clients, counters })
    }

    /// Headless build: any declared store is a configuration error.
    #[cfg(not(feature = "redis-store"))]
    pub fn rebuild(_prev: &StoreRegistry, stores: &[StoreConfig]) -> Result<StoreRegistry, String> {
        if stores.is_empty() {
            Ok(StoreRegistry::default())
        } else {
            Err(format!(
                "gateway config declares {} store(s) but this binary was built without the redis-store feature",
                stores.len()
            ))
        }
    }

    /// Resolves the counter backend for a named store; the error carries the
    /// declared-store list so a typo is self-explanatory.
    #[cfg(feature = "redis-store")]
    pub fn counter_store(&self, name: &str) -> Result<Arc<dyn CounterStore>, String> {
        self.counters.get(name).cloned().ok_or_else(|| {
            let mut names: Vec<&str> = self.clients.keys().map(String::as_str).collect();
            names.sort_unstable();
            format!(
                "unknown store '{}' — declared stores: {}",
                name,
                if names.is_empty() {
                    "(none)".to_string()
                } else {
                    names.join(", ")
                }
            )
        })
    }

    #[cfg(not(feature = "redis-store"))]
    pub fn counter_store(&self, name: &str) -> Result<Arc<dyn CounterStore>, String> {
        Err(format!(
            "store '{}': this binary was built without the redis-store feature",
            name
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store(name: &str, ty: &str) -> StoreConfig {
        serde_yaml::from_str(&format!(
            "name: {name}\ntype: {ty}\nurl: redis://127.0.0.1:6379\n"
        ))
        .unwrap()
    }

    #[test]
    fn test_validate_stores_rules() {
        assert!(validate_stores(&[]).is_ok());
        assert!(validate_stores(&[store("a", "redis"), store("b", "valkey")]).is_ok());

        let err = validate_stores(&[store("a", "redis"), store("a", "redis")]).unwrap_err();
        assert!(err.contains("Duplicate store name 'a'"), "{err}");

        let err = validate_stores(&[store("a", "memcached")]).unwrap_err();
        assert!(err.contains("unknown type 'memcached'"), "{err}");

        let mut s = store("a", "redis");
        s.topology = Some("cluster".to_string());
        let err = validate_stores(&[s]).unwrap_err();
        assert!(err.contains("not yet supported"), "{err}");

        let mut s = store("a", "redis");
        s.urls = Some(vec!["redis://x".to_string()]);
        let err = validate_stores(&[s]).unwrap_err();
        assert!(err.contains("'urls' requires"), "{err}");

        let mut s = store("a", "redis");
        s.url = String::new();
        let err = validate_stores(&[s]).unwrap_err();
        assert!(err.contains("url must not be empty"), "{err}");
    }

    #[test]
    fn test_counter_store_unknown_name_lists_declared() {
        let reg = StoreRegistry::default();
        let err = reg.counter_store("nope").unwrap_err();
        // Exact wording differs by build flavor; both name the store.
        assert!(err.contains("'nope'"), "{err}");
    }
}
```

The `redis_store` module referenced above gets its real implementation in Task 3. To keep THIS task's default build green, write `src/stores/mod.rs` exactly as shown (feature-gated code included) and create `src/stores/redis_store.rs` as a compiling stub whose `build` always errors — so a config that declares a store fails loudly instead of pretending to work until Task 3 replaces the stub:

```rust
//! Redis/Valkey client for named stores. Fleshed out incrementally:
//! client + ping (Task 3), fixed-window counter (Task 4).

use crate::config::StoreConfig;

/// Placeholder until the `redis` dependency lands (next task): building any
/// store fails loudly rather than pretending to connect.
pub struct RedisStoreClient {
    fingerprint: String,
}

impl RedisStoreClient {
    pub fn build(cfg: &StoreConfig) -> Result<Self, String> {
        Err(format!(
            "store '{}': redis client not implemented yet (plan task 3)",
            cfg.name
        ))
    }

    pub fn fingerprint_of(_cfg: &StoreConfig) -> String {
        String::new()
    }

    pub fn fingerprint(&self) -> &str {
        &self.fingerprint
    }
}
```

In `src/main.rs`, add `mod stores;` to the module list (alphabetical, after `mod state;`).

- [ ] **Step 3: Run the new tests — verify they pass**

Run: `cargo test test_validate_stores_rules -- --exact && cargo test test_counter_store_unknown_name_lists_declared -- --exact`
Expected: PASS (both). Also run `cargo check --no-default-features` — Expected: PASS (headless variant compiles).

- [ ] **Step 4: Write the failing compile-integration test**

In `src/state.rs` tests (bottom of file, after the existing env-resolution test ~line 319):

```rust
    /// `stores:` validation runs on every compile: duplicates are rejected
    /// with the running config left intact, and the stored config keeps raw
    /// `${...}` placeholders (the security invariant shared with routes).
    #[tokio::test]
    async fn test_stores_validated_at_compile_and_kept_raw() {
        let system: crate::config::SystemConfig = serde_yaml::from_str("{}").unwrap();
        let gw: crate::config::GatewayConfig = serde_yaml::from_str(
            "stores:\n  - name: s1\n    type: redis\n    url: ${STORE_TEST_URL:-redis://127.0.0.1:6379}\n",
        )
        .unwrap();
        let state = SharedState::new(
            system,
            gw,
            None,
            std::sync::Arc::new(crate::config_store::FileConfigStore::new(
                std::path::PathBuf::from("gateway.yaml"),
            )),
        )
        .unwrap();

        // Stored config still holds the placeholder.
        let gw = state.gateway.read().await;
        assert_eq!(gw.stores[0].url, "${STORE_TEST_URL:-redis://127.0.0.1:6379}");
        drop(gw);

        // A candidate with a duplicate store name is rejected...
        let bad: crate::config::GatewayConfig = serde_yaml::from_str(
            "stores:\n  - name: d\n    type: redis\n    url: redis://a\n  - name: d\n    type: redis\n    url: redis://b\n",
        )
        .unwrap();
        let err = state.apply_gateway(bad).await.unwrap_err();
        assert!(err.contains("Duplicate store name 'd'"), "{err}");

        // ...and the last-good config keeps serving.
        assert_eq!(state.gateway.read().await.stores.len(), 1);
    }
```

NOTE for the headless-aware executor: with `redis-store` ON (default test run), `SharedState::new` above will currently FAIL because the stub `RedisStoreClient::build` errors. That is the *expected failure* for this step — the assertion flow is completed in Task 3. For THIS task, make the test pass by asserting the stub's error instead: replace the `.unwrap()` on `SharedState::new(...)` with:

```rust
        let result = SharedState::new(
            system,
            gw,
            None,
            std::sync::Arc::new(crate::config_store::FileConfigStore::new(
                std::path::PathBuf::from("gateway.yaml"),
            )),
        );
        // Until the real client lands (Task 3), declaring a store fails loudly.
        let err = result.err().expect("stub client must reject stores");
        assert!(err.contains("not implemented yet"), "{err}");
        return;
```

(and keep the rest of the test body after `return;` — Task 3 deletes the early return and restores the full flow. Add a `#[allow(unreachable_code)]` on the test fn to keep clippy quiet for one task.)

- [ ] **Step 5: Run it to verify it fails**

Run: `cargo test test_stores_validated_at_compile_and_kept_raw -- --exact`
Expected: FAIL — `SharedState::new` succeeds today because `compile_routes` ignores `gw.stores`.

- [ ] **Step 6: Wire the registry into `PluginResources` and `compile_routes`**

`src/plugins/resources.rs` — add the field and construction (the file already has `use arc_swap::ArcSwap;` for `consumers`):

```rust
    /// Named shared stores (redis/valkey), swapped on config (re)compile.
    /// Plugins resolve a store by name at construction time and hold the
    /// resulting `Arc` — nothing reads this on the request path.
    pub stores: ArcSwap<crate::stores::StoreRegistry>,
```

and in `PluginResources::new`:

```rust
            stores: ArcSwap::from_pointee(crate::stores::StoreRegistry::default()),
```

`src/state.rs` — at the top of `compile_routes` (before the existing plugin-config resolution, ~line 164), insert the validate + swap-for-compile dance, and wrap the existing body so failure restores the previous registry. Refactor mechanically:

```rust
    fn compile_routes(
        gw: &GatewayConfig,
        resources: &Arc<PluginResources>,
    ) -> Result<Vec<(RouteConfig, Arc<CompiledGraph>)>, String> {
        crate::stores::validate_stores(&gw.stores)?;
        // Swap the candidate store registry in for the duration of the
        // compile (plugins resolve `store:` names at construction). The
        // registry is never read on the request path, so this transient swap
        // cannot affect in-flight traffic; on failure the previous registry
        // is restored, preserving the last-good invariant.
        let prev = resources.stores.load_full();
        let candidate =
            crate::stores::StoreRegistry::rebuild(&prev, &gw.stores).inspect_err(|_| {})?;
        resources.stores.store(Arc::new(candidate));
        let result = Self::compile_routes_inner(gw, resources);
        if result.is_err() {
            resources.stores.store(prev);
        }
        result
    }

    fn compile_routes_inner(
        gw: &GatewayConfig,
        resources: &Arc<PluginResources>,
    ) -> Result<Vec<(RouteConfig, Arc<CompiledGraph>)>, String> {
        // ... the ENTIRE existing body of compile_routes, moved verbatim ...
    }
```

(Keep the existing doc comment on `compile_routes`; give `compile_routes_inner` a one-liner: `/// The pre-stores compile body; called with the candidate registry already swapped in.`)

- [ ] **Step 7: Run tests to verify they pass**

Run: `cargo test test_stores_validated_at_compile_and_kept_raw -- --exact && cargo test`
Expected: PASS, full suite green.

- [ ] **Step 8: Commit**

```bash
git add src/stores/ src/main.rs src/plugins/resources.rs src/state.rs Cargo.toml
git commit -m "feat(stores): store registry, validation, and compile-time wiring"
```

---

### Task 3: `redis` dependency + real `RedisStoreClient`

**Files:**
- Modify: `Cargo.toml` (dependency + feature edge)
- Rewrite: `src/stores/redis_store.rs` (replace the stub)
- Modify: `src/state.rs` (restore the full Task-2 test)

**Interfaces:**
- Consumes: `StoreConfig`, `StoreTlsConfig` (Task 1); `crate::config::interpolate_env` (`src/config/loader.rs:27`, `pub fn interpolate_env(input: &str) -> String`).
- Produces (Tasks 4, 9, and Plan 2 consume):
  - `RedisStoreClient::build(cfg: &StoreConfig) -> Result<Self, String>` (resolves env, parses URL, NO network I/O)
  - `RedisStoreClient::conn(&self) -> Result<redis::aio::ConnectionManager, String>` (async; lazy, cached, auto-reconnecting)
  - `RedisStoreClient::ping(&self) -> Result<PingInfo, String>` (async), `pub struct PingInfo { pub latency_ms: u64, pub version: String }`
  - `RedisStoreClient::{name, key_prefix, fingerprint}(&self) -> &str`, `RedisStoreClient::connect_timeout(&self) -> Duration`
  - `RedisStoreClient::fingerprint_of(cfg: &StoreConfig) -> String`

- [ ] **Step 1: Add the dependency**

In `Cargo.toml` `[dependencies]` (near the other optional deps, house comment style):

```toml
# Redis/Valkey client for the `stores:` resource (`redis-store` feature).
# default-features = false + rustls: the tree must stay ring-only — after any
# version bump, verify `cargo tree -i aws-lc-sys` prints nothing.
redis = { version = "0.32", optional = true, default-features = false, features = ["tokio-rustls-comp", "connection-manager", "script"] }
```

and change the feature edge:

```toml
redis-store = ["dep:redis"]
```

- [ ] **Step 2: Verify the tree stays ring-only**

Run: `cargo build && cargo tree -i aws-lc-sys`
Expected: build OK; `cargo tree -i aws-lc-sys` prints `error: package ID specification ... did not match any packages` (i.e. **absent**). If aws-lc-sys DOES appear: check which `redis` feature pulled it (`cargo tree -i aws-lc-sys -e features`), and switch to the minimal rustls feature combination for the pinned version that avoids it (consult docs.rs/redis for the exact names; `tls-rustls` without the `-native-certs`/`-insecure` variants is the usual fix). Do not proceed with aws-lc-sys in the tree.
(If the pinned `0.32` does not exist or feature names differ, use the latest 0.x on crates.io and adapt the three feature names — the API used below is stable across recent versions.)

- [ ] **Step 3: Write the failing client unit tests**

Replace `src/stores/redis_store.rs` entirely:

```rust
//! Redis/Valkey client for named stores (`redis-store` feature).
//!
//! One [`RedisStoreClient`] per `stores:` entry: env placeholders in `url` /
//! `password` resolve here (point of use — the stored config stays raw), the
//! underlying connection is a single auto-reconnecting multiplexed
//! `ConnectionManager` created lazily on first use, and a resolved-config
//! fingerprint lets [`super::StoreRegistry::rebuild`] keep the connection
//! across unrelated config reloads.

use std::time::Duration;

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use redis::IntoConnectionInfo;
use ring::digest::{digest, SHA256};
use tokio::sync::OnceCell;

use crate::config::interpolate_env;
use crate::config::StoreConfig;

/// Result of a connectivity check (`POST /api/stores/{name}/ping`).
pub struct PingInfo {
    pub latency_ms: u64,
    /// `valkey_version` when the server is Valkey, else `redis_version`.
    pub version: String,
}

pub struct RedisStoreClient {
    name: String,
    key_prefix: String,
    fingerprint: String,
    connect_timeout: Duration,
    client: redis::Client,
    conn: OnceCell<redis::aio::ConnectionManager>,
}

impl RedisStoreClient {
    /// Builds the client: resolves `${ENV}` in url/password, parses the URL,
    /// loads the CA bundle if configured. **No network I/O** — connection is
    /// deferred to [`Self::conn`], so config apply never blocks on a store.
    pub fn build(cfg: &StoreConfig) -> Result<Self, String> {
        let url = interpolate_env(&cfg.url);
        if url.trim().is_empty() {
            return Err(format!(
                "store '{}': url resolved to an empty string",
                cfg.name
            ));
        }
        let mut info = url
            .as_str()
            .into_connection_info()
            .map_err(|e| format!("store '{}': invalid url: {}", cfg.name, e))?;
        if let Some(pw) = cfg.password.as_deref() {
            let pw = interpolate_env(pw);
            if !pw.is_empty() {
                info.redis.password = Some(pw);
            }
        }
        let client = match cfg.tls.as_ref().and_then(|t| t.ca_cert_path.as_deref()) {
            Some(path) => {
                let pem = std::fs::read(path).map_err(|e| {
                    format!("store '{}': cannot read ca_cert_path '{}': {}", cfg.name, path, e)
                })?;
                redis::Client::build_with_tls(
                    info,
                    redis::TlsCertificates {
                        client_tls: None,
                        root_cert: Some(pem),
                    },
                )
                .map_err(|e| format!("store '{}': tls setup: {}", cfg.name, e))?
            }
            None => redis::Client::open(info)
                .map_err(|e| format!("store '{}': {}", cfg.name, e))?,
        };
        Ok(Self {
            name: cfg.name.clone(),
            key_prefix: cfg.key_prefix.clone(),
            fingerprint: Self::fingerprint_of(cfg),
            connect_timeout: Duration::from_millis(cfg.connect_timeout_ms),
            client,
            conn: OnceCell::new(),
        })
    }

    /// Fingerprint over the *resolved* connection-relevant fields, so a
    /// changed env var (not just changed YAML) rebuilds the client on the
    /// next reload. Hashed so no secret sits in an easily-dumped string.
    pub fn fingerprint_of(cfg: &StoreConfig) -> String {
        let material = format!(
            "{}|{}|{}|{}|{}",
            interpolate_env(&cfg.url),
            cfg.password.as_deref().map(interpolate_env).unwrap_or_default(),
            cfg.key_prefix,
            cfg.connect_timeout_ms,
            cfg.tls
                .as_ref()
                .and_then(|t| t.ca_cert_path.as_deref())
                .unwrap_or(""),
        );
        BASE64.encode(digest(&SHA256, material.as_bytes()))
    }

    /// The shared multiplexed connection; established on first use and
    /// auto-reconnecting thereafter.
    pub async fn conn(&self) -> Result<redis::aio::ConnectionManager, String> {
        let manager = self
            .conn
            .get_or_try_init(|| async {
                let cfg = redis::aio::ConnectionManagerConfig::new()
                    .set_connection_timeout(self.connect_timeout)
                    .set_response_timeout(self.connect_timeout);
                redis::aio::ConnectionManager::new_with_config(self.client.clone(), cfg).await
            })
            .await
            .map_err(|e| format!("store '{}': connect: {}", self.name, e))?;
        Ok(manager.clone())
    }

    /// `PING` + server version, for the Admin API connectivity check.
    pub async fn ping(&self) -> Result<PingInfo, String> {
        let mut conn = self.conn().await?;
        let start = std::time::Instant::now();
        let pong: String = redis::cmd("PING")
            .query_async(&mut conn)
            .await
            .map_err(|e| format!("store '{}': ping: {}", self.name, e))?;
        if pong != "PONG" {
            return Err(format!("store '{}': unexpected PING reply '{}'", self.name, pong));
        }
        let latency_ms = start.elapsed().as_millis() as u64;
        let info: String = redis::cmd("INFO")
            .arg("server")
            .query_async(&mut conn)
            .await
            .unwrap_or_default();
        let version = info
            .lines()
            .find_map(|l| {
                l.strip_prefix("valkey_version:")
                    .or_else(|| l.strip_prefix("redis_version:"))
            })
            .unwrap_or("unknown")
            .trim()
            .to_string();
        Ok(PingInfo { latency_ms, version })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn key_prefix(&self) -> &str {
        &self.key_prefix
    }

    pub fn fingerprint(&self) -> &str {
        &self.fingerprint
    }

    pub fn connect_timeout(&self) -> Duration {
        self.connect_timeout
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(yaml: &str) -> StoreConfig {
        serde_yaml::from_str(yaml).unwrap()
    }

    /// Env placeholders resolve at build; the config object stays raw; a
    /// changed env var changes the fingerprint.
    #[test]
    fn test_build_resolves_env_and_fingerprints() {
        std::env::set_var("STORE_T3_URL", "redis://127.0.0.1:6399");
        let c = cfg("name: s1\ntype: redis\nurl: ${STORE_T3_URL}\n");
        let built = RedisStoreClient::build(&c).unwrap();
        assert_eq!(built.name(), "s1");
        assert_eq!(built.key_prefix(), "fb");
        // Raw config untouched.
        assert_eq!(c.url, "${STORE_T3_URL}");

        let fp1 = RedisStoreClient::fingerprint_of(&c);
        std::env::set_var("STORE_T3_URL", "redis://127.0.0.1:6400");
        let fp2 = RedisStoreClient::fingerprint_of(&c);
        assert_ne!(fp1, fp2, "resolved env change must change the fingerprint");
        std::env::remove_var("STORE_T3_URL");
    }

    #[test]
    fn test_build_rejects_bad_url() {
        let c = cfg("name: s1\ntype: redis\nurl: 'not a url'\n");
        let err = RedisStoreClient::build(&c).unwrap_err();
        assert!(err.contains("store 's1'"), "{err}");
    }

    /// Live-backend test; skipped unless FEATHERBIT_TEST_REDIS_URL is set
    /// (e.g. `docker run -p 6379:6379 redis:7` then
    /// FEATHERBIT_TEST_REDIS_URL=redis://127.0.0.1:6379 cargo test).
    #[tokio::test]
    async fn test_ping_live() {
        let Ok(url) = std::env::var("FEATHERBIT_TEST_REDIS_URL") else {
            eprintln!("skipping test_ping_live: FEATHERBIT_TEST_REDIS_URL not set");
            return;
        };
        let c = cfg(&format!("name: live\ntype: redis\nurl: {url}\n"));
        let client = RedisStoreClient::build(&c).unwrap();
        let info = client.ping().await.unwrap();
        assert!(!info.version.is_empty());
    }
}
```

Also in `src/stores/mod.rs`: rename the module reference from the stub name if you used one (`pub mod redis_store;` stays; the stub file is replaced wholesale).

- [ ] **Step 4: Restore the full compile-integration test from Task 2**

In `src/state.rs`, delete the stub-error early-return block (and the `#[allow(unreachable_code)]`) from `test_stores_validated_at_compile_and_kept_raw`, restoring the original `.unwrap()` flow so the test now asserts: state builds with a declared store, placeholder stays raw, duplicate-name candidate rejected, last-good preserved.

- [ ] **Step 5: Run tests**

Run: `cargo test test_build_resolves_env_and_fingerprints -- --exact && cargo test test_build_rejects_bad_url -- --exact && cargo test test_stores_validated_at_compile_and_kept_raw -- --exact && cargo test && cargo check --no-default-features`
Expected: all PASS (`test_ping_live` self-skips without the env var).

Optional local verification with a live backend (also proves the Valkey alias):

```bash
docker run -d --rm -p 16379:6379 --name fb-test-redis redis:7
FEATHERBIT_TEST_REDIS_URL=redis://127.0.0.1:16379 cargo test test_ping_live -- --exact
docker stop fb-test-redis
```

- [ ] **Step 6: Commit**

```bash
git add Cargo.toml Cargo.lock src/stores/ src/state.rs
git commit -m "feat(stores): redis/valkey client with lazy connection and env-resolved fingerprint"
```

---

### Task 4: `RedisCounterStore` + error metric

**Files:**
- Create: `src/stores/counter.rs`
- Modify: `src/stores/mod.rs` (module decl; `rebuild` gains counters + metrics param)
- Modify: `src/state.rs` (one-line call-site update)
- Modify: `src/metrics/mod.rs` (new `IntCounterVec`)

**Interfaces:**
- Consumes: `RedisStoreClient` (Task 3); `CounterStore` trait — `src/ratelimit/mod.rs:42-52`: `async fn incr_fixed_window(&self, key: &str, limit: u64, window: Duration) -> Result<WindowResult, CounterError>`; `WindowResult { allowed: bool, remaining: u64, reset: Duration, limit: u64 }`; `CounterError(pub String)`.
- Produces: `StoreRegistry::rebuild(prev, stores, metrics: Option<Arc<GatewayMetrics>>)` (signature change), working `StoreRegistry::counter_store(name)`; `GatewayMetrics.counter_store_errors: IntCounterVec` (`gateway_counter_store_errors_total{store}`).

- [ ] **Step 1: Write the failing window-math unit test**

Create `src/stores/counter.rs`:

```rust
//! Redis-backed fixed-window counter (`policy: redis` for limit-count and
//! the workflow limit-count action).
//!
//! Increment-and-check runs as one server-side Lua script so concurrent
//! gateway instances count atomically. Window boundaries are wall-clock
//! aligned (`now / window`), so every instance agrees on them — unlike the
//! local store's per-process `Instant` windows. Backend errors bump
//! `gateway_counter_store_errors_total{store}` and surface as
//! [`CounterError`]; the calling plugin decides fail-open vs reject.

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;

use crate::metrics::GatewayMetrics;
use crate::ratelimit::{CounterError, CounterStore, WindowResult};

use super::redis_store::RedisStoreClient;

/// `INCR` + first-increment `PEXPIRE` + `PTTL`, atomically.
const FIXED_WINDOW_SCRIPT: &str = r#"
local current = redis.call('INCR', KEYS[1])
if current == 1 then
  redis.call('PEXPIRE', KEYS[1], ARGV[1])
end
local ttl = redis.call('PTTL', KEYS[1])
return {current, ttl}
"#;

/// Wall-clock window slot: the window index (key component shared by all
/// instances) and the milliseconds from `now_ms` to the window's end (the
/// PEXPIRE argument). Pure, so the boundary math is unit-testable.
fn window_slot(now_ms: u64, window_ms: u64) -> (u64, u64) {
    let window_ms = window_ms.max(1);
    let start = now_ms / window_ms;
    let expire_ms = (start + 1) * window_ms - now_ms;
    (start, expire_ms)
}

pub struct RedisCounterStore {
    client: Arc<RedisStoreClient>,
    store_name: String,
    metrics: Option<Arc<GatewayMetrics>>,
    script: redis::Script,
}

impl RedisCounterStore {
    pub fn new(
        client: Arc<RedisStoreClient>,
        store_name: String,
        metrics: Option<Arc<GatewayMetrics>>,
    ) -> Self {
        Self {
            client,
            store_name,
            metrics,
            script: redis::Script::new(FIXED_WINDOW_SCRIPT),
        }
    }

    fn backend_err(&self, msg: String) -> CounterError {
        if let Some(ref m) = self.metrics {
            m.counter_store_errors
                .with_label_values(&[&self.store_name])
                .inc();
        }
        tracing::warn!(store = %self.store_name, "counter store error: {}", msg);
        CounterError(msg)
    }
}

#[async_trait]
impl CounterStore for RedisCounterStore {
    async fn incr_fixed_window(
        &self,
        key: &str,
        limit: u64,
        window: Duration,
    ) -> Result<WindowResult, CounterError> {
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        let (slot, expire_ms) = window_slot(now_ms, window.as_millis() as u64);
        let redis_key = format!("{}:cnt:{}:{}", self.client.key_prefix(), slot, key);

        let mut conn = self.client.conn().await.map_err(|e| self.backend_err(e))?;
        let (count, pttl): (u64, i64) = self
            .script
            .key(&redis_key)
            .arg(expire_ms)
            .invoke_async(&mut conn)
            .await
            .map_err(|e| self.backend_err(format!("fixed-window script: {}", e)))?;

        Ok(WindowResult {
            allowed: count <= limit,
            remaining: limit.saturating_sub(count),
            reset: Duration::from_millis(pttl.max(0) as u64),
            limit,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Window slots are wall-clock aligned and expiry lands exactly on the
    /// window boundary — the property that makes limits cluster-consistent.
    #[test]
    fn test_window_slot_alignment() {
        // 10s window: 25_000ms is 5s into slot 2, 5s left.
        assert_eq!(window_slot(25_000, 10_000), (2, 5_000));
        // Exactly on a boundary: full window remains.
        assert_eq!(window_slot(30_000, 10_000), (3, 10_000));
        // 1ms before the boundary.
        assert_eq!(window_slot(29_999, 10_000), (2, 1));
        // Degenerate zero window is clamped, never divides by zero.
        assert_eq!(window_slot(5, 0), (5, 1));
    }

    /// Live-backend atomicity test; skipped unless FEATHERBIT_TEST_REDIS_URL
    /// is set. N concurrent tasks never over-admit past the limit.
    #[tokio::test]
    async fn test_concurrent_increments_never_exceed_limit_live() {
        let Ok(url) = std::env::var("FEATHERBIT_TEST_REDIS_URL") else {
            eprintln!(
                "skipping test_concurrent_increments_never_exceed_limit_live: FEATHERBIT_TEST_REDIS_URL not set"
            );
            return;
        };
        let cfg: crate::config::StoreConfig = serde_yaml::from_str(&format!(
            "name: live\ntype: redis\nurl: {url}\nkey_prefix: fbtest{}\n",
            std::process::id()
        ))
        .unwrap();
        let client = Arc::new(RedisStoreClient::build(&cfg).unwrap());
        let store = Arc::new(RedisCounterStore::new(client, "live".to_string(), None));

        let limit = 10u64;
        let window = Duration::from_secs(60);
        let mut handles = Vec::new();
        for _ in 0..40 {
            let store = store.clone();
            handles.push(tokio::spawn(async move {
                store
                    .incr_fixed_window("conc-key", limit, window)
                    .await
                    .unwrap()
                    .allowed
            }));
        }
        let mut admitted = 0;
        for h in handles {
            if h.await.unwrap() {
                admitted += 1;
            }
        }
        assert_eq!(admitted as u64, limit, "exactly `limit` requests admitted");
    }
}
```

- [ ] **Step 2: Run the pure test to verify it fails to compile, then wire the module**

Run: `cargo test test_window_slot_alignment -- --exact`
Expected: COMPILE FAIL (module not declared; metric field missing).

In `src/stores/mod.rs`: add `#[cfg(feature = "redis-store")] pub mod counter;` next to the `redis_store` decl, and extend `rebuild` (feature-on variant) to build counters — replace `let counters = HashMap::new();` and the loop body with:

```rust
        let mut counters: HashMap<String, Arc<dyn CounterStore>> = HashMap::new();
        for cfg in stores {
            let fingerprint = redis_store::RedisStoreClient::fingerprint_of(cfg);
            let client = match prev.clients.get(&cfg.name) {
                Some(existing) if existing.fingerprint() == fingerprint => existing.clone(),
                _ => Arc::new(redis_store::RedisStoreClient::build(cfg)?),
            };
            counters.insert(
                cfg.name.clone(),
                Arc::new(counter::RedisCounterStore::new(
                    client.clone(),
                    cfg.name.clone(),
                    metrics.clone(),
                )) as Arc<dyn CounterStore>,
            );
            clients.insert(cfg.name.clone(), client);
        }
```

Change BOTH `rebuild` signatures (feature-on and headless) to:

```rust
    pub fn rebuild(
        prev: &StoreRegistry,
        stores: &[StoreConfig],
        metrics: Option<Arc<crate::metrics::GatewayMetrics>>,
    ) -> Result<StoreRegistry, String>
```

(headless variant names it `_metrics`). Update the single call site in `src/state.rs` `compile_routes`:

```rust
        let candidate =
            crate::stores::StoreRegistry::rebuild(&prev, &gw.stores, resources.metrics.clone())?;
```

(remove the leftover `.inspect_err(|_| {})` from Task 2 while here).

In `src/metrics/mod.rs`: add the field to `GatewayMetrics`:

```rust
    /// Counter-store (stores:) backend errors, per named store.
    pub counter_store_errors: IntCounterVec,
```

register it in `GatewayMetrics::new` following the existing pattern (`src/metrics/mod.rs:102-125`):

```rust
        let counter_store_errors = IntCounterVec::new(
            Opts::new(
                "gateway_counter_store_errors_total",
                "Total counter-store backend errors per named store",
            ),
            &["store"],
        )
        .unwrap();
```

plus `registry.register(Box::new(counter_store_errors.clone())).unwrap();` and the struct-literal entry.

- [ ] **Step 3: Run tests to verify they pass**

Run: `cargo test test_window_slot_alignment -- --exact && cargo test && cargo check --no-default-features`
Expected: PASS / green / green. With a live backend (optional, recommended once):

```bash
docker run -d --rm -p 16379:6379 --name fb-test-redis redis:7
FEATHERBIT_TEST_REDIS_URL=redis://127.0.0.1:16379 cargo test test_concurrent_increments_never_exceed_limit_live -- --exact
docker stop fb-test-redis
```
Expected: PASS — exactly 10 of 40 concurrent requests admitted.

- [ ] **Step 4: Commit**

```bash
git add src/stores/ src/state.rs src/metrics/mod.rs
git commit -m "feat(stores): redis fixed-window counter with clock-aligned windows and error metric"
```

---

### Task 5: `limit-count` gains `policy: redis` + `store:`

**Files:**
- Modify: `src/plugins/native/limit_count.rs` (`from_config` ~:119-125; tests ~:308-330)
- Modify: `src/ratelimit/mod.rs` (module doc :5-8 wording + `test_registry_lookup` comment)

**Interfaces:**
- Consumes: `resources.stores.load().counter_store(name)` (Tasks 2/4).
- Produces: config contract `policy: local|redis` + `store: <name>` (required iff redis) — the workflow task and all docs rely on these exact keys and error strings.

- [ ] **Step 1: Write the failing config tests**

In `src/plugins/native/limit_count.rs` tests, REPLACE `test_unknown_policy_rejected` (~line 317) with:

```rust
    /// `policy: redis` requires `store:`; a policy name that is neither
    /// `local` nor `redis` is rejected with the supported list.
    #[test]
    fn test_redis_policy_requires_store() {
        let err = plugin(serde_json::json!({
            "count": 1, "time_window": 60, "policy": "redis"
        }))
        .unwrap_err();
        assert!(err.contains("requires 'store'"), "{err}");
    }

    #[test]
    fn test_unknown_policy_rejected() {
        let err = plugin(serde_json::json!({
            "count": 1, "time_window": 60, "policy": "memcached"
        }))
        .unwrap_err();
        assert!(err.contains("unknown rate-limit policy"), "{err}");
    }

    /// `store:` must name a declared store; the error lists what exists.
    #[cfg(feature = "redis-store")]
    #[test]
    fn test_redis_policy_unknown_store_rejected() {
        let err = plugin(serde_json::json!({
            "count": 1, "time_window": 60, "policy": "redis", "store": "nope"
        }))
        .unwrap_err();
        assert!(err.contains("unknown store 'nope'"), "{err}");
    }
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test test_redis_policy_requires_store -- --exact`
Expected: FAIL — today `policy: redis` errors with `unknown rate-limit policy 'redis'`, not `requires 'store'`.

- [ ] **Step 3: Implement the branch**

Replace `src/plugins/native/limit_count.rs:119-125` (the `policy` parse + `resources.counters.get`) with:

```rust
        let policy = config
            .get("policy")
            .and_then(|v| v.as_str())
            .unwrap_or("local");
        // `redis` resolves a named `stores:` entry (cluster-shared counters);
        // anything else goes through the local registry, which also produces
        // the supported-policy list for unknown names.
        let store = if policy == "redis" {
            let name = config
                .get("store")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .ok_or_else(|| {
                    "limit-count: policy 'redis' requires 'store' naming a declared stores: entry"
                        .to_string()
                })?;
            resources.stores.load().counter_store(name)?
        } else {
            resources.counters.get(policy)?
        };
```

In `src/ratelimit/mod.rs`: update the module doc lines 5-8 — replace the sentence about the planned Redis store with:

```rust
//! per-client key, then ask a [`CounterStore`] to count it within a fixed
//! window. `local` (in-memory, per gateway instance) is always available;
//! `policy: redis` resolves a named `stores:` entry to a cluster-shared
//! counter (`crate::stores::counter::RedisCounterStore`, `redis-store`
//! feature) instead of going through this registry.
```

and in `test_registry_lookup` (~:170-178), change the panic message `"'redis' should not resolve yet"` to `"'redis' is not in this registry (it resolves via stores:)"` — the assertion itself stays.

- [ ] **Step 4: Run tests**

Run: `cargo test test_redis_policy_requires_store -- --exact && cargo test test_unknown_policy_rejected -- --exact && cargo test test_redis_policy_unknown_store_rejected -- --exact && cargo test`
Expected: PASS ×3, suite green.

- [ ] **Step 5: Add the end-to-end compile test (declared store resolves)**

In `src/state.rs` tests, append:

```rust
    /// A limit-count node with `policy: redis` resolves its named store at
    /// compile time: declared store compiles, missing store fails compile.
    #[cfg(feature = "redis-store")]
    #[test]
    fn test_limit_count_redis_store_resolved_at_compile() {
        let policy_yaml = |store_line: &str| {
            format!(
                r#"
{store_line}
policies:
  - name: p
    nodes:
      - id: l
        type: listener
      - id: lc
        type: limit-count
        config: {{ count: 1, time_window: 60, policy: redis, store: s1 }}
      - id: c
        type: client
    edges:
      - from: l.out
        to: lc.in
      - from: lc.success
        to: c.in
      - from: lc.limited
        to: c.in
routes:
  - name: r
    match: {{ path: /x }}
    policy: p
"#
            )
        };
        let system: crate::config::SystemConfig = serde_yaml::from_str("{}").unwrap();
        let with_store: crate::config::GatewayConfig = serde_yaml::from_str(&policy_yaml(
            "stores:\n  - name: s1\n    type: redis\n    url: redis://127.0.0.1:6379",
        ))
        .unwrap();
        crate::state::validate_gateway_config(&system, &with_store)
            .expect("declared store must compile");

        let without_store: crate::config::GatewayConfig =
            serde_yaml::from_str(&policy_yaml("")).unwrap();
        let err = crate::state::validate_gateway_config(&system, &without_store).unwrap_err();
        assert!(err.contains("unknown store 's1'"), "{err}");
    }
```

(If `validate_gateway_config`'s exact signature differs — it is at `src/state.rs:226-230` — adapt the two calls to it; the route `match` key shape can be cribbed from any neighboring state test.)

Run: `cargo test test_limit_count_redis_store_resolved_at_compile -- --exact`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add src/plugins/native/limit_count.rs src/ratelimit/mod.rs src/state.rs
git commit -m "feat(limit-count): redis policy resolves named stores for cluster-shared counters"
```

---

### Task 6: `workflow` limit-count action gains `policy: redis` + `store:`

**Files:**
- Modify: `src/plugins/native/workflow.rs` (store resolution ~:225-229; `test_config_errors` ~:569-574)

**Interfaces:**
- Consumes: same `counter_store(name)` contract as Task 5; error strings prefixed `workflow limit-count:` instead of `limit-count:`.

- [ ] **Step 1: Write the failing test**

In `workflow.rs`'s `test_config_errors` (~:569-574), the existing case asserts `policy: "redis"` fails config load. Replace that case with two:

```rust
        // redis policy without a store name.
        let err = mk(serde_json::json!({
            "rules": [{ "case": [["uri", "==", "/x"]],
                        "actions": [["limit-count", {"count": 1, "time_window": 1, "policy": "redis"}]] }]
        }))
        .unwrap_err();
        assert!(err.contains("requires 'store'"), "{err}");

        // redis policy naming an undeclared store.
        #[cfg(feature = "redis-store")]
        {
            let err = mk(serde_json::json!({
                "rules": [{ "case": [["uri", "==", "/x"]],
                            "actions": [["limit-count", {"count": 1, "time_window": 1, "policy": "redis", "store": "nope"}]] }]
            }))
            .unwrap_err();
            assert!(err.contains("unknown store 'nope'"), "{err}");
        }
```

(`mk` = whatever local helper `test_config_errors` already uses to call `WorkflowPlugin::from_config` — reuse it verbatim; if it's inline `from_config` calls, follow that shape.)

- [ ] **Step 2: Run to verify failure**

Run: `cargo test test_config_errors -- --exact`
Expected: FAIL — assertion on `requires 'store'` (today the error is `unknown rate-limit policy 'redis'`).

- [ ] **Step 3: Implement**

Replace `src/plugins/native/workflow.rs:225-229` with:

```rust
                    let policy = params
                        .get("policy")
                        .and_then(|v| v.as_str())
                        .unwrap_or("local");
                    let store = if policy == "redis" {
                        let name = params
                            .get("store")
                            .and_then(|v| v.as_str())
                            .filter(|s| !s.is_empty())
                            .ok_or_else(|| {
                                "workflow limit-count: policy 'redis' requires 'store' naming a declared stores: entry"
                                    .to_string()
                            })?;
                        resources.stores.load().counter_store(name)?
                    } else {
                        resources.counters.get(policy)?
                    };
```

- [ ] **Step 4: Run tests**

Run: `cargo test test_config_errors -- --exact && cargo test test_lua_ && cargo test`
Expected: PASS, suite green.

- [ ] **Step 5: Commit**

```bash
git add src/plugins/native/workflow.rs
git commit -m "feat(workflow): limit-count action supports redis policy via named stores"
```

---

### Task 7: etcd `stores/` key family

**Files:**
- Modify: `src/config_store/etcd.rs` (all seven edits below)

**Interfaces:**
- Consumes: `StoreConfig` (Task 1).
- Produces: etcd key family `<prefix>/stores/<name>`, value = the store's JSON.

- [ ] **Step 1: Write the failing parse test**

In `src/config_store/etcd.rs` tests (~:480-606, next to the per-resource parse tests), add:

```rust
    /// The stores/ key family round-trips through gateway_from_kvs and
    /// participates in is_empty (which gates seed-from-file).
    #[test]
    fn test_gateway_from_kvs_parses_stores() {
        let kvs = vec![kv(
            "/fb/stores/s1",
            r#"{"name":"s1","type":"redis","url":"redis://127.0.0.1:6379","key_prefix":"fb","connect_timeout_ms":2000}"#,
        )];
        let gw = gateway_from_kvs("/fb", &kvs).unwrap();
        assert_eq!(gw.stores.len(), 1);
        assert_eq!(gw.stores[0].name, "s1");
        assert_eq!(gw.stores[0].store_type, "redis");
        assert!(!is_empty(&gw));

        let err = gateway_from_kvs("/fb", &[kv("/fb/stores/bad", "{notjson")]).unwrap_err();
        assert!(err.contains("bad store 'bad'"), "{err}");
    }
```

(`kv(...)`, `gateway_from_kvs`, `is_empty` — reuse the exact helpers/visibility the neighboring tests use; if `is_empty` is a private method, assert through the same seam those tests do.)

- [ ] **Step 2: Run to verify failure**

Run: `cargo test test_gateway_from_kvs_parses_stores -- --exact`
Expected: FAIL — stores keys are currently ignored by `gateway_from_kvs`, so `gw.stores` is empty.

- [ ] **Step 3: The seven edits**

1. Key builder next to `plugin_config_key` (~:207-209):

```rust
    fn store_key(&self, name: &str) -> String {
        format!("{}/stores/{}", self.prefix, name)
    }
```

2. `write_all` (~:212-240): sixth loop mirroring the plugin_configs one:

```rust
        for s in &gw.stores {
            self.put(&self.store_key(&s.name), &serde_json::to_string(s).unwrap())
                .await?;
        }
```

3. `commit` (~:250-296): sixth put-loop after the plugin_configs block, **including the `desired.insert`** (omitting it makes the stale-key sweep GC the keys just written):

```rust
        for s in &candidate.stores {
            let key = self.store_key(&s.name);
            self.put(&key, &serde_json::to_string(s).unwrap()).await?;
            desired.insert(key);
        }
```

4. `gateway_from_kvs` (~:305-352): `stores: Vec::new(),` is already in the literal (Task 1); add the match arm mirroring the plugin_configs arm (~:343-347):

```rust
            "stores" => {
                let s: StoreConfig = serde_json::from_str(&value)
                    .map_err(|e| format!("bad store '{}': {}", name, e))?;
                gw.stores.push(s);
            }
```

and add `StoreConfig` to the `use crate::config::{...}` import (~:34).

5. `is_empty` (~:369-375): `&& gw.stores.is_empty()`.

6. Docs: add `<prefix>/stores/<name>` to the module doc key list (:6-8) AND to the `gateway_from_kvs` fn doc (:301) — the fn doc was missed once before for supernodes; do both.

7. Test-module `GatewayConfig` literal (~:530-536) already fixed in Task 1 — verify it still lists `stores`.

- [ ] **Step 4: Run tests**

Run: `cargo test test_gateway_from_kvs_parses_stores -- --exact && cargo test etcd`
Expected: PASS, all etcd tests green.

- [ ] **Step 5: Commit**

```bash
git add src/config_store/etcd.rs
git commit -m "feat(etcd): sync stores as a sixth key family"
```

---

### Task 8: Admin API — `/api/stores` CRUD with referrer-guarded delete

**Files:**
- Create: `src/admin/stores.rs`
- Modify: `src/admin/mod.rs:9-20` (`mod stores;`, alphabetical) and the router chain (~:142-150, `.merge(stores::router())` — **before** the `.layer(...)` auth middleware)

**Interfaces:**
- Consumes: the universal admin mutation pattern (read-lock → clone candidate → mutate → `state.config_store.clone().commit(&state, candidate).await`); `StoreConfig`.
- Produces: `GET/POST /api/stores`, `GET/PUT/DELETE /api/stores/{name}`; DELETE returns `409 {"error":"in_use","referrers":[...]}` when referenced. Referrer strings: `policy '<p>' node '<id>'`, `supernode '<s>' node '<id>'`, `plugin_config '<name>'`. (Task 9 adds ping to this same router.)

- [ ] **Step 1: Write the failing tests**

Create `src/admin/stores.rs` with the test module first (handlers stubbed as `todo!()` won't compile cleanly — write the full file in Step 3; here, define the tests you will paste into it):

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{GatewayConfig, SystemConfig};
    use crate::config_store::FileConfigStore;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    fn test_state(gateway_yaml: &str) -> Arc<SharedState> {
        let system: SystemConfig = serde_yaml::from_str("{}").unwrap();
        let gateway: GatewayConfig = serde_yaml::from_str(gateway_yaml).unwrap();
        Arc::new(
            SharedState::new(
                system,
                gateway,
                None,
                Arc::new(FileConfigStore::new(std::path::PathBuf::from(
                    "gateway.yaml",
                ))),
            )
            .unwrap(),
        )
    }

    fn app(state: Arc<SharedState>) -> Router {
        router().with_state(state)
    }

    const VALID_STORE: &str = r#"{
        "name": "s1",
        "type": "redis",
        "url": "${TEST_STORES_URL:-redis://127.0.0.1:6379}"
    }"#;

    async fn send(
        state: &Arc<SharedState>,
        req: Request<Body>,
    ) -> (StatusCode, serde_json::Value) {
        let resp = app(state.clone()).oneshot(req).await.unwrap();
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let v = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
        (status, v)
    }

    #[tokio::test]
    async fn test_crud_roundtrip_keeps_placeholders_raw() {
        let state = test_state("{}");
        let (status, _) = send(
            &state,
            Request::post("/api/stores")
                .header("content-type", "application/json")
                .body(Body::from(VALID_STORE))
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);

        // Duplicate create → 409.
        let (status, _) = send(
            &state,
            Request::post("/api/stores")
                .header("content-type", "application/json")
                .body(Body::from(VALID_STORE))
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);

        // GET serves the RAW placeholder, never a resolved value.
        let (status, body) = send(
            &state,
            Request::get("/api/stores/s1").body(Body::empty()).unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body["url"].as_str().unwrap(),
            "${TEST_STORES_URL:-redis://127.0.0.1:6379}"
        );

        // PUT upserts; invalid type is rejected by commit-time validation.
        let (status, body) = send(
            &state,
            Request::put("/api/stores/s1")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"name":"s1","type":"memcached","url":"redis://x"}"#,
                ))
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(
            body["error"].as_str().unwrap().contains("unknown type"),
            "{body}"
        );

        // DELETE, then 404.
        let (status, _) = send(
            &state,
            Request::delete("/api/stores/s1")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let (status, _) = send(
            &state,
            Request::get("/api/stores/s1").body(Body::empty()).unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn test_delete_referenced_store_is_409_with_referrers() {
        let state = test_state(
            r#"
stores:
  - name: s1
    type: redis
    url: redis://127.0.0.1:6379
plugin_configs:
  - name: shared-lc
    type: limit-count
    config: { count: 1, time_window: 60, policy: redis, store: s1 }
"#,
        );
        let (status, body) = send(
            &state,
            Request::delete("/api/stores/s1")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(body["error"], "in_use");
        let refs: Vec<String> = body["referrers"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect();
        assert!(
            refs.contains(&"plugin_config 'shared-lc'".to_string()),
            "{refs:?}"
        );
    }
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test test_crud_roundtrip_keeps_placeholders_raw -- --exact`
Expected: COMPILE FAIL — no `router()` / handlers exist yet.

- [ ] **Step 3: Implement the handlers**

Top of `src/admin/stores.rs` (above the test module):

```rust
//! Admin CRUD for the `stores:` resource (named redis/valkey connections).
//!
//! Responses always carry the RAW stored config — `${ENV}` placeholders are
//! never resolved here (they resolve only when a client is built). Deleting
//! a store still referenced by any plugin config is rejected with
//! `409 {"error":"in_use","referrers":[...]}` — a new envelope, since the
//! reference is not discoverable via graph recompilation (a store used only
//! by a shared plugin_config compiles fine without the node being wired).

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router};

use crate::config::{GatewayConfig, NodeConfig, StoreConfig};
use crate::state::SharedState;

pub fn router() -> Router<Arc<SharedState>> {
    Router::new()
        .route("/api/stores", get(list_stores).post(create_store))
        .route(
            "/api/stores/{name}",
            get(get_store).put(update_store).delete(delete_store),
        )
}

async fn list_stores(State(state): State<Arc<SharedState>>) -> impl IntoResponse {
    let gw = state.gateway.read().await;
    Json(gw.stores.clone()).into_response()
}

async fn get_store(
    State(state): State<Arc<SharedState>>,
    Path(name): Path<String>,
) -> impl IntoResponse {
    let gw = state.gateway.read().await;
    match gw.stores.iter().find(|s| s.name == name) {
        Some(s) => Json(s.clone()).into_response(),
        None => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "not_found"})),
        )
            .into_response(),
    }
}

async fn create_store(
    State(state): State<Arc<SharedState>>,
    Json(store): Json<StoreConfig>,
) -> impl IntoResponse {
    let candidate = {
        let gw = state.gateway.read().await;
        if gw.stores.iter().any(|s| s.name == store.name) {
            return (
                StatusCode::CONFLICT,
                Json(serde_json::json!({"error": "store already exists"})),
            )
                .into_response();
        }
        let mut candidate = gw.clone();
        candidate.stores.push(store);
        candidate
    };
    match state.config_store.clone().commit(&state, candidate).await {
        Ok(_) => (
            StatusCode::CREATED,
            Json(serde_json::json!({"status": "created"})),
        )
            .into_response(),
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": e})),
        )
            .into_response(),
    }
}

async fn update_store(
    State(state): State<Arc<SharedState>>,
    Path(name): Path<String>,
    Json(mut store): Json<StoreConfig>,
) -> impl IntoResponse {
    store.name = name.clone();
    let candidate = {
        let gw = state.gateway.read().await;
        let mut candidate = gw.clone();
        if let Some(existing) = candidate.stores.iter_mut().find(|s| s.name == name) {
            *existing = store;
        } else {
            candidate.stores.push(store);
        }
        candidate
    };
    match state.config_store.clone().commit(&state, candidate).await {
        Ok(_) => Json(serde_json::json!({"status": "updated"})).into_response(),
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": e})),
        )
            .into_response(),
    }
}

async fn delete_store(
    State(state): State<Arc<SharedState>>,
    Path(name): Path<String>,
) -> impl IntoResponse {
    let candidate = {
        let gw = state.gateway.read().await;
        let referrers = store_referrers(&gw, &name);
        if !referrers.is_empty() {
            return (
                StatusCode::CONFLICT,
                Json(serde_json::json!({"error": "in_use", "referrers": referrers})),
            )
                .into_response();
        }
        let mut candidate = gw.clone();
        let before = candidate.stores.len();
        candidate.stores.retain(|s| s.name != name);
        if candidate.stores.len() == before {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({"error": "not_found"})),
            )
                .into_response();
        }
        candidate
    };
    match state.config_store.clone().commit(&state, candidate).await {
        Ok(_) => Json(serde_json::json!({"status": "deleted"})).into_response(),
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": e})),
        )
            .into_response(),
    }
}

/// Everything that references store `name`: node config `store:` keys (flat,
/// for limit-count/workflow) and nested `session.store` (session plugins —
/// wired in a later plan, scanned now so the guard never lags the feature).
fn store_referrers(gw: &GatewayConfig, name: &str) -> Vec<String> {
    fn config_references(
        config: &std::collections::HashMap<String, serde_json::Value>,
        name: &str,
    ) -> bool {
        config.get("store").and_then(|v| v.as_str()) == Some(name)
            || config
                .get("session")
                .and_then(|v| v.get("store"))
                .and_then(|v| v.as_str())
                == Some(name)
    }
    fn scan_nodes(nodes: &[NodeConfig], owner: &str, name: &str, out: &mut Vec<String>) {
        for n in nodes {
            if config_references(&n.config, name) {
                out.push(format!("{} node '{}'", owner, n.id));
            }
        }
    }
    let mut refs = Vec::new();
    for p in &gw.policies {
        scan_nodes(&p.nodes, &format!("policy '{}'", p.name), name, &mut refs);
    }
    for s in &gw.supernodes {
        scan_nodes(&s.nodes, &format!("supernode '{}'", s.name), name, &mut refs);
    }
    for pc in &gw.plugin_configs {
        if config_references(&pc.config, name) {
            refs.push(format!("plugin_config '{}'", pc.name));
        }
    }
    refs
}
```

(If `NodeConfig`'s field names differ — `id`/`config` are per `src/config/gateway.rs` — the compiler will point at them; fix to match.)

In `src/admin/mod.rs`: add `mod stores;` to the declarations (:9-20, alphabetical) and `.merge(stores::router())` in the router chain BEFORE `.layer(...)`.

- [ ] **Step 4: Run tests**

Run: `cargo test test_crud_roundtrip_keeps_placeholders_raw -- --exact && cargo test test_delete_referenced_store_is_409_with_referrers -- --exact && cargo test`
Expected: PASS ×2, suite green. (The referrer test needs `redis-store` on so the plugin_config's store reference doesn't fail earlier — the default test build has it on; if it fails on the headless matrix later, gate the test with `#[cfg(feature = "redis-store")]`.)

- [ ] **Step 5: Commit**

```bash
git add src/admin/stores.rs src/admin/mod.rs
git commit -m "feat(admin): stores CRUD with referrer-guarded delete"
```

---

### Task 9: Admin API — `POST /api/stores/{name}/ping`

**Files:**
- Modify: `src/admin/stores.rs` (route + handler + tests)

**Interfaces:**
- Consumes: `RedisStoreClient::{build, ping, connect_timeout}` (Task 3).
- Produces: `POST /api/stores/{name}/ping` → `200 {"status":"ok","latency_ms":N,"version":"..."}` | `404 not_found` | `400` (client build failed) | `502 {"error":...}` (unreachable) | `504 ping_timeout` | `501` (headless build).

- [ ] **Step 1: Write the failing tests**

Append to the `src/admin/stores.rs` test module:

```rust
    /// Ping on an unknown store is 404; on an unreachable store it is a 502
    /// or 504 within the configured timeout — never a hang, never a 200.
    #[tokio::test]
    #[cfg(feature = "redis-store")]
    async fn test_ping_unknown_and_unreachable() {
        // Port 1 is reserved/closed; 300ms timeout keeps the test fast.
        let state = test_state(
            "stores:\n  - name: dead\n    type: redis\n    url: redis://127.0.0.1:1\n    connect_timeout_ms: 300\n",
        );
        let (status, _) = send(
            &state,
            Request::post("/api/stores/nope/ping")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);

        let (status, body) = send(
            &state,
            Request::post("/api/stores/dead/ping")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert!(
            status == StatusCode::BAD_GATEWAY || status == StatusCode::GATEWAY_TIMEOUT,
            "{status} {body}"
        );
    }

    /// Live ping; skipped unless FEATHERBIT_TEST_REDIS_URL is set.
    #[tokio::test]
    #[cfg(feature = "redis-store")]
    async fn test_ping_live() {
        let Ok(url) = std::env::var("FEATHERBIT_TEST_REDIS_URL") else {
            eprintln!("skipping test_ping_live: FEATHERBIT_TEST_REDIS_URL not set");
            return;
        };
        let state = test_state(&format!(
            "stores:\n  - name: live\n    type: redis\n    url: {url}\n"
        ));
        let (status, body) = send(
            &state,
            Request::post("/api/stores/live/ping")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["status"], "ok");
        assert!(body["latency_ms"].is_u64(), "{body}");
        assert!(body["version"].is_string(), "{body}");
    }
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test test_ping_unknown_and_unreachable -- --exact`
Expected: FAIL — 404 for both (route not registered).

- [ ] **Step 3: Implement**

In `router()`, add (a separate route — axum 0.8 will not conflict with `/api/stores/{name}`):

```rust
        .route("/api/stores/{name}/ping", axum::routing::post(ping_store))
```

Handler (both build flavors):

```rust
/// Connectivity check: resolves the store's config (env placeholders included)
/// and PINGs it, bounded by the store's own connect_timeout_ms. The response
/// never echoes resolved connection details — only latency and version.
#[cfg(feature = "redis-store")]
async fn ping_store(
    State(state): State<Arc<SharedState>>,
    Path(name): Path<String>,
) -> impl IntoResponse {
    let cfg = {
        let gw = state.gateway.read().await;
        match gw.stores.iter().find(|s| s.name == name) {
            Some(s) => s.clone(),
            None => {
                return (
                    StatusCode::NOT_FOUND,
                    Json(serde_json::json!({"error": "not_found"})),
                )
                    .into_response()
            }
        }
    };
    let client = match crate::stores::redis_store::RedisStoreClient::build(&cfg) {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": e})),
            )
                .into_response()
        }
    };
    match tokio::time::timeout(client.connect_timeout(), client.ping()).await {
        Err(_) => (
            StatusCode::GATEWAY_TIMEOUT,
            Json(serde_json::json!({
                "error": "ping_timeout",
                "message": format!("no reply within connect_timeout_ms ({}ms)", cfg.connect_timeout_ms),
            })),
        )
            .into_response(),
        Ok(Err(e)) => (
            StatusCode::BAD_GATEWAY,
            Json(serde_json::json!({"error": e})),
        )
            .into_response(),
        Ok(Ok(info)) => Json(serde_json::json!({
            "status": "ok",
            "latency_ms": info.latency_ms,
            "version": info.version,
        }))
        .into_response(),
    }
}

#[cfg(not(feature = "redis-store"))]
async fn ping_store(
    State(_state): State<Arc<SharedState>>,
    Path(_name): Path<String>,
) -> impl IntoResponse {
    (
        StatusCode::NOT_IMPLEMENTED,
        Json(serde_json::json!({
            "error": "this binary was built without the redis-store feature"
        })),
    )
        .into_response()
}
```

- [ ] **Step 4: Run tests**

Run: `cargo test test_ping_unknown_and_unreachable -- --exact && cargo test && cargo check --no-default-features`
Expected: PASS / green / green. Optional live check:

```bash
docker run -d --rm -p 16379:6379 --name fb-test-redis valkey/valkey:8
FEATHERBIT_TEST_REDIS_URL=redis://127.0.0.1:16379 cargo test --package featherbit test_ping_live
docker stop fb-test-redis
```
Expected: both `test_ping_live` tests pass; the admin one reports a `valkey_version` string — proving the Valkey alias end to end.

- [ ] **Step 5: Commit**

```bash
git add src/admin/stores.rs
git commit -m "feat(admin): store connectivity ping endpoint"
```

---

### Task 10: Docs, spec amendments, final verification

**Files:**
- Modify: `website/docs/guides/admin-api.md` (endpoint table :27-63, mutation notes :65-73, front-matter description :3)
- Modify: `website/docs/reference/plugins/limit-count.md` (policy row :17 + new `store` row)
- Modify: `website/docs/reference/roadmap.md` (:17 — redis policy shipped)
- Modify: `docs/superpowers/specs/2026-08-21-session-storage-design.md` (two amendments)
- Modify: `CLAUDE.md` (Core features + Configuration sections)

**Interfaces:** none — documentation of everything Tasks 1-9 shipped, using their exact endpoint paths, config keys, and error envelopes.

- [ ] **Step 1: Admin API reference**

In `website/docs/guides/admin-api.md`, insert after the plugin-configs rows (:45), matching the table's `:name` path convention:

```markdown
| GET | `/api/stores` | List named stores (redis/valkey connections) | — |
| POST | `/api/stores` | Create a store | 409 if the name exists, 400 on validation failure |
| GET | `/api/stores/:name` | Get one store (raw config — `${ENV}` placeholders are never resolved) | 404 |
| PUT | `/api/stores/:name` | Create or update a store (upsert) | 400 on validation failure |
| DELETE | `/api/stores/:name` | Delete a store | 404; 409 `{"error":"in_use","referrers":[...]}` if referenced |
| POST | `/api/stores/:name/ping` | Connectivity check: latency + server version | 404; 400 bad config; 502 unreachable; 504 timeout; 501 headless build |
```

And a bullet in "Notes on mutation semantics":

```markdown
- `stores` follow the standard semantics (POST rejects duplicates, PUT upserts) with one addition: DELETE is guarded — a store referenced by any node or shared plugin config returns `409 {"error":"in_use","referrers":[...]}` naming each referrer. Ping resolves `${ENV_VAR}` placeholders at call time; responses never echo connection details. Binaries built without the `redis-store` feature reject any declared store at config load and answer ping with 501.
```

Also extend the front-matter `description` (:3) resource list with `stores`.

- [ ] **Step 2: limit-count plugin reference**

In `website/docs/reference/plugins/limit-count.md`, replace the policy row (:17) and add the store row:

```markdown
| policy | string | local | Counter backend. `local` = per-instance in-memory windows; `redis` = cluster-shared windows via a named `stores:` entry. Anything else is rejected at config load with the supported list. |
| store | string | — | Required when `policy: redis`: the name of a declared `stores:` entry (redis or valkey). Unknown names fail policy compilation. |
```

And append a behavior note wherever the page discusses windows:

```markdown
With `policy: redis`, window boundaries are wall-clock aligned (`now / time_window`) and shared by every gateway instance — switching from `local` changes boundaries from first-request-aligned to clock-aligned. Backend errors respect `allow_degradation` (default `false`: reject). Errors are counted in `gateway_counter_store_errors_total{store}`.
```

- [ ] **Step 3: Roadmap, module docs, CLAUDE.md**

- `website/docs/reference/roadmap.md:17`: update the redis rate-limit policy entry from planned to shipped ("limit-count and workflow support `policy: redis` via named `stores:`; token-bucket `rate-limit` and `limit-conn` remain local-only, distributed backends planned on the same resource").
- `CLAUDE.md`: in "Core features", extend the shared-plugin-configs bullet's neighborhood with a new bullet: `- **Shared stores** — named redis/valkey connections (\`stores:\` in gateway.yaml) referenced by plugin config; clients built at config-apply (raw \`${ENV}\` in storage), Admin CRUD + ping at /api/stores, etcd-synced; powers \`policy: redis\` cluster-accurate limit-count counters (\`redis-store\` cargo feature, default-on)`. In "Configuration", note `stores` as a gateway.yaml section.
- `docs/superpowers/specs/2026-08-21-session-storage-design.md` — two amendments (mark inline, don't rewrite history): in §1, change `pool_size: 8 # optional` to a line noting v1 uses a single auto-reconnecting multiplexed connection (no pool_size field); in §3, change `counter_store_errors_total{store}` to `gateway_counter_store_errors_total{store}` (house prefix).

- [ ] **Step 4: Docs build check**

Run: `cd website && npm run build`
Expected: builds clean (broken links in these files would fail here, not in CI).

- [ ] **Step 5: Full final verification**

Run, from the repo root:

```bash
cargo fmt --check
cargo test
cargo clippy --all-targets -- -D warnings
cargo check --no-default-features
cargo clippy --no-default-features --all-targets -- -D warnings
cargo tree -i aws-lc-sys   # expected: "did not match any packages"
```

Expected: everything green. Optional full live pass (redis + valkey):

```bash
docker run -d --rm -p 16379:6379 --name fb-t-redis redis:7
FEATHERBIT_TEST_REDIS_URL=redis://127.0.0.1:16379 cargo test live
docker stop fb-t-redis
docker run -d --rm -p 16379:6379 --name fb-t-valkey valkey/valkey:8
FEATHERBIT_TEST_REDIS_URL=redis://127.0.0.1:16379 cargo test live
docker stop fb-t-valkey
```

- [ ] **Step 6: Commit**

```bash
git add website/docs CLAUDE.md docs/superpowers/specs/2026-08-21-session-storage-design.md
git commit -m "docs: stores resource, redis limit-count policy, spec amendments"
```

---

## Out of scope for this plan (tracked)

- **Plan 2** (written after this plan lands): `SessionStore` trait + `RedisSessionStore` (hash-tagged keys), `session:` config block for the five auth plugins, refresh lock, sessions Admin API.
- **Plan 3**: UI (stores editor with ping button, `store` pickers in node config — `ui/src/pluginConfig.ts:469` gains `redis` in the policy options, workflow's raw-textarea rules panel documented as no-picker), e2e scenarios + `e2e/E2E_TESTBOOK.md` rows, CI job with `redis:7`/`valkey:8` service containers, a `website/docs/concepts/stores.md` page, `ui/src/api/client.ts` + `ui/src/types/index.ts` typed client.
- Debug sandbox (`src/admin/debug.rs:252-285`): no change needed — it compiles candidate policies against `state.resources`, whose store registry always reflects the applied config; a sandbox policy referencing a declared store resolves correctly. Noted here so nobody "fixes" it.
