# Distributed Response Cache Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let `proxy-cache` share its cached responses across gateway instances through a declared redis/valkey store, and bound what the local cache keeps.

**Architecture:** A `ResponseCache` trait with two implementations (local `DashMap`, redis hash), resolved at policy-compile time from `policy: local | redis` + `store:` — the shape `limit-count` already uses. `proxy-cache` holds an `Arc<dyn ResponseCache>` and maps a backend error to a cache miss in one visible place.

**Tech Stack:** Rust, `async_trait`, `dashmap` 6, `redis` 0.32 (`AsyncCommands`, hashes + pipelines), `prometheus` counters.

**Spec:** `docs/superpowers/specs/2026-09-17-distributed-response-cache-design.md`

## Global Constraints

- **Config shape, verbatim:** `policy: local | redis`, `store: <name>`, `max_object_bytes` (default `1048576`). `max_entries` (default `10000`) is process-wide, in `system.yaml`, not per node.
- **A backend failure is a MISS**, never a failed request. It must be logged at `warn` and metered — a silently degraded cache is indistinguishable from a working one.
- **`ResponseCache` returns `Result`**, never folding an error into `Ok(None)`. The degrade decision lives in `proxy-cache`, where a test can observe it.
- **Key namespace:** `format!("{}:{}:{}", key_prefix, namespaces::CACHE, key)` with `CACHE = "cache"`, declared in `src/stores/namespaces.rs`, added to `MANAGED`, and covered by the existing drift test.
- **Redis expiry is redis's own TTL.** Never compare `Instant`s for a redis entry — two instances do not share a monotonic clock.
- **No new dependencies.** `dashmap` is the only concurrent-map crate in the tree; the local bound is built on it. Adding an LRU crate is out of scope (`cargo-deny` surface).
- **Everything redis is behind the `redis-store` feature**, and this crate has **no `src/lib.rs`**, so dead-code reachability roots at `fn main`: code lands in the same commit as its first caller, and `#[allow(dead_code)]` is never the fix.
- **Lint with CI's real commands:** `cargo clippy --all-targets --locked -- -D warnings` AND the same with `--no-default-features`. Plain `cargo clippy` exits 0 on unused items.
- **Run the full `cargo test`** and `cargo test --release`; the e2e suite runs the release binary and embeds `ui/dist` at build time (`cd ui && npm run build` before `cargo build --release` if UI files changed).
- **Commit style:** Conventional Commits, no `Co-Authored-By`, no AI attribution.
- **Branch:** `feature/distributed-response-cache`, off `develop`.

## File Structure

| File | Responsibility |
|---|---|
| `src/traffic/cache.rs` (create) | `CachedResponse`, `CacheError`, the `ResponseCache` trait, `LocalResponseCache` |
| `src/traffic/mod.rs` (modify) | Re-export the above; `TrafficRegistries.cache` becomes the local implementation |
| `src/plugins/native/proxy_cache.rs` (modify) | Hold `Arc<dyn ResponseCache>`; resolve by `policy`/`store`; map `Err` to a miss |
| `src/stores/namespaces.rs` (modify) | `CACHE` constant, in `MANAGED`, covered by the drift test |
| `src/stores/redis_cache.rs` (create) | `RedisResponseCache`: hash encoding, TTL, key namespacing |
| `src/config/system.rs` (modify) | `cache.max_entries` |
| `src/metrics/mod.rs` (modify) | hit / miss / error / eviction / too-large counters |
| `website/docs/reference/plugins/proxy-cache.md` (modify) | the three new keys, and why this one fails open |

---

### Task 1: The `ResponseCache` trait and the local backend

**Files:**
- Create: `src/traffic/cache.rs`
- Modify: `src/traffic/mod.rs`, `src/plugins/native/proxy_cache.rs`

**Interfaces:**
- Produces, used by every later task:
  - `pub struct CachedResponse { pub status: u16, pub headers: HashMap<String, Vec<String>>, pub body: Bytes }`
  - `pub struct CacheError(pub String)`
  - `#[async_trait] pub trait ResponseCache: Send + Sync { async fn get(&self, key: &str) -> Result<Option<CachedResponse>, CacheError>; async fn put(&self, key: &str, entry: &CachedResponse, ttl: Duration) -> Result<(), CacheError>; }`
  - `pub struct LocalResponseCache` implementing it

> **Type split, and why.** Today `CacheEntry` carries `expires_at` alongside the response. The trait must not: a redis entry's lifetime is redis's TTL, and an `Instant` written by one instance is meaningless to another. `CachedResponse` is the response alone; the local backend keeps its own `(CachedResponse, Instant)` internally. The spec calls this type `CacheEntry` throughout — it is the same thing, renamed here for that reason.

- [ ] **Step 1: Write the failing tests**

Create `src/traffic/cache.rs` with only this test module:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn response(body: &str) -> CachedResponse {
        let mut headers = HashMap::new();
        headers.insert("content-type".to_string(), vec!["text/plain".to_string()]);
        CachedResponse {
            status: 200,
            headers,
            body: Bytes::from(body.to_string()),
        }
    }

    #[tokio::test]
    async fn test_local_round_trips_a_response() {
        let cache = LocalResponseCache::default();
        cache.put("k", &response("hello"), Duration::from_secs(60)).await.unwrap();

        let got = cache.get("k").await.unwrap().expect("a stored entry");
        assert_eq!(got.status, 200);
        assert_eq!(got.body, Bytes::from("hello"));
        assert_eq!(got.headers.get("content-type").unwrap(), &vec!["text/plain".to_string()]);
    }

    #[tokio::test]
    async fn test_local_miss_is_ok_none_not_an_error() {
        // The distinction the whole design rests on: "nothing stored" is a
        // successful answer, not a failure. Only a backend that could not
        // answer returns Err.
        let cache = LocalResponseCache::default();
        assert!(cache.get("absent").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn test_local_entry_expires() {
        let cache = LocalResponseCache::default();
        cache.put("k", &response("x"), Duration::from_millis(1)).await.unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(cache.get("k").await.unwrap().is_none(), "an expired entry must not be served");
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --no-run 2>&1 | grep "^error"`
Expected: `cannot find type 'CachedResponse'`, `cannot find type 'LocalResponseCache'`.

- [ ] **Step 3: Write the implementation**

Above the test module in `src/traffic/cache.rs`:

```rust
//! Response-cache backends for the `proxy-cache` node.
//!
//! One trait, two implementations: a process-local map and (from the redis
//! backend onwards) a shared store. `proxy-cache` holds whichever it was
//! configured with and never learns which.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use bytes::Bytes;
use dashmap::DashMap;

/// A cached upstream response, without any notion of when it expires.
///
/// Lifetime is the backend's business: the local map compares `Instant`s,
/// while a redis entry lives on the key's own TTL. Carrying an `Instant` here
/// would be meaningless to a second instance, which does not share the first
/// one's monotonic clock.
#[derive(Debug, Clone)]
pub struct CachedResponse {
    pub status: u16,
    pub headers: HashMap<String, Vec<String>>,
    pub body: Bytes,
}

/// A backend could not answer. Distinct from a miss; see [`ResponseCache`].
#[derive(Debug)]
pub struct CacheError(pub String);

impl std::fmt::Display for CacheError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "response cache error: {}", self.0)
    }
}

/// Storage for `proxy-cache`.
///
/// `Ok(None)` means the backend answered and had nothing; `Err` means it could
/// not answer. Keeping those apart is deliberate: the caller decides that an
/// outage should read as a miss, and that decision belongs somewhere a
/// reviewer can find it rather than inside a backend that swallows its own
/// errors.
#[async_trait]
pub trait ResponseCache: Send + Sync {
    async fn get(&self, key: &str) -> Result<Option<CachedResponse>, CacheError>;
    async fn put(&self, key: &str, entry: &CachedResponse, ttl: Duration)
        -> Result<(), CacheError>;
}

/// Process-local cache. Shared by every `policy: local` node in the gateway.
#[derive(Default)]
pub struct LocalResponseCache {
    entries: DashMap<String, (CachedResponse, Instant)>,
}

#[async_trait]
impl ResponseCache for LocalResponseCache {
    async fn get(&self, key: &str) -> Result<Option<CachedResponse>, CacheError> {
        let Some(found) = self.entries.get(key) else {
            return Ok(None);
        };
        if Instant::now() < found.1 {
            let hit = found.0.clone();
            Ok(Some(hit))
        } else {
            drop(found);
            self.entries.remove(key);
            Ok(None)
        }
    }

    async fn put(
        &self,
        key: &str,
        entry: &CachedResponse,
        ttl: Duration,
    ) -> Result<(), CacheError> {
        self.entries
            .insert(key.to_string(), (entry.clone(), Instant::now() + ttl));
        Ok(())
    }
}
```

Add `pub mod cache;` and `pub use cache::{CachedResponse, CacheError, LocalResponseCache, ResponseCache};` to `src/traffic/mod.rs`, and change `TrafficRegistries.cache` to `LocalResponseCache`. Delete the old `CacheRegistry` and `CacheEntry` — nothing else uses them once `proxy-cache` is switched over in the next step.

- [ ] **Step 4: Switch `proxy-cache` onto the trait**

In `src/plugins/native/proxy_cache.rs`, replace the two call sites (around lines 301 and 324). The lookup arm:

```rust
            Role::Lookup => {
                // A backend that cannot answer is treated as a miss: this
                // cache exists to save a trip upstream, not to decide
                // whether the request is allowed. Metered in a later task.
                let found = match self.cache.get(&key).await {
                    Ok(found) => found,
                    Err(e) => {
                        tracing::warn!(key = %key, "proxy-cache lookup failed: {e}");
                        None
                    }
                };
                if let Some(entry) = found {
                    ctx.response.status_code = entry.status;
                    ctx.response.headers = entry.headers;
                    ctx.response.body = entry.body;
```

and the store arm:

```rust
            Role::Store => {
                let status = ctx.response.status_code;
                if self.cache_statuses.contains(&status) {
                    let entry = CachedResponse {
                        status,
                        headers: ctx.response.headers.clone(),
                        body: ctx.response.body.clone(),
                    };
                    if let Err(e) = self.cache.put(&key, &entry, self.cache_ttl).await {
                        tracing::warn!(key = %key, "proxy-cache store failed: {e}");
                    }
                }
```

The plugin's field becomes `cache: Arc<dyn ResponseCache>`, set in `from_config` from `resources.traffic.cache` (a `local` cache for now; `policy` arrives in Task 4). The existing `resources` field stays if other code uses it.

- [ ] **Step 5: Run the full suite**

Run: `cargo test`, `cargo test --release`, `cargo fmt --all --check`, `cargo clippy --all-targets --locked -- -D warnings`, and the same clippy with `--no-default-features`.
Expected: all pass. `proxy-cache`'s existing tests must pass unchanged — this step is a refactor, not a behaviour change, and those tests are what prove it.

- [ ] **Step 6: Commit**

```bash
git add src/traffic/ src/plugins/native/proxy_cache.rs
git commit -m "refactor(cache): put proxy-cache behind a ResponseCache trait

One trait, one implementation so far: the process-local map that
CacheRegistry already was. proxy-cache now holds an Arc<dyn ResponseCache>
and does not know which backend it has, which is what lets a shared one
arrive without touching the node's logic.

The response type loses its expires_at. Lifetime is the backend's
business: the local map compares Instants, while a redis entry will live
on the key's own TTL, and an Instant written by one instance means
nothing to another that does not share its monotonic clock.

get returns Result<Option<_>> rather than Option: a miss and an
unreachable backend are different answers, and folding them together
inside the backend would hide a policy decision where no test could
observe it. proxy-cache maps Err to a miss in one visible place.

Behaviour is unchanged; proxy-cache's existing tests are what prove it."
```

---

### Task 2: Bound the local cache

**Files:**
- Modify: `src/traffic/cache.rs`, `src/config/system.rs`, `src/main.rs`

**Interfaces:**
- Consumes: `LocalResponseCache` (Task 1).
- Produces: `LocalResponseCache::set_capacity(&self, max_entries: usize)`; `CacheConfig { max_entries: usize }` on `SystemConfig` as `cache`.

> The local map evicts an expired entry **only when something reads it**, so anything written and never read again is kept for the life of the process. A key space that churns — per user, per tenant, per path segment — grows without bound.

- [ ] **Step 1: Write the failing tests**

Add to the test module in `src/traffic/cache.rs`:

```rust
    #[tokio::test]
    async fn test_local_evicts_expired_entries_before_live_ones() {
        let cache = LocalResponseCache::default();
        cache.set_capacity(2);

        // One entry that is already dead, one that is not.
        cache.put("dead", &response("d"), Duration::from_millis(1)).await.unwrap();
        cache.put("live", &response("l"), Duration::from_secs(60)).await.unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;

        // At capacity: the expired entry is the one that must go, not the
        // useful one.
        cache.put("new", &response("n"), Duration::from_secs(60)).await.unwrap();

        assert!(cache.get("dead").await.unwrap().is_none());
        assert!(cache.get("live").await.unwrap().is_some(), "a live entry must survive a sweep");
        assert!(cache.get("new").await.unwrap().is_some());
    }

    #[tokio::test]
    async fn test_local_at_capacity_evicts_the_soonest_to_expire() {
        let cache = LocalResponseCache::default();
        cache.set_capacity(2);

        cache.put("short", &response("s"), Duration::from_secs(1)).await.unwrap();
        cache.put("long", &response("l"), Duration::from_secs(600)).await.unwrap();
        cache.put("new", &response("n"), Duration::from_secs(600)).await.unwrap();

        // Nothing has expired, so the bound falls back to discarding what was
        // about to become worthless anyway. This is NOT an LRU and the test
        // says so: a hot short-TTL entry loses to a cold long-TTL one.
        assert!(cache.get("short").await.unwrap().is_none());
        assert!(cache.get("long").await.unwrap().is_some());
        assert!(cache.get("new").await.unwrap().is_some());
    }

    #[tokio::test]
    async fn test_local_never_exceeds_its_capacity() {
        let cache = LocalResponseCache::default();
        cache.set_capacity(4);
        for i in 0..50 {
            cache
                .put(&format!("k{i}"), &response("x"), Duration::from_secs(600))
                .await
                .unwrap();
        }
        assert!(cache.len() <= 4, "the bound must hold under sustained writes: {}", cache.len());
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --no-run 2>&1 | grep "^error"`
Expected: `no method named 'set_capacity'`, `no method named 'len'`.

- [ ] **Step 3: Write the implementation**

In `src/traffic/cache.rs`, give the struct a capacity and an eviction path:

```rust
/// Entries the local cache keeps before it starts evicting.
const DEFAULT_MAX_ENTRIES: usize = 10_000;

pub struct LocalResponseCache {
    entries: DashMap<String, (CachedResponse, Instant)>,
    /// Read on every insert; set once at startup from `cache.max_entries`.
    capacity: std::sync::atomic::AtomicUsize,
}

impl Default for LocalResponseCache {
    fn default() -> Self {
        Self {
            entries: DashMap::new(),
            capacity: std::sync::atomic::AtomicUsize::new(DEFAULT_MAX_ENTRIES),
        }
    }
}

impl LocalResponseCache {
    /// Sets the entry bound. Called once at startup, before traffic.
    pub fn set_capacity(&self, max_entries: usize) {
        self.capacity
            .store(max_entries.max(1), std::sync::atomic::Ordering::Relaxed);
    }

    /// Entries currently held, live or not yet swept.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Makes room for one more entry.
    ///
    /// Expired entries go first, since they are already worthless. If that is
    /// not enough, the entry expiring soonest goes next — deliberately not an
    /// LRU (no LRU crate is in the dependency tree, and adding one for this is
    /// not worth the supply-chain surface). For a cache whose entries all
    /// carry TTLs this discards what was about to become useless anyway; the
    /// cost is that a hot short-TTL entry loses to a cold long-TTL one.
    fn make_room(&self, capacity: usize) {
        if self.entries.len() < capacity {
            return;
        }
        let now = Instant::now();
        self.entries.retain(|_, (_, expires_at)| now < *expires_at);

        while self.entries.len() >= capacity {
            let soonest = self
                .entries
                .iter()
                .min_by_key(|e| e.value().1)
                .map(|e| e.key().clone());
            match soonest {
                Some(key) => {
                    self.entries.remove(&key);
                }
                None => break,
            }
        }
    }
}
```

and call it from `put`, before the insert:

```rust
        self.make_room(self.capacity.load(std::sync::atomic::Ordering::Relaxed));
        self.entries
            .insert(key.to_string(), (entry.clone(), Instant::now() + ttl));
```

- [ ] **Step 4: Add the config and wire it at startup**

In `src/config/system.rs`, alongside the other sections:

```rust
    /// Response-cache limits for `proxy-cache`'s `policy: local` backend.
    #[serde(default)]
    pub cache: CacheConfig,
```

```rust
/// Process-wide response-cache limits.
///
/// `max_entries` is here rather than on the node because every `policy: local`
/// node shares one cache; a per-node value would be a setting that silently
/// meant something else.
#[derive(Debug, Deserialize, Clone)]
#[serde(default)]
pub struct CacheConfig {
    pub max_entries: usize,
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self { max_entries: 10_000 }
    }
}
```

In `src/main.rs`, after the resources handle is built and before listeners start:

```rust
    resources.traffic.cache.set_capacity(system.cache.max_entries);
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test traffic::cache` → the three new tests plus Task 1's pass.
Then the full suite: `cargo test`, `cargo test --release`, `cargo fmt --all --check`, and both clippy invocations.

- [ ] **Step 6: Commit**

```bash
git add src/traffic/cache.rs src/config/system.rs src/main.rs
git commit -m "fix(cache): bound the local response cache

The local cache removed an expired entry only when something read it, so
anything written and never read again was retained for the life of the
process. A key space that churns -- per user, per tenant, per path
segment -- grew the map without limit, and nothing in the gateway said so.

Bound it with cache.max_entries (default 10000, process-wide because
every policy: local node shares one cache). At capacity, expired entries
are swept first; if that is not enough, the entry expiring soonest is
discarded.

That is deliberately not an LRU, and the tests say so rather than
implying otherwise: a hot short-TTL entry loses to a cold long-TTL one.
No LRU crate is in the dependency tree and adding one for this is not
worth the cargo-deny surface; for a cache whose entries all carry TTLs,
evicting the soonest to expire discards what was about to become
worthless anyway."
```

---

### Task 3: Degrade visibly — metrics and a failing backend

**Files:**
- Modify: `src/metrics/mod.rs`, `src/plugins/native/proxy_cache.rs`

**Interfaces:**
- Consumes: `ResponseCache`, `CacheError` (Task 1).
- Produces: `GatewayMetrics.cache_events: IntCounterVec` labelled `["backend", "event"]`, where `event` is one of `hit`, `miss`, `error`, `too_large`.

> A cache that has quietly degraded to always-miss looks exactly like a working one from every angle except the upstream's load graph. This task is what makes the difference observable, and it is testable now — with a deliberately failing backend — rather than only once redis exists.

- [ ] **Step 1: Write the failing tests**

Add to the test module in `src/plugins/native/proxy_cache.rs`:

```rust
    /// A backend that cannot answer. Stands in for a redis outage, so the
    /// degradation path is testable without a live store.
    struct BrokenCache;

    #[async_trait::async_trait]
    impl crate::traffic::ResponseCache for BrokenCache {
        async fn get(
            &self,
            _key: &str,
        ) -> Result<Option<crate::traffic::CachedResponse>, crate::traffic::CacheError> {
            Err(crate::traffic::CacheError("backend down".to_string()))
        }
        async fn put(
            &self,
            _key: &str,
            _entry: &crate::traffic::CachedResponse,
            _ttl: std::time::Duration,
        ) -> Result<(), crate::traffic::CacheError> {
            Err(crate::traffic::CacheError("backend down".to_string()))
        }
    }

    /// The load-bearing behaviour: an outage costs latency, not availability.
    /// A lookup against a dead backend must leave through `success` (on to the
    /// upstream), not `error` and not `hit`.
    #[tokio::test]
    async fn test_a_failing_backend_is_a_miss_not_an_error() {
        let plugin = lookup_plugin_with_cache(Arc::new(BrokenCache));
        let out = plugin.execute(test_context()).await.expect("a cache outage must not fail the request");
        assert_eq!(out.port, None, "a miss continues to the upstream on `success`");
    }

    /// ...but it must not be silent, or a fully-degraded cache is
    /// indistinguishable from a working one.
    #[tokio::test]
    async fn test_a_failing_backend_increments_the_error_counter() {
        let metrics = test_metrics();
        let plugin = lookup_plugin_with_cache_and_metrics(Arc::new(BrokenCache), metrics.clone());
        plugin.execute(test_context()).await.unwrap();

        assert_eq!(
            metrics.cache_events.with_label_values(&["local", "error"]).get(),
            1,
            "a backend error must be visible in metrics"
        );
    }
```

Add the two small constructors (`lookup_plugin_with_cache`, `lookup_plugin_with_cache_and_metrics`) beside the file's existing test helpers, building a `Role::Lookup` plugin with the given `Arc<dyn ResponseCache>`; and `test_metrics()` returning an `Arc<GatewayMetrics>` over a fresh `prometheus::Registry`, as `src/graph/engine.rs` tests already do.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --no-run 2>&1 | grep "^error"`
Expected: `no field 'cache_events' on type 'GatewayMetrics'`, plus the missing helpers.

- [ ] **Step 3: Add the metric**

In `src/metrics/mod.rs`, beside `counter_store_errors`:

```rust
    /// Response-cache outcomes, per backend and event.
    ///
    /// `error` is the one that matters: a cache degraded to always-miss keeps
    /// serving correct responses, just slower and with more upstream load, so
    /// it is invisible in every other signal.
    pub cache_events: IntCounterVec,
```

```rust
        let cache_events = IntCounterVec::new(
            Opts::new(
                "gateway_cache_events_total",
                "Response-cache outcomes per backend (hit, miss, error, too_large)",
            ),
            &["backend", "event"],
        )
        .unwrap();
```

and register it with the others: `registry.register(Box::new(cache_events.clone())).unwrap();`

- [ ] **Step 4: Record the events in `proxy-cache`**

Extend the lookup arm written in Task 1 so each outcome is counted, with `backend` being `"local"` or `"redis"`:

```rust
                let found = match self.cache.get(&key).await {
                    Ok(found) => found,
                    Err(e) => {
                        tracing::warn!(key = %key, "proxy-cache lookup failed: {e}");
                        self.record("error");
                        None
                    }
                };
                self.record(if found.is_some() { "hit" } else { "miss" });
```

with a small helper on the plugin:

```rust
    /// Counts one cache outcome. A no-op when metrics are disabled (unit tests).
    fn record(&self, event: &str) {
        if let Some(metrics) = &self.resources.metrics {
            metrics
                .cache_events
                .with_label_values(&[self.backend_label, event])
                .inc();
        }
    }
```

where `backend_label: &'static str` is set in `from_config` (`"local"` for now).

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test proxy_cache` then the full suite, `cargo test --release`, `cargo fmt --all --check`, and both clippy invocations.

- [ ] **Step 6: Commit**

```bash
git add src/metrics/mod.rs src/plugins/native/proxy_cache.rs
git commit -m "feat(cache): degrade visibly when the backend cannot answer

A cache exists to save a trip upstream, not to decide whether a request
is allowed, so a backend that cannot answer is treated as a miss and the
request proceeds. That is deliberately opposite to sessions and the
store-* nodes, which fail closed because losing their store loses
correctness; turning a redis blip into a 503 on a route that was merely
going faster would be a worse outage than the one it reports.

It must not be silent, though. A cache degraded to always-miss keeps
serving correct responses -- just slower, with more upstream load -- so
it is invisible in every signal except the upstream's. Count hits,
misses and errors per backend.

Both paths are tested now rather than waiting for a redis backend: a
deliberately failing test double stands in for an outage, which also
means the degradation has a test that does not need a live store."
```

---

### Task 4: The redis backend

**Files:**
- Create: `src/stores/redis_cache.rs`
- Modify: `src/stores/mod.rs`, `src/stores/namespaces.rs`, `src/plugins/native/proxy_cache.rs`

**Interfaces:**
- Consumes: `ResponseCache`, `CachedResponse`, `CacheError` (Task 1); `StoreRegistry::client` and `namespaces` (existing).
- Produces: `RedisResponseCache::new(client: Arc<RedisStoreClient>) -> Self`; `namespaces::CACHE`.

- [ ] **Step 1: Write the failing tests**

Create `src/stores/redis_cache.rs` with only this test module:

```rust
#[cfg(all(test, feature = "redis-store"))]
mod tests {
    use super::*;

    fn store_url() -> Option<String> {
        std::env::var("FEATHERBIT_TEST_REDIS_URL").ok().filter(|s| !s.is_empty())
    }

    fn client(url: &str) -> Arc<crate::stores::redis_store::RedisStoreClient> {
        let cfg: crate::config::gateway::StoreConfig = serde_yaml::from_str(&format!(
            "name: cache-test\ntype: redis\nurl: {url}\nkey_prefix: fbtest\n"
        ))
        .unwrap();
        Arc::new(crate::stores::redis_store::RedisStoreClient::build(&cfg).unwrap())
    }

    fn response(body: &[u8]) -> CachedResponse {
        let mut headers = HashMap::new();
        headers.insert("x-multi".to_string(), vec!["a".to_string(), "b".to_string()]);
        CachedResponse { status: 203, headers, body: Bytes::copy_from_slice(body) }
    }

    /// The point of the whole feature: an entry written by one instance is
    /// readable by another. Two handles over the same store stand in for two
    /// gateways -- nothing else demonstrates sharing.
    #[tokio::test]
    async fn test_an_entry_written_by_one_handle_is_read_by_another() {
        let Some(url) = store_url() else { return };
        let writer = RedisResponseCache::new(client(&url));
        let reader = RedisResponseCache::new(client(&url));
        let key = format!("share-{}", uuid::Uuid::new_v4());

        writer.put(&key, &response(b"shared"), Duration::from_secs(60)).await.unwrap();

        let got = reader.get(&key).await.unwrap().expect("the second handle must see it");
        assert_eq!(got.body, Bytes::from_static(b"shared"));
    }

    /// Binary bodies and multi-value headers must survive the encoding: a
    /// base64 or JSON wrapper would be the easy way to get this wrong.
    #[tokio::test]
    async fn test_a_binary_body_and_multi_value_header_round_trip() {
        let Some(url) = store_url() else { return };
        let cache = RedisResponseCache::new(client(&url));
        let key = format!("bin-{}", uuid::Uuid::new_v4());
        let body = vec![0u8, 159, 146, 150, 255];

        cache.put(&key, &response(&body), Duration::from_secs(60)).await.unwrap();

        let got = cache.get(&key).await.unwrap().unwrap();
        assert_eq!(got.status, 203);
        assert_eq!(got.body.as_ref(), body.as_slice());
        assert_eq!(got.headers.get("x-multi").unwrap(), &vec!["a".to_string(), "b".to_string()]);
    }

    #[tokio::test]
    async fn test_a_missing_key_is_ok_none() {
        let Some(url) = store_url() else { return };
        let cache = RedisResponseCache::new(client(&url));
        assert!(cache.get(&format!("absent-{}", uuid::Uuid::new_v4())).await.unwrap().is_none());
    }

    /// Expiry is redis's own TTL, never an Instant comparison -- two instances
    /// do not share a monotonic clock.
    #[tokio::test]
    async fn test_the_key_carries_a_ttl() {
        let Some(url) = store_url() else { return };
        let cache = RedisResponseCache::new(client(&url));
        let key = format!("ttl-{}", uuid::Uuid::new_v4());
        cache.put(&key, &response(b"x"), Duration::from_secs(30)).await.unwrap();

        let mut conn = cache.client.conn().await.unwrap();
        let ttl: i64 = redis::cmd("TTL")
            .arg(cache.redis_key(&key))
            .query_async(&mut conn)
            .await
            .unwrap();
        assert!(ttl > 0, "the entry must expire on redis's clock, not ours: {ttl}");
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --no-run 2>&1 | grep "^error"`
Expected: `cannot find type 'RedisResponseCache'`.

- [ ] **Step 3: Declare the namespace**

In `src/stores/namespaces.rs`:

```rust
/// Cached responses (`proxy-cache` with `policy: redis`).
pub const CACHE: &str = "cache";
```

and add `CACHE` to `MANAGED`. The existing drift test covers the builders; extend `test_key_builders_stay_inside_their_declared_namespace` with:

```rust
        let cache = crate::stores::redis_cache::cache_key("fb", "abc");
        assert!(cache.starts_with(&format!("fb:{}:", CACHE)), "{cache}");
```

- [ ] **Step 4: Write the implementation**

Above the test module in `src/stores/redis_cache.rs`:

```rust
//! Shared response cache over a declared `stores:` entry.
//!
//! One redis hash per entry: `status`, `headers` (JSON) and `body` (raw
//! bytes). Redis values are binary-safe, so the body is stored as-is --
//! base64 would inflate every cached response by a third, and a JSON wrapper
//! would escape it on top of that.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use redis::AsyncCommands;

use crate::stores::namespaces;
use crate::stores::redis_store::RedisStoreClient;
use crate::traffic::{CacheError, CachedResponse, ResponseCache};

/// The redis key for a cache entry.
pub(crate) fn cache_key(prefix: &str, key: &str) -> String {
    format!("{}:{}:{}", prefix, namespaces::CACHE, key)
}

pub struct RedisResponseCache {
    pub(crate) client: Arc<RedisStoreClient>,
}

impl RedisResponseCache {
    pub fn new(client: Arc<RedisStoreClient>) -> Self {
        Self { client }
    }

    pub(crate) fn redis_key(&self, key: &str) -> String {
        cache_key(self.client.key_prefix(), key)
    }
}

#[async_trait]
impl ResponseCache for RedisResponseCache {
    async fn get(&self, key: &str) -> Result<Option<CachedResponse>, CacheError> {
        let mut conn = self.client.conn().await.map_err(CacheError)?;
        let fields: HashMap<String, Vec<u8>> = conn
            .hgetall(self.redis_key(key))
            .await
            .map_err(|e| CacheError(e.to_string()))?;

        if fields.is_empty() {
            return Ok(None);
        }

        // A hash the gateway cannot parse is unusable either way, so it reads
        // as a miss rather than an error: the request keeps moving, and the
        // two outcomes stay distinguishable in metrics.
        let (Some(status), Some(headers), Some(body)) =
            (fields.get("status"), fields.get("headers"), fields.get("body"))
        else {
            return Ok(None);
        };
        let Ok(status) = String::from_utf8_lossy(status).parse::<u16>() else {
            return Ok(None);
        };
        let Ok(headers) = serde_json::from_slice::<HashMap<String, Vec<String>>>(headers) else {
            return Ok(None);
        };

        Ok(Some(CachedResponse {
            status,
            headers,
            body: Bytes::copy_from_slice(body),
        }))
    }

    async fn put(
        &self,
        key: &str,
        entry: &CachedResponse,
        ttl: Duration,
    ) -> Result<(), CacheError> {
        let mut conn = self.client.conn().await.map_err(CacheError)?;
        let redis_key = self.redis_key(key);
        let headers =
            serde_json::to_vec(&entry.headers).map_err(|e| CacheError(e.to_string()))?;

        // Fields and expiry in one pipeline: an entry without a TTL would
        // outlive its freshness and never be swept.
        redis::pipe()
            .atomic()
            .hset(&redis_key, "status", entry.status.to_string())
            .hset(&redis_key, "headers", headers)
            .hset(&redis_key, "body", entry.body.as_ref())
            .expire(&redis_key, ttl.as_secs().max(1) as i64)
            .query_async::<()>(&mut conn)
            .await
            .map_err(|e| CacheError(e.to_string()))
    }
}
```

Add `#[cfg(feature = "redis-store")] pub mod redis_cache;` to `src/stores/mod.rs`.

- [ ] **Step 5: Resolve the backend from config**

In `proxy_cache.rs`'s `from_config`, mirroring `limit_count.rs:131`:

```rust
        let policy = config.get("policy").and_then(|v| v.as_str()).unwrap_or("local");
        let (cache, backend_label): (Arc<dyn ResponseCache>, &'static str) = match policy {
            "local" => (resources.traffic.cache.clone(), "local"),
            #[cfg(feature = "redis-store")]
            "redis" => {
                let name = config
                    .get("store")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| {
                        "proxy-cache: policy 'redis' requires 'store' naming a declared stores: entry"
                            .to_string()
                    })?;
                let client = resources.stores.load().client(name)?;
                (
                    Arc::new(crate::stores::redis_cache::RedisResponseCache::new(client)),
                    "redis",
                )
            }
            other => {
                return Err(format!(
                    "proxy-cache: unknown policy '{other}' — supported: local, redis"
                ))
            }
        };
```

`TrafficRegistries.cache` becomes `Arc<LocalResponseCache>` so it can be cloned into the plugin.

- [ ] **Step 6: Run the tests**

Start a backend: `docker run --rm -d --name fb-cache-redis -p 6379:6379 redis:7`

Run: `FEATHERBIT_TEST_REDIS_URL=redis://127.0.0.1:6379 cargo test` — the gated tests must actually run, not skip. Then `cargo test` without the variable (they skip cleanly), `cargo test --release`, `cargo fmt --all --check`, and both clippy invocations.

- [ ] **Step 7: Prove the sharing test discriminates**

Temporarily change `redis_key` to append a per-instance random suffix, so two handles no longer agree on a key. Run `test_an_entry_written_by_one_handle_is_read_by_another` and confirm it **FAILS**. Restore the file from a copy (not `git checkout --`, which would discard the whole uncommitted task) and confirm it passes. Record both outcomes in your report — a sharing test that cannot fail proves nothing about sharing.

- [ ] **Step 8: Commit**

```bash
git add src/stores/redis_cache.rs src/stores/mod.rs src/stores/namespaces.rs src/plugins/native/proxy_cache.rs
git commit -m "feat(cache): shared response cache over a declared store

proxy-cache gains policy: redis + store:, the shape limit-count already
uses for the same local-or-shared choice. Entries live in one redis hash
each -- status, headers as JSON, body raw -- under a new `cache`
namespace in the registry, so they cannot collide with session, counter,
ACME or policy keys.

The body is stored as raw bytes because redis values are binary-safe:
base64 would inflate every cached response by a third and a JSON wrapper
would escape it on top of that.

Expiry is the key's own TTL. The local backend compares Instants because
it must; a shared one must not, since two instances do not share a
monotonic clock and an entry written by one would be judged against the
other's.

A hash the gateway cannot parse reads as a miss rather than an error: it
is unusable either way, and a miss keeps the request moving."
```

---

### Task 5: `max_object_bytes`, docs and e2e

**Files:**
- Modify: `src/plugins/native/proxy_cache.rs`, `website/docs/reference/plugins/proxy-cache.md`, `website/docs/reference/roadmap.md`
- Create: `e2e/tests/response-cache.spec.ts`
- Modify: `e2e/E2E_TESTBOOK.md`

**Interfaces:**
- Consumes: everything from Tasks 1–4.

- [ ] **Step 1: Write the failing test**

In `proxy_cache.rs`'s test module:

```rust
    /// One large response must not be able to fill a store that sessions,
    /// counters and ACME also live in.
    #[tokio::test]
    async fn test_a_response_over_max_object_bytes_is_not_cached() {
        let cache = Arc::new(crate::traffic::LocalResponseCache::default());
        let plugin = store_plugin_with_cache_and_limit(cache.clone(), 16);

        let mut ctx = test_context();
        ctx.response.status_code = 200;
        ctx.response.body = bytes::Bytes::from(vec![b'x'; 64]);
        plugin.execute(ctx).await.unwrap();

        assert_eq!(cache.len(), 0, "an oversized response must not be stored");
    }

    #[tokio::test]
    async fn test_a_response_within_max_object_bytes_is_cached() {
        let cache = Arc::new(crate::traffic::LocalResponseCache::default());
        let plugin = store_plugin_with_cache_and_limit(cache.clone(), 1024);

        let mut ctx = test_context();
        ctx.response.status_code = 200;
        ctx.response.body = bytes::Bytes::from_static(b"small");
        plugin.execute(ctx).await.unwrap();

        assert_eq!(cache.len(), 1);
    }
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test proxy_cache`
Expected: FAIL — the oversized body is stored, so `len()` is 1 rather than 0.

- [ ] **Step 3: Implement the guard**

Parse the key in `from_config` (`max_object_bytes`, default `1_048_576`), and in the store arm, before building the entry:

```rust
                if ctx.response.body.len() > self.max_object_bytes {
                    // Metered so a route that mysteriously never caches is
                    // explicable rather than mysterious.
                    self.record("too_large");
                } else if self.cache_statuses.contains(&status) {
```

- [ ] **Step 4: Documentation**

In `website/docs/reference/plugins/proxy-cache.md`, add the three keys to the config table (`policy`, `store`, `max_object_bytes`) and a note that says plainly why this node fails open where the rest of the system fails closed:

````markdown
:::note[This cache fails open, unlike the rest of the system]
Sessions and the `store-*` nodes fail **closed**: losing their store means
losing correctness, so the request fails rather than proceeding as though an
absent session said yes.

A cache is different in kind. Its only job is to save a trip to the upstream,
so a backend it cannot reach is treated as a **miss** and the request is served
normally. Turning a redis blip into a `503` on a route that was merely going
faster would be a worse outage than the one it reports.

It is not silent: `gateway_cache_events_total{event="error"}` counts every
failed lookup, and a cache degraded to always-miss is otherwise invisible in
every signal except the upstream's load.
:::
````

Update the `proxy-cache` row on `website/docs/reference/roadmap.md`: the shared-backend follow-up is now implemented; the remaining ones (explicit invalidation, caching streamed responses) stay.

- [ ] **Step 5: e2e scenarios**

Create `e2e/tests/response-cache.spec.ts`, gated on `FEATHERBIT_TEST_REDIS_URL`, following the structure of `e2e/tests/policy-state.spec.ts` (policies via `PUT /api/policies/{name}`, routes via `POST /api/routes`, cleanup in `afterAll`):

- **E2E-CACHE-01** — a route with `policy: redis` serves the first request from the upstream (`x-cache-status: MISS`) and the second from the cache (`HIT`).
- **E2E-CACHE-02** — a second route, different policy, same `store` and same cache key, also gets a `HIT`: two policies sharing one store is the same property two instances rely on.
- **E2E-CACHE-03** — `POST /api/policies/validate` on a `policy: redis` proxy-cache without `store` reports the error naming the supported policies.

Add a row per ID to `e2e/E2E_TESTBOOK.md`, each appearing verbatim in a test title.

- [ ] **Step 6: Run everything**

```
docker run --rm -d --name fb-cache-redis -p 6379:6379 redis:7
cargo test && cargo test --release
cargo fmt --all --check
cargo clippy --all-targets --locked -- -D warnings
cargo clippy --all-targets --no-default-features --locked -- -D warnings
cargo build --release
cd e2e && FEATHERBIT_TEST_REDIS_URL=redis://127.0.0.1:6379 npx playwright test
cd website && npm run build
docker rm -f fb-cache-redis
```

- [ ] **Step 7: Commit**

```bash
git add src/plugins/native/proxy_cache.rs website/ e2e/
git commit -m "feat(cache): cap cached object size, document, and cover end to end

max_object_bytes (default 1 MiB) stops one large response filling a store
that sessions, counters and ACME share. The skip is metered, so a route
that never caches is explicable rather than mysterious.

The plugin page now states plainly that this cache fails open where the
rest of the system fails closed, and why: sessions and store-* protect
correctness, while a cache only protects latency.

E2E-CACHE-01..03 drive a redis-backed cache through real routes,
including two policies sharing one store -- the same property two
instances rely on."
```

---

## Self-Review

**Spec coverage.** §4 (abstraction) → Task 1. §5 (encoding) → Task 4. §6 (failure is a miss) → Task 3, with the test double so it is covered before redis exists. §7.1 (`max_object_bytes`) → Task 5. §7.2 (`max_entries`) → Task 2. §8 (observability) → Task 3. §9 (config/registration) → Task 4 Step 5 and Task 5 Step 4. §10 (testing) → each task's tests, with the cross-instance sharing test in Task 4 and its mutation check in Task 4 Step 7. §3's namespace decision → Task 4 Step 3.

**Placeholder scan.** No TBDs. Task 5 Step 5 describes three e2e scenarios by behaviour rather than transcribing their bodies; the structural template is named (`policy-state.spec.ts`) and each assertion is stated. Task 3 names three test helpers to add beside existing ones rather than repeating the file's fixture boilerplate.

**Type consistency.** `CachedResponse`, `CacheError`, `ResponseCache`, `LocalResponseCache` are defined in Task 1 and used with those names in Tasks 2–5. `set_capacity`/`len` (Task 2) are used by Task 5's assertions. `cache_events` with labels `["backend", "event"]` (Task 3) is used by Task 5's `too_large`. `cache_key`/`redis_key` (Task 4) are used by that task's TTL test and the drift test. The spec calls the response type `CacheEntry`; Task 1 records the rename and the reason.
