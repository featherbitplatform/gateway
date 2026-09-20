# Cache Invalidation Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let an operator, an agent, or a policy purge everything a `proxy-cache` pair has cached, by the pair's `id`, on whichever backend holds it.

**Architecture:** `ResponseCache` gains `purge(id)` (a prefix removal on `{id}\u{1}`). Each `proxy-cache` node exposes its `(id, backend)` through a `Plugin` default method; `compile_policy` collects those onto `CompiledGraph`, so a purge request can find every backend for an `id` without a process-wide registry. Three triggers share one helper: `DELETE /api/cache/{id}`, the MCP `purge_cache` tool, and a `proxy-cache` `phase: purge` node.

**Tech Stack:** Rust, `async_trait`, `dashmap`, `redis` 0.32 (`SCAN` driven manually, `UNLINK`), axum, `rmcp`.

**Spec:** `docs/superpowers/specs/2026-09-20-cache-invalidation-design.md`

## Global Constraints

- **A purge removes exactly the keys beginning `{id}\u{1}`** — the pair `products` must never touch `products-v2`. This is the entire safety argument; two tests pin it (prefix boundary, glob escaping) and a third pins it from the config side (`\u{1}` rejected in `id`).
- **Reads degrade, purges report.** A failed purge exits the node's `error` port / returns `502` from the API. Never silently succeed.
- **Unknown `id` → `404`** on the Admin API and `not_found` from MCP. A typo must not read as a successful flush of nothing.
- **Do not use `redis::AsyncIter`.** It is `#[deprecated]` without the `safe_iterators` feature and fails `-D warnings`. Drive `SCAN` with an explicit cursor loop via `redis::cmd("SCAN")`.
- **Delete with `UNLINK`, in batches of at most 200 keys**, never one giant `DEL`.
- **Everything redis is behind `redis-store`**, and this crate has **no `src/lib.rs`**: dead-code reachability roots at `fn main`, so code lands in the same commit as its first caller and `#[allow(dead_code)]` is never the fix — test-only items take `#[cfg(test)]`.
- **Lint with CI's real commands:** `cargo clippy --all-targets --locked -- -D warnings` AND the same with `--no-default-features`.
- **Run the full `cargo test`** and `cargo test --release`. Gated tests need `FEATHERBIT_TEST_REDIS_URL=redis://127.0.0.1:6379`; a skip is not a pass.
- **Run commands in the foreground** and read their output. Backgrounding `cargo test` has stalled agents on this repo.
- **Mutation checks restore from a file copy, never `git checkout --`**, which discards uncommitted work.
- **Commit style:** Conventional Commits, no `Co-Authored-By`, no AI attribution.
- **Branch:** `feature/cache-invalidation`, off `develop`.

## File Structure

| File | Responsibility |
|---|---|
| `src/traffic/cache.rs` (modify) | `purge` on the trait and on `LocalResponseCache` |
| `src/stores/redis_cache.rs` (modify) | `purge` on `RedisResponseCache`: glob escaping, `SCAN` loop, batched `UNLINK` |
| `src/traffic/purge.rs` (create) | `CacheTarget`, `collect_targets`, `purge_targets` — the one helper all three triggers share |
| `src/plugins/mod.rs` (modify) | `Plugin::cache_target` default method |
| `src/plugins/native/proxy_cache.rs` (modify) | `cache_target` override; `Role::Purge`; `\u{1}` rejection in `id` |
| `src/graph/engine.rs` (modify) | `CompiledGraph.cache_targets` + collection; `validate_cache_pairs` knows the purge role |
| `src/admin/cache.rs` (create) | `DELETE /api/cache/{id}` |
| `src/mcp/tools/cache.rs` (create) + `mod.rs` (modify) | `purge_cache` tool |
| `src/metrics/mod.rs` | no change — `cache_events` already takes an arbitrary `event` label |
| docs + `e2e/tests/response-cache.spec.ts` + `e2e/E2E_TESTBOOK.md` | pages, guides, `E2E-CACHE-04/05` |

---

### Task 1: `purge` on both backends, and the prefix boundary enforced from the config side

**Files:**
- Modify: `src/traffic/cache.rs`, `src/stores/redis_cache.rs`, `src/plugins/native/proxy_cache.rs`

**Interfaces:**
- Consumes: `ResponseCache`, `LocalResponseCache`, `RedisResponseCache` (existing).
- Produces: `async fn purge(&self, id: &str) -> Result<u64, CacheError>` on the trait and both impls; `pub(crate) fn pair_prefix(id: &str) -> String` in `cache.rs`; `pub(crate) fn escape_glob(s: &str) -> String` in `redis_cache.rs`.

- [ ] **Step 1: Write the failing tests**

In `src/traffic/cache.rs`'s test module:

```rust
    /// The prefix boundary is the whole safety argument: purging `products`
    /// must not touch `products-v2`, whose keys share every byte up to the
    /// separator.
    #[tokio::test]
    async fn test_local_purge_removes_only_the_named_pair() {
        let cache = LocalResponseCache::default();
        let ttl = Duration::from_secs(60);
        cache.put(&format!("products\u{1}/a"), &response("a"), ttl).await.unwrap();
        cache.put(&format!("products\u{1}/b"), &response("b"), ttl).await.unwrap();
        cache.put(&format!("products-v2\u{1}/a"), &response("v2"), ttl).await.unwrap();

        let removed = cache.purge("products").await.unwrap();

        assert_eq!(removed, 2);
        assert!(cache.get("products\u{1}/a").await.unwrap().is_none());
        assert!(cache.get("products\u{1}/b").await.unwrap().is_none());
        assert!(
            cache.get("products-v2\u{1}/a").await.unwrap().is_some(),
            "a sibling pair sharing a textual prefix must survive"
        );
    }

    /// Purging a pair that cached nothing is a normal answer, not a failure.
    #[tokio::test]
    async fn test_local_purge_of_an_empty_pair_is_zero_not_an_error() {
        let cache = LocalResponseCache::default();
        assert_eq!(cache.purge("nothing-here").await.unwrap(), 0);
    }
```

In `src/stores/redis_cache.rs` (the ungated unit part — add a plain `#[cfg(test)] mod unit_tests` alongside the gated module if none exists):

```rust
    /// An `id` is free-form config text. Glob metacharacters in it must not
    /// widen a SCAN MATCH: purging the pair literally named `a*` must not
    /// match every pair beginning with `a`.
    #[test]
    fn test_escape_glob_neutralises_every_metacharacter() {
        assert_eq!(escape_glob("plain"), "plain");
        assert_eq!(escape_glob("a*b?c[d]e\\f"), "a\\*b\\?c\\[d\\]e\\\\f");
    }
```

In the gated `redis_cache` live tests:

```rust
    /// More entries than one SCAN page, so the cursor loop and the UNLINK
    /// batching are actually exercised, and a sibling pair to prove the
    /// boundary holds on the shared backend too.
    #[tokio::test]
    async fn test_redis_purge_removes_only_the_named_pair_across_scan_pages() {
        let Some(url) = store_url() else { return };
        let cache = RedisResponseCache::new(client(&url));
        let run = uuid::Uuid::new_v4();
        let target = format!("purge-{run}");
        let sibling = format!("purge-{run}-v2");
        let ttl = Duration::from_secs(60);

        for i in 0..250 {
            cache.put(&format!("{target}\u{1}/{i}"), &response(b"x"), ttl).await.unwrap();
        }
        cache.put(&format!("{sibling}\u{1}/0"), &response(b"keep"), ttl).await.unwrap();

        let removed = cache.purge(&target).await.unwrap();

        assert_eq!(removed, 250, "every entry of the pair, across more than one SCAN page");
        assert!(cache.get(&format!("{target}\u{1}/0")).await.unwrap().is_none());
        assert!(
            cache.get(&format!("{sibling}\u{1}/0")).await.unwrap().is_some(),
            "the sibling pair must be untouched"
        );
    }
```

In `src/plugins/native/proxy_cache.rs`'s test module:

```rust
    /// The separator is the prefix boundary purge relies on. An `id` that
    /// contains it would produce keys another pair's prefix also matches, so
    /// it is refused at policy-compile time rather than trusted not to happen.
    #[test]
    fn test_an_id_containing_the_separator_is_rejected() {
        let r = PluginResources::empty();
        let err = ProxyCachePlugin::from_config(
            &cfg(&[
                ("phase", serde_json::json!("lookup")),
                ("id", serde_json::json!("products\u{1}x")),
            ]),
            &r,
        )
        .unwrap_err();
        assert!(err.contains("control character"), "{err}");
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --no-run 2>&1 | grep "^error"`
Expected: `no method named 'purge'`, `cannot find function 'escape_glob'`. The `\u{1}` test compiles and FAILS (the id is accepted today) — confirm that by running it alone: `cargo test test_an_id_containing_the_separator_is_rejected`.

- [ ] **Step 3: Implement the trait method and the local backend**

There are **three** `impl ResponseCache` blocks in the crate: `LocalResponseCache` (`src/traffic/cache.rs`), `RedisResponseCache` (`src/stores/redis_cache.rs`) and the test double `BrokenCache` in `proxy_cache.rs`'s test module. Adding a required method to the trait breaks all three at once; give `BrokenCache` a `purge` that returns `Err(CacheError("backend down".to_string()))` — Task 3 relies on it.

In `src/traffic/cache.rs`:

```rust
/// The key prefix shared by every entry a `proxy-cache` pair writes.
///
/// `proxy-cache` derives keys as `{id}\u{1}{component}\u{1}…`, so this prefix
/// selects a pair exactly: `products\u{1}` matches nothing of `products-v2`.
pub(crate) fn pair_prefix(id: &str) -> String {
    format!("{id}\u{1}")
}
```

Add to the trait, after `put`:

```rust
    /// Removes every entry belonging to the pair `id` and returns how many.
    ///
    /// "Belonging to" means the key begins with [`pair_prefix`]. `Ok(0)` is a
    /// normal answer: the pair had nothing cached.
    async fn purge(&self, id: &str) -> Result<u64, CacheError>;
```

Implement on `LocalResponseCache`:

```rust
    async fn purge(&self, id: &str) -> Result<u64, CacheError> {
        let prefix = pair_prefix(id);
        let before = self.entries.len();
        self.entries.retain(|key, _| !key.starts_with(&prefix));
        Ok((before - self.entries.len()) as u64)
    }
```

- [ ] **Step 4: Implement the redis backend**

In `src/stores/redis_cache.rs`:

```rust
/// Backslash-escapes every glob metacharacter Redis's `MATCH` understands.
///
/// An `id` is free-form config text. Without this, a pair named `a*` would
/// purge every pair beginning with `a`.
pub(crate) fn escape_glob(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if matches!(c, '*' | '?' | '[' | ']' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// Keys per `UNLINK`. Small enough that no single round trip holds the
/// server long; large enough that a big purge is not thousands of them.
const UNLINK_BATCH: usize = 200;
```

and on `RedisResponseCache`:

```rust
    async fn purge(&self, id: &str) -> Result<u64, CacheError> {
        let mut conn = self.client.conn().await.map_err(CacheError)?;
        let pattern = format!(
            "{}*",
            self.redis_key(&crate::traffic::cache::pair_prefix(&escape_glob(id)))
        );

        // SCAN driven by hand rather than through `redis::AsyncIter`: that
        // type is deprecated without the `safe_iterators` feature and fails
        // `-D warnings`. The explicit loop also makes the batching visible.
        let mut cursor: u64 = 0;
        let mut removed: u64 = 0;
        loop {
            let (next, keys): (u64, Vec<String>) = redis::cmd("SCAN")
                .arg(cursor)
                .arg("MATCH")
                .arg(&pattern)
                .arg("COUNT")
                .arg(UNLINK_BATCH)
                .query_async(&mut conn)
                .await
                .map_err(|e| CacheError(e.to_string()))?;

            for chunk in keys.chunks(UNLINK_BATCH) {
                let n: u64 = redis::cmd("UNLINK")
                    .arg(chunk)
                    .query_async(&mut conn)
                    .await
                    .map_err(|e| CacheError(e.to_string()))?;
                removed += n;
            }

            if next == 0 {
                break;
            }
            cursor = next;
        }
        Ok(removed)
    }
```

**Escaping order matters:** escape the `id`, *then* build the prefix and the redis key around it. The `\u{1}` separator and the `:` in the key prefix are not glob metacharacters, so they pass through unchanged.

- [ ] **Step 5: Reject the separator in `id`**

In `proxy_cache.rs`'s `from_config`, immediately after the `id` is parsed (around line 174):

```rust
        // The `\u{1}` separator is the prefix boundary that purging a pair
        // relies on: an `id` containing it would produce keys another pair's
        // prefix also matches. Refuse it -- and every other control character,
        // which have no business in a cache namespace -- at compile time
        // rather than trust config never to contain one.
        if id.chars().any(char::is_control) {
            return Err(format!(
                "proxy-cache: 'id' must not contain control characters (got {:?})",
                id
            ));
        }
```

- [ ] **Step 6: Run the tests to verify they pass**

Run: `FEATHERBIT_TEST_REDIS_URL=redis://127.0.0.1:6379 cargo test` (a redis must be running: `docker run --rm -d --name fb-inv-redis -p 6379:6379 redis:7`). Confirm the gated purge test **executed**, then the full matrix: plain `cargo test`, `cargo test --release`, both clippy invocations, `cargo fmt --all --check`.

- [ ] **Step 7: Prove the boundary test discriminates**

Copy `src/traffic/cache.rs` to a scratch file. Change `pair_prefix` to `format!("{id}")` (drop the separator), run `test_local_purge_removes_only_the_named_pair`, and confirm it **FAILS** — `products-v2` would now be purged too. Restore from the copy. Record both outcomes in your report.

- [ ] **Step 8: Commit**

```bash
git add src/traffic/cache.rs src/stores/redis_cache.rs src/plugins/native/proxy_cache.rs
git commit -m "feat(cache): purge a proxy-cache pair by id on both backends

Every key a pair writes begins with {id}\u{1}, so purging a pair is a
prefix removal: a retain on the local map, and SCAN MATCH + batched UNLINK
on redis. Glob metacharacters in the id are escaped so a pair literally
named 'a*' cannot widen the match to every pair beginning with 'a'.

SCAN is driven by hand rather than through redis::AsyncIter, which is
deprecated without the safe_iterators feature and fails -D warnings; the
explicit cursor loop also makes the batching visible.

Close the hole the design's own safety argument had: from_config now
rejects an id containing the separator (or any control character). An id
of 'products\u{1}x' would otherwise produce keys that the prefix
'products\u{1}' also matches, so purging one pair would take another's
entries with it. The boundary is enforced, not assumed."
```

---

### Task 2: Discovery — every backend that holds an `id`

**Files:**
- Create: `src/traffic/purge.rs`
- Modify: `src/traffic/mod.rs`, `src/plugins/mod.rs`, `src/plugins/native/proxy_cache.rs`, `src/graph/engine.rs`

**Interfaces:**
- Consumes: `ResponseCache::purge` (Task 1); `CompiledGraph`, `SharedState.routes: RwLock<Vec<(RouteConfig, Arc<CompiledGraph>)>>`.
- Produces:
  - `pub struct CacheTarget { pub id: String, pub backend: Arc<dyn ResponseCache>, pub backend_label: &'static str, pub store: String }` (in `traffic/purge.rs`)
  - `Plugin::cache_target(&self) -> Option<CacheTarget>` default `None`
  - `CompiledGraph::cache_targets(&self) -> &[CacheTarget]`
  - `pub fn collect_targets(graphs: &[Arc<CompiledGraph>], id: &str) -> Vec<CacheTarget>` — deduplicated by `(backend_label, store)`
  - `pub struct PurgeOutcome { pub backend_label: &'static str, pub store: String, pub removed: u64 }`
  - `pub async fn purge_targets(targets: &[CacheTarget]) -> Result<Vec<PurgeOutcome>, (Vec<PurgeOutcome>, CacheError)>` — on failure returns what succeeded before the failure

- [ ] **Step 1: Write the failing tests**

In `src/traffic/purge.rs` (test module only, for now):

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::engine::compile_policy;
    use crate::plugins::resources::PluginResources;

    fn graph(json: serde_json::Value) -> Arc<CompiledGraph> {
        let mut value = json;
        if let serde_json::Value::Object(ref mut map) = value {
            map.entry("name").or_insert_with(|| serde_json::json!("p"));
        }
        let policy = serde_json::from_value(value).unwrap();
        Arc::new(compile_policy(&policy, PluginResources::empty()).unwrap())
    }

    fn local_pair_policy(cache_id: &str) -> serde_json::Value {
        serde_json::json!({
            "nodes": [
                { "id": "listener", "type": "listener", "config": {} },
                { "id": "look", "type": "proxy-cache",
                  "config": { "phase": "lookup", "id": cache_id, "policy": "local" } },
                { "id": "up", "type": "upstream",
                  "config": { "targets": [{ "host": "h", "port": 80 }] } },
                { "id": "keep", "type": "proxy-cache",
                  "config": { "phase": "store", "id": cache_id, "policy": "local" } },
                { "id": "client", "type": "client", "config": {} }
            ],
            "edges": [
                { "from": "listener.out", "to": "look.in" },
                { "from": "look.success", "to": "up.in" },
                { "from": "look.hit", "to": "client.in" },
                { "from": "up.success", "to": "keep.in" },
                { "from": "keep.success", "to": "client.in" },
                { "from": "keep.hit", "to": "client.in" }
            ]
        })
    }

    /// Both halves of a local pair, and two policies with the same pair,
    /// all share ONE LocalResponseCache -- so a purge must hit it once.
    /// Without deduplication the shared cache would be purged per half, and
    /// the removed count would be nonsense.
    #[tokio::test]
    async fn test_collect_targets_deduplicates_the_shared_local_cache() {
        let a = graph(local_pair_policy("products"));
        let b = graph(local_pair_policy("products"));
        let targets = collect_targets(&[a, b], "products");
        assert_eq!(targets.len(), 1, "two policies, four halves, one local cache");
        assert_eq!(targets[0].backend_label, "local");
    }

    /// An id no pair uses yields nothing -- which the API turns into a 404
    /// rather than a successful flush of nothing.
    #[tokio::test]
    async fn test_collect_targets_for_an_unknown_id_is_empty() {
        let g = graph(local_pair_policy("products"));
        assert!(collect_targets(&[g], "prodcuts").is_empty());
    }

    /// The whole point: a purge through the collected target removes what
    /// the pair cached.
    #[tokio::test]
    async fn test_purge_targets_removes_the_pairs_entries() {
        let g = graph(local_pair_policy("products"));
        let targets = collect_targets(&[g], "products");
        let cache = targets[0].backend.clone();
        cache
            .put("products\u{1}/x", &crate::traffic::CachedResponse {
                status: 200, headers: Default::default(), body: bytes::Bytes::from_static(b"x"),
            }, std::time::Duration::from_secs(60))
            .await
            .unwrap();

        let outcomes = purge_targets(&targets).await.unwrap();

        assert_eq!(outcomes.len(), 1);
        assert_eq!(outcomes[0].removed, 1);
        assert!(cache.get("products\u{1}/x").await.unwrap().is_none());
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --no-run 2>&1 | grep "^error"`
Expected: `cannot find function 'collect_targets'`, `cannot find type 'CacheTarget'`.

- [ ] **Step 3: Implement the shared helper**

`src/traffic/purge.rs`, above the tests:

```rust
//! Finding and purging every backend that holds entries for a cache pair.
//!
//! Shared by all three triggers -- the Admin API, the MCP tool and the
//! `phase: purge` node -- so they cannot drift on what "purge `products`"
//! means.

use std::sync::Arc;

use crate::graph::engine::CompiledGraph;
use crate::traffic::cache::{CacheError, ResponseCache};

/// One `proxy-cache` half's backend, as it described itself at compile time.
#[derive(Clone)]
pub struct CacheTarget {
    /// The pair id this half belongs to.
    pub id: String,
    pub backend: Arc<dyn ResponseCache>,
    /// `"local"` or `"redis"`, for the response and the metric label.
    pub backend_label: &'static str,
    /// The declared store's name for `redis`; empty for `local`.
    pub store: String,
}

/// What one backend reported after a purge.
#[derive(Debug, Clone, serde::Serialize)]
pub struct PurgeOutcome {
    #[serde(rename = "backend")]
    pub backend_label: &'static str,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub store: String,
    pub removed: u64,
}

/// Every distinct backend holding entries for `id`, across all compiled graphs.
///
/// Deduplicated by `(backend_label, store)`: every `policy: local` half in the
/// process shares one `LocalResponseCache`, and two policies can point at the
/// same redis store. Purging a shared backend once per half would repeat the
/// work and report a count that means nothing.
pub fn collect_targets(graphs: &[Arc<CompiledGraph>], id: &str) -> Vec<CacheTarget> {
    let mut out: Vec<CacheTarget> = Vec::new();
    for g in graphs {
        for t in g.cache_targets().iter().filter(|t| t.id == id) {
            let dup = out
                .iter()
                .any(|o| o.backend_label == t.backend_label && o.store == t.store);
            if !dup {
                out.push(t.clone());
            }
        }
    }
    out
}

/// Purges each target in turn.
///
/// On failure, returns what succeeded before it alongside the error, so the
/// caller can report both -- a half-completed purge is worth knowing about.
pub async fn purge_targets(
    targets: &[CacheTarget],
) -> Result<Vec<PurgeOutcome>, (Vec<PurgeOutcome>, CacheError)> {
    let mut done = Vec::with_capacity(targets.len());
    for t in targets {
        match t.backend.purge(&t.id).await {
            Ok(removed) => done.push(PurgeOutcome {
                backend_label: t.backend_label,
                store: t.store.clone(),
                removed,
            }),
            Err(e) => return Err((done, e)),
        }
    }
    Ok(done)
}
```

Add `pub mod purge;` and `pub use purge::{CacheTarget, PurgeOutcome, collect_targets, purge_targets};` to `src/traffic/mod.rs`.

- [ ] **Step 4: The `Plugin` default method and the override**

In `src/plugins/mod.rs`, beside `reads_response_body`:

```rust
    /// The cache backend this node writes to, if it is a `proxy-cache` half.
    ///
    /// Consulted at policy-compile time so an invalidation request can find
    /// every backend that holds entries for a pair `id` -- the same shape as
    /// `reads_response_body`: the node describes itself, the compiler
    /// collects the answers, and there is no process-wide registry that would
    /// have to survive hot-reloads.
    fn cache_target(&self) -> Option<crate::traffic::CacheTarget> {
        None
    }
```

In `proxy_cache.rs`'s `impl Plugin`:

```rust
    fn cache_target(&self) -> Option<crate::traffic::CacheTarget> {
        Some(crate::traffic::CacheTarget {
            id: self.id.clone(),
            backend: self.cache.clone(),
            backend_label: self.backend_label,
            store: self.store_label.clone(),
        })
    }
```

- [ ] **Step 5: Collect onto the graph**

In `src/graph/engine.rs`: add `cache_targets: Vec<crate::traffic::CacheTarget>` to `CompiledGraph`, an accessor beside `cache_pair_warnings()`:

```rust
    /// Every `proxy-cache` half's backend, for invalidation by pair `id`.
    pub fn cache_targets(&self) -> &[crate::traffic::CacheTarget] {
        &self.cache_targets
    }
```

and, in `compile_policy` after the node map is built:

```rust
    let cache_targets: Vec<_> = nodes.values().filter_map(|n| n.cache_target()).collect();
```

There are **three** `CompiledGraph { … }` construction sites in the file (one in `compile_policy`, two in tests); every one needs the field — the previous plan missed two and the build failed.

- [ ] **Step 6: Run the tests and the full matrix**

Run: `cargo test traffic::purge` → 3 passed. Then the full matrix (both clippy invocations, `cargo test`, `--release`, fmt).

- [ ] **Step 7: Commit**

```bash
git add src/traffic/purge.rs src/traffic/mod.rs src/plugins/mod.rs src/plugins/native/proxy_cache.rs src/graph/engine.rs
git commit -m "feat(cache): find every backend that holds a pair id

An id alone does not say where a pair caches: two policies named
products can use different backends, and every policy: local pair shares
one LocalResponseCache. Each proxy-cache half now describes its backend
through a Plugin default method, the way reads_response_body already lets
a node describe itself, and compile_policy collects the answers onto the
graph. No process-wide registry, so nothing to leak or go stale across
hot-reloads.

collect_targets deduplicates by (backend, store): without it the shared
local cache would be purged once per half and report a count that means
nothing. purge_targets is the single helper the Admin API, the MCP tool
and the purge node will all call, so they cannot drift on what 'purge
products' means."
```

---

### Task 3: The `phase: purge` node

**Files:**
- Modify: `src/plugins/native/proxy_cache.rs`, `src/graph/engine.rs`, `website/docs/reference/plugins/proxy-cache.md`

**Interfaces:**
- Consumes: `ResponseCache::purge` (Task 1), `record()` (existing).
- Produces: `Role::Purge`; `phase: purge` accepted by `from_config`.

- [ ] **Step 1: Write the failing tests**

In `proxy_cache.rs`'s test module:

```rust
    /// A write route that ends in a purge half clears what the read route's
    /// pair cached, so the next read misses instead of serving the old value.
    #[tokio::test]
    async fn test_purge_phase_clears_its_pair() {
        let cache = Arc::new(crate::traffic::LocalResponseCache::default());
        let entry = crate::traffic::CachedResponse {
            status: 200,
            headers: HashMap::new(),
            body: bytes::Bytes::from_static(b"x"),
        };
        cache
            .put("cat\u{1}/x", &entry, std::time::Duration::from_secs(60))
            .await
            .unwrap();
        let plugin = purge_plugin_with_cache(cache.clone());

        let out = plugin.execute(test_context()).await.unwrap();

        assert_eq!(out.port, None, "a completed purge continues on success");
        assert!(cache.get("cat\u{1}/x").await.unwrap().is_none());
    }

    /// Reads degrade, purges report. A purge that could not reach its backend
    /// leaves the cache stale in exactly the situation invalidation exists to
    /// fix, so it takes the error port rather than continuing silently.
    #[tokio::test]
    async fn test_purge_phase_against_a_failing_backend_exits_error() {
        let plugin = purge_plugin_with_cache(Arc::new(BrokenCache));
        let result = plugin.execute(test_context()).await;
        assert!(result.is_err(), "a failed purge must not read as success");
        assert_eq!(result.unwrap_err().error.code, "CACHE_PURGE_FAILED");
    }
```

Add `purge_plugin_with_cache` beside the existing `lookup_plugin_with_cache` helper (`proxy_cache.rs:520`), building a plugin with `("phase", "purge")` and `("id", "cat")`. `BrokenCache` (same module, ~line 692) already has a failing `purge` from Task 1.

In `src/graph/engine.rs`'s tests:

```rust
    /// A purge half is held to the same agreement rule as the other two: a
    /// purge pointed at a different backend than its pair clears nothing.
    #[tokio::test]
    async fn test_a_purge_half_split_from_its_pair_is_rejected() {
        let err = compile_test_policy_err(serde_json::json!({
            "nodes": [
                { "id": "listener", "type": "listener", "config": {} },
                { "id": "look", "type": "proxy-cache",
                  "config": { "phase": "lookup", "id": "products", "policy": "local" } },
                { "id": "up", "type": "upstream",
                  "config": { "targets": [{ "host": "h", "port": 80 }] } },
                { "id": "drop", "type": "proxy-cache",
                  "config": { "phase": "purge", "id": "products", "policy": "redis", "store": "s" } },
                { "id": "client", "type": "client", "config": {} }
            ],
            "edges": [
                { "from": "listener.out", "to": "look.in" },
                { "from": "look.success", "to": "up.in" },
                { "from": "look.hit", "to": "client.in" },
                { "from": "up.success", "to": "drop.in" },
                { "from": "drop.success", "to": "client.in" },
                { "from": "drop.hit", "to": "client.in" }
            ]
        }));
        assert!(err.contains("products") && err.contains("drop"), "{err}");
    }

    /// A purge with nothing to purge for is useless but not wrong -- reported
    /// like any other lone half.
    #[tokio::test]
    async fn test_a_lone_purge_half_is_reported() {
        let graph = compile_test_policy(serde_json::json!({
            "nodes": [
                { "id": "listener", "type": "listener", "config": {} },
                { "id": "up", "type": "upstream",
                  "config": { "targets": [{ "host": "h", "port": 80 }] } },
                { "id": "drop", "type": "proxy-cache",
                  "config": { "phase": "purge", "id": "orphan", "policy": "local" } },
                { "id": "client", "type": "client", "config": {} }
            ],
            "edges": [
                { "from": "listener.out", "to": "up.in" },
                { "from": "up.success", "to": "drop.in" },
                { "from": "drop.success", "to": "client.in" },
                { "from": "drop.hit", "to": "client.in" }
            ]
        }));
        let w = graph.cache_pair_warnings();
        assert_eq!(w.len(), 1);
        assert_eq!(w[0].present_node_id, "drop");
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --no-run 2>&1 | grep "^error"`
Expected: `cannot find function 'purge_plugin_with_cache'`; the engine tests compile and FAIL because `phase: purge` is currently rejected by `from_config` ("unknown phase/role").

- [ ] **Step 3: Implement the role**

In `proxy_cache.rs`: add `Purge` to `Role` with the doc `/// Runs on a write path: invalidates everything its pair cached.`; accept `"purge"` where `"lookup"` and `"store"` are matched in `from_config`; update the two error strings that enumerate the accepted values to `'lookup', 'store' or 'purge'`.

Add the `execute` arm beside `Role::Lookup` and `Role::Store`:

```rust
            Role::Purge => {
                // Reads degrade, purges report. A lookup that cannot reach its
                // backend becomes a miss, because a cache exists to save latency.
                // A purge is different in kind: the caller asked for state to
                // change, and silently continuing would leave the cache stale
                // in exactly the situation invalidation exists to fix.
                match self.cache.purge(&self.id).await {
                    Ok(removed) => {
                        tracing::info!(id = %self.id, removed, "proxy-cache purged pair");
                        self.record("purge");
                        Ok(PluginOutput::success(ctx))
                    }
                    Err(e) => {
                        tracing::warn!(id = %self.id, "proxy-cache purge failed: {e}");
                        self.record("error");
                        Err(PluginExecutionError {
                            context: ctx,
                            error: crate::context::GatewayError {
                                node_id: String::new(),
                                code: "CACHE_PURGE_FAILED".to_string(),
                                message: format!("proxy-cache: purging pair '{}' failed: {e}", self.id),
                                metadata: HashMap::new(),
                            },
                        })
                    }
                }
            }
```

`reads_response_body` already returns `false` for every role; no change.

- [ ] **Step 4: Teach the validator the third role**

In `validate_cache_pairs` (`src/graph/engine.rs`), the disagreement check already walks every half in a group, so a purge half is covered as soon as `from_config` accepts it. Only the lone-half logic needs the new role: a group is *complete* when it has both a lookup and a store; a purge alone is a lone half. Change the block starting `let has_lookup = …`:

```rust
        let has_lookup = group.iter().any(|h| h.role == "lookup");
        let has_store = group.iter().any(|h| h.role == "store");
        if !(has_lookup && has_store) {
            // Whatever IS present (a lone lookup, a lone store, or a purge
            // with nothing to purge for) is reported; the first missing role
            // names what would complete it.
            let present = group.first().expect("groups are non-empty");
            warnings.push(CachePairWarning {
                cache_id: cache_id.to_string(),
                present_node_id: present.node_id.to_string(),
                missing_role: if !has_lookup { "lookup" } else { "store" }.to_string(),
            });
        }
```

Check the existing `test_a_lone_cache_half_is_reported_not_rejected` still passes — it asserts `present_node_id == "look"` and `missing_role == "store"`, which this preserves.

- [ ] **Step 5: Docs**

In `website/docs/reference/plugins/proxy-cache.md`: add `purge` to the `phase` row; a section:

````markdown
## Invalidating on a write

A third phase, `purge`, clears everything its pair has cached. Put it on the
route that changes the resource, after the upstream:

```yaml
- id: drop-cache
  type: proxy-cache
  config: { phase: purge, id: products, policy: redis, store: sessions }
```

wired `forward-write.success → drop-cache.in`. Gating on the upstream's status
is yours to decide — a `condition` on `status` before it, if only a `2xx` should
purge.

**A failed purge takes `error`, unlike a failed lookup.** A lookup that cannot
reach its backend becomes a miss, because a cache only saves latency. A purge is
different: you asked for state to change, and continuing silently would leave the
cache stale in exactly the case invalidation exists to fix.

**A `policy: local` purge clears this instance only.** No message reaches other
instances. `policy: redis` purges are cluster-wide because the store is shared.

The purge half is held to the same agreement rule as the other two: it must use
the same `policy` and `store` as its pair, or the compiler rejects the policy.

`PortSpec` is per node *type*, so a `phase: purge` node — like `phase: store` —
must wire a `hit` port that never fires.
````

- [ ] **Step 6: Run the full matrix and commit**

```bash
git add src/plugins/native/proxy_cache.rs src/graph/engine.rs website/docs/reference/plugins/proxy-cache.md
git commit -m "feat(cache): phase: purge invalidates a pair on a write path

A TTL cannot cover the one case that matters most: a write to the
resource a cache serves. The staleness window is exactly the TTL, every
time. A purge half on the write route clears the pair on the way through,
so the next read is fresh.

Reads degrade, purges report. A lookup that cannot reach its backend
becomes a miss, because a cache exists to save latency. A purge is
different in kind: the caller asked for state to change, and silently
continuing would leave the cache stale in precisely the situation
invalidation exists to fix. It exits the error port.

The purge half joins the pair-agreement check: a purge pointed at a
different backend than its pair would clear nothing, silently."
```

---

### Task 4: `DELETE /api/cache/{id}`

**Files:**
- Create: `src/admin/cache.rs`
- Modify: `src/admin/mod.rs`

**Interfaces:**
- Consumes: `collect_targets`, `purge_targets` (Task 2).
- Produces: `pub fn router() -> Router<Arc<SharedState>>`.

- [ ] **Step 1: Write the implementation** (behaviour is unit-tested through the helper in Task 2 and through the MCP tool in Task 5, which share this exact logic; the HTTP surface is covered by `E2E-CACHE-04` in Task 6)

`src/admin/cache.rs`:

```rust
//! `DELETE /api/cache/{id}` -- purge everything a `proxy-cache` pair cached.
//!
//! A runtime action, like `DELETE /api/debug/traces` and
//! `POST /api/stores/{name}/ping`: it changes no configuration.

use std::sync::Arc;

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
    routing::delete,
    Json, Router,
};

use crate::state::SharedState;
use crate::traffic::{collect_targets, purge_targets};

pub fn router() -> Router<Arc<SharedState>> {
    Router::new().route("/api/cache/{id}", delete(purge_cache))
}

/// Purges the pair `id` on every backend that holds it.
///
/// `404` when no compiled policy has a pair with that id: a typo must not
/// read as a successful flush of nothing. `502` when a backend could not be
/// reached, naming it and listing what did succeed first.
async fn purge_cache(
    State(state): State<Arc<SharedState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let graphs: Vec<_> = state
        .routes
        .read()
        .await
        .iter()
        .map(|(_, g)| g.clone())
        .collect();
    let targets = collect_targets(&graphs, &id);

    if targets.is_empty() {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({
                "error": "not_found",
                "message": format!("no proxy-cache pair with id '{id}' in any policy"),
            })),
        )
            .into_response();
    }

    match purge_targets(&targets).await {
        Ok(purged) => Json(serde_json::json!({ "id": id, "purged": purged })).into_response(),
        Err((purged, e)) => (
            StatusCode::BAD_GATEWAY,
            Json(serde_json::json!({
                "error": "cache_purge_failed",
                "message": e.to_string(),
                "id": id,
                "purged": purged,
            })),
        )
            .into_response(),
    }
}
```

`state.routes` is a `tokio::sync::RwLock` (`src/state.rs:36`; `src/admin/status.rs:47` reads it with `.read().await`), so the `.await` above is right.

Register it in `src/admin/mod.rs` with `.merge(cache::router())` beside the others, and `mod cache;`.

- [ ] **Step 2: Build and run the matrix**

The handler has no unit test of its own by design (its two halves are tested in Tasks 2 and 5, and the HTTP surface in Task 6); the matrix must still be clean — both clippy invocations especially, since a new `pub fn router` with no caller would be dead code, and the `.merge` is its caller.

- [ ] **Step 3: Commit**

```bash
git add src/admin/cache.rs src/admin/mod.rs
git commit -m "feat(admin): DELETE /api/cache/{id} purges a proxy-cache pair

A runtime action, like clearing traces or pinging a store: it changes no
configuration. Finds every backend holding the pair through the targets
compile_policy collected, purges each, and reports per-backend counts.

404 when no policy has a pair with that id. Without it, a typo returns an
empty purged array and the operator walks away believing the cache is
flushed. 502 when a backend could not be reached, listing what succeeded
before it -- a half-completed purge is worth knowing about."
```

---

### Task 5: MCP `purge_cache`

**Files:**
- Create: `src/mcp/tools/cache.rs`
- Modify: `src/mcp/tools/mod.rs`

**Interfaces:**
- Consumes: `collect_targets`, `purge_targets` (Task 2); `ToolError::not_found`, `ToolDef`, `schema_of` (existing).
- Produces: the `purge_cache` tool, scope `Write`.

- [ ] **Step 1: Write the failing tests**

In `src/mcp/tools/cache.rs`, a test module using the same `test_state()` / `call` / `obj` helpers the `debug.rs` tests use (import them from wherever that module defines them):

```rust
    /// dry_run shows what a purge would hit and removes nothing -- an agent
    /// should be able to see the blast radius before committing to it.
    #[tokio::test]
    async fn purge_cache_dry_run_lists_targets_and_removes_nothing() {
        let s = state_with_local_cache_pair("products").await;
        seed(&s, "products\u{1}/x").await;

        let v = call(&s, "purge_cache", obj(serde_json::json!({ "id": "products", "dry_run": true })))
            .await
            .unwrap();

        assert_eq!(v["purged"].as_array().unwrap().len(), 1);
        assert!(v["purged"][0].get("removed").is_none(), "dry_run must not report a count: {v}");
        assert!(still_cached(&s, "products\u{1}/x").await, "dry_run must not delete");
    }

    #[tokio::test]
    async fn purge_cache_removes_and_counts() {
        let s = state_with_local_cache_pair("products").await;
        seed(&s, "products\u{1}/x").await;

        let v = call(&s, "purge_cache", obj(serde_json::json!({ "id": "products" })))
            .await
            .unwrap();

        assert_eq!(v["purged"][0]["removed"], 1);
        assert!(!still_cached(&s, "products\u{1}/x").await);
    }

    /// The same 404 the Admin API gives: a typo is not a successful flush.
    #[tokio::test]
    async fn purge_cache_unknown_id_is_not_found() {
        let s = state_with_local_cache_pair("products").await;
        let err = call(&s, "purge_cache", obj(serde_json::json!({ "id": "prodcuts" })))
            .await
            .unwrap_err();
        assert!(err.message.contains("prodcuts"), "{}", err.message);
    }
```

The helpers, in the same test module (`call`, `obj` and `state` come from `crate::mcp::tools::{call, test_support::{obj, state}}`, exactly as `debug.rs`'s tests import them):

```rust
    const LOCAL_PAIR_GATEWAY: &str = r#"
routes:
  - name: products
    match: { path: /products }
    policy: products-policy
policies:
  - name: products-policy
    nodes:
      - { id: listener, type: listener, config: {} }
      - { id: look, type: proxy-cache, config: { phase: lookup, id: products, policy: local } }
      - { id: up, type: upstream, config: { targets: [{ host: h, port: 80 }] } }
      - { id: keep, type: proxy-cache, config: { phase: store, id: products, policy: local } }
      - { id: client, type: client, config: {} }
    edges:
      - { from: listener.out, to: look.in }
      - { from: look.success, to: up.in }
      - { from: look.hit, to: client.in }
      - { from: up.success, to: keep.in }
      - { from: keep.success, to: client.in }
      - { from: keep.hit, to: client.in }
"#;

    async fn state_with_local_cache_pair(_id: &str) -> Arc<SharedState> {
        state("{}", LOCAL_PAIR_GATEWAY)
    }

    /// The backend the compiled pair actually uses -- found the same way the
    /// tool finds it, so the test seeds exactly where the purge will look.
    async fn backend(s: &SharedState) -> Arc<dyn crate::traffic::ResponseCache> {
        let graphs: Vec<_> = s.routes.read().await.iter().map(|(_, g)| g.clone()).collect();
        crate::traffic::collect_targets(&graphs, "products").remove(0).backend
    }

    async fn seed(s: &SharedState, key: &str) {
        let entry = crate::traffic::CachedResponse {
            status: 200,
            headers: Default::default(),
            body: bytes::Bytes::from_static(b"x"),
        };
        backend(s).await.put(key, &entry, std::time::Duration::from_secs(60)).await.unwrap();
    }

    async fn still_cached(s: &SharedState, key: &str) -> bool {
        backend(s).await.get(key).await.unwrap().is_some()
    }
```

`state.routes` is a `tokio::sync::RwLock` (`src/state.rs:36`); `.read().await` is right.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --no-run 2>&1 | grep "^error"` — the tool does not exist yet.

- [ ] **Step 3: Implement the tool**

```rust
//! `purge_cache` -- the Admin API's DELETE /api/cache/{id}, for agents.

use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

use super::ToolError;
use crate::state::SharedState;
use crate::traffic::{collect_targets, purge_targets};

#[derive(Debug, Deserialize, JsonSchema)]
pub struct PurgeCacheArgs {
    /// The `proxy-cache` pair id (the `id` config key shared by its halves).
    pub id: String,
    /// List the backends a purge would hit without deleting anything. Default false.
    #[serde(default)]
    pub dry_run: bool,
}

pub async fn purge_cache(state: &SharedState, a: PurgeCacheArgs) -> Result<Value, ToolError> {
    let graphs: Vec<_> = state.routes.read().await.iter().map(|(_, g)| g.clone()).collect();
    let targets = collect_targets(&graphs, &a.id);
    if targets.is_empty() {
        return Err(ToolError::not_found("proxy-cache pair", &a.id));
    }

    if a.dry_run {
        let would: Vec<Value> = targets
            .iter()
            .map(|t| {
                let mut v = serde_json::json!({ "backend": t.backend_label });
                if !t.store.is_empty() {
                    v["store"] = Value::String(t.store.clone());
                }
                v
            })
            .collect();
        return Ok(serde_json::json!({ "id": a.id, "dry_run": true, "purged": would }));
    }

    match purge_targets(&targets).await {
        Ok(purged) => Ok(serde_json::json!({ "id": a.id, "purged": purged })),
        Err((purged, e)) => {
            let mut err = ToolError::invalid_config(vec![e.to_string()]);
            err.message = format!("purging pair '{}' failed on a backend", a.id);
            err.hint = Some(format!("{} backend(s) were purged before the failure", purged.len()));
            Err(err)
        }
    }
}
```

Register in `src/mcp/tools/mod.rs`: `pub mod cache;`, a dispatch arm `"purge_cache" => cache::purge_cache(state, args(a)?).await,` beside `"delete_store"`, and a `ToolDef`:

```rust
    ToolDef { name: "purge_cache", scope: Write, description: "Purge everything a proxy-cache pair has cached, by its id, on every backend that holds it. A policy: local purge clears this instance only. dry_run lists the backends without deleting.", input_schema: schema_of::<cache::PurgeCacheArgs> },
```

- [ ] **Step 4: Run the tests and the full matrix, then commit**

```bash
git add src/mcp/tools/cache.rs src/mcp/tools/mod.rs
git commit -m "feat(mcp): purge_cache tool with dry_run

The Admin API's DELETE /api/cache/{id}, for agents, with the same helper
underneath so the two cannot drift. Write scope.

dry_run returns the backends a purge would hit and deletes nothing. An
agent should be able to see the blast radius before committing to it,
the way every other write tool here already offers.

An unknown id is not_found rather than an empty result: a typo is not a
successful flush of nothing."
```

---

### Task 6: Docs, observability note, and e2e

**Files:**
- Modify: `website/docs/guides/admin-api.md`, `website/docs/guides/observability.md`, `website/docs/reference/roadmap.md`, `e2e/tests/response-cache.spec.ts`, `e2e/E2E_TESTBOOK.md`, `docs/superpowers/notes/2026-09-15-follow-ups.md`

- [ ] **Step 1: Docs**

- `admin-api.md`: a `DELETE /api/cache/{id}` entry with the response shape, the `404`/`502` semantics, and the sentence *"A `policy: local` purge clears the instance that received the request only; `policy: redis` purges are cluster-wide because the store is shared."*
- `observability.md`: in the `gateway_cache_events_total` row, add `purge` to the events and note it counts **operations**, not entries removed.
- `roadmap.md`: the `proxy-cache` row's follow-ups drop "explicit invalidation"; add one line describing the three triggers.
- The follow-ups memo: mark the invalidation item fixed.

- [ ] **Step 2: e2e**

In `e2e/tests/response-cache.spec.ts`, reusing its `cachePolicy` helper and fixture store, add:

- **`E2E-CACHE-04`** — populate through the cached route (`MISS` then `HIT`), `DELETE /api/cache/{CACHE_ID}` via `adminApi()`, assert `200` with a `purged` array, then the next request is a `MISS` again. Also assert `DELETE /api/cache/no-such-pair` is `404`.
- **`E2E-CACHE-05`** — a second route whose policy is `upstream → proxy-cache(phase: purge, same id/policy/store) → client`. Populate the cached route to a `HIT`, hit the purge route once, assert the cached route is a `MISS` again. This is write-through invalidation, the case a TTL cannot cover.

Both gated on `FEATHERBIT_TEST_REDIS_URL` like their siblings. Add a row per ID to `e2e/E2E_TESTBOOK.md`; every ID must appear verbatim in a test title.

- [ ] **Step 3: Run everything**

```
docker run --rm -d --name fb-inv-redis -p 6379:6379 redis:7   # if not already up
FEATHERBIT_TEST_REDIS_URL=redis://127.0.0.1:6379 cargo test && cargo test && cargo test --release
cargo clippy --all-targets --locked -- -D warnings
cargo clippy --all-targets --no-default-features --locked -- -D warnings
cargo fmt --all --check
cd website && npm run build && cd ..
cargo build --release            # no UI files change in this plan, so ui/dist need not be rebuilt
cd e2e && FEATHERBIT_TEST_REDIS_URL=redis://127.0.0.1:6379 npx playwright test
docker rm -f fb-inv-redis
```

Expected e2e: **154 passed**, 1 skipped (ACME/Pebble).

- [ ] **Step 4: Commit**

```bash
git add website/ e2e/ docs/superpowers/notes/2026-09-15-follow-ups.md
git commit -m "docs(cache): invalidation across the Admin API, MCP and a purge node

E2E-CACHE-04 drives DELETE /api/cache/{id} through a real cached route --
HIT, purge, MISS -- and asserts an unknown id is 404. E2E-CACHE-05 is the
case a TTL cannot cover: a write route ending in a phase: purge node
clears the read route's cache on the way through."
```

---

## Self-Review

**Spec coverage.** §4 (trait + both backends, escaping, SCAN/UNLINK) → Task 1. §2's hole (`\u{1}` in `id`) → Task 1 Step 5. §5 (discovery, dedup) → Task 2. §6 (Admin API, 404/502) → Task 4. §7 (MCP, dry_run, not_found) → Task 5. §8 (purge node, error port, validator) → Task 3. §9 (per-instance scope) → docs in Tasks 3 and 6. §10 (purge event counts operations) → Task 3 `record("purge")`, Task 6 observability note. §11 tests: prefix boundary, glob escaping, `Ok(0)`, `\u{1}` rejection → Task 1; validator + dedup → Tasks 2/3; live redis multi-page → Task 1; Admin behaviour → via Tasks 2/5 + `E2E-CACHE-04`; MCP → Task 5; node → Task 3; e2e → Task 6.

**Placeholders.** None. Task 4 has no unit test of its own by stated design (both halves are tested elsewhere and the HTTP surface in e2e). Every helper the tests call either exists today (`cfg`, `test_context`, `lookup_plugin_with_cache`, `BrokenCache`, `response`, `store_url`, `client`, `test_support::{state, obj}`, `call`, `compile_test_policy[_err]`, `PluginResources::empty`) or is defined in the task that uses it (`purge_plugin_with_cache`, `graph`, `local_pair_policy`, `backend`, `seed`, `still_cached`).

**Type consistency.** `pair_prefix`, `escape_glob`, `UNLINK_BATCH` (Task 1); `CacheTarget { id, backend, backend_label, store }`, `PurgeOutcome`, `collect_targets(&[Arc<CompiledGraph>], &str)`, `purge_targets(&[CacheTarget]) -> Result<Vec<PurgeOutcome>, (Vec<PurgeOutcome>, CacheError)>` (Task 2) are used with those exact shapes in Tasks 3–5. `Role::Purge`, `"CACHE_PURGE_FAILED"`, `record("purge")` (Task 3). `PurgeCacheArgs { id, dry_run }` (Task 5). The `cache_events` metric already accepts any `event` string, so `"purge"` needs no metrics change.
