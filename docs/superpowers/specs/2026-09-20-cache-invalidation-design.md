# Explicit invalidation for `proxy-cache`

**Date:** 2026-09-20
**Status:** approved design, not yet implemented
**Target version:** 0.11.0 (new capability)
**Origin:** the "explicit invalidation" follow-up recorded on `website/docs/reference/roadmap.md` and in the 0.10.0 release notes

## 1. Problem

Cached responses only expire. Nothing purges one on demand.

So when an upstream's responses change — a deploy, a content edit, a price update — the
gateway keeps serving the old one until its TTL runs out. Before 0.10.0 that was one
instance's problem; with `policy: redis` the stale entry is now shared by **every**
instance, so the blast radius of "wait for the TTL" grew with the feature that made the
cache useful.

The workarounds are all bad: a TTL short enough to bound staleness defeats the cache, and
restarting instances to clear a `policy: local` cache is an outage to fix a stale page.

Two callers need this and they are different in kind:

- **An operator or agent** who knows the backend changed and wants the cache flushed *now*,
  from outside the request path.
- **A policy** that just forwarded a write — a `POST` or `PUT` to the resource the cache
  serves — and should invalidate on the way through, so the next read is fresh. TTLs cannot
  cover this case at all: the staleness window is exactly the TTL, every time.

## 2. What already exists

| Piece | Where | Note |
|---|---|---|
| `ResponseCache` trait, two backends | `src/traffic/cache.rs`, `src/stores/redis_cache.rs` | `get`/`put`; the local backend is a bounded `DashMap`, the redis one a hash per entry |
| Key derivation | `src/plugins/native/proxy_cache.rs:315` | `{id}\u{1}{component}\u{1}…` — every entry of a pair shares the prefix `{id}\u{1}` |
| Redis key | `src/stores/redis_cache.rs:21` | `{key_prefix}:cache:{key}` |
| Pair validation | `src/graph/engine.rs`, `validate_cache_pairs` | halves grouped by `id`; disagreement is a compile error, a lone half a `cache_pairs` warning |
| Compiled routes | `src/state.rs:36` | `routes: RwLock<Vec<(RouteConfig, Arc<CompiledGraph>)>>` |
| Trait default methods as a discovery channel | `Plugin::reads_response_body` | a node describes itself; the compiler collects the answers |
| Runtime admin actions | `DELETE /api/debug/traces`, `POST /api/stores/{name}/ping` | actions that change no config |
| MCP write tools | `src/mcp/tools/mod.rs`, `ToolDef { scope: Write }` | scope enforced in `server.rs` |
| `SCAN`, `UNLINK` | redis 0.32 | both exposed by the crate |

The `\u{1}` separator is what makes this cheap: the prefix `products\u{1}` matches every
entry of the pair `products` and none of a pair named `products-v2`. Purging a pair is a
prefix match, not a key reconstruction.

**That guarantee currently has a hole, and this design closes it.** `proxy-cache` checks
only that `id` is non-empty (`proxy_cache.rs:174`). Nothing stops a config `id` from
containing `\u{1}` itself — and an `id` of `products\u{1}x` would produce keys that the
prefix `products\u{1}` also matches, so purging `products` would take another pair's entries
with it. `from_config` must reject an `id` containing `\u{1}` (and, for the same reason,
any other control character) at policy-compile time. The prefix boundary is the entire
safety argument of §4, so it has to be enforced, not assumed.

## 3. Decisions

| Question | Decision | Rationale |
|---|---|---|
| Granularity | **By pair `id`** — everything that pair cached | A prefix match; needs no knowledge of how keys are derived. Purging one entry would require the caller to reproduce a template rendering, and a mismatch is a silent no-op |
| Triggers | **Admin API, MCP tool, and a `phase: purge` node** | Operators and agents need an out-of-band flush; a policy needs to invalidate on its own writes, which no TTL can do |
| Finding the backends for an `id` | **Collected at compile time** via a `Plugin` default method, gathered onto `CompiledGraph` | Mirrors `reads_response_body`; no process-wide mutable registry to leak or go stale across hot-reloads |
| Unknown `id` on the Admin API | **`404`** | A typo must not read as a successful flush of nothing |
| `policy: local` scope | **The receiving instance only**, stated in docs and in the response | No cross-instance signalling exists; adding pub/sub is its own design (§9) |
| Purge failure | **Surfaces**: `error` port on the node, `502` on the API | The caller explicitly asked for state to change. Deliberately opposite to a failed read, which degrades to a miss |
| Redis deletion | **`SCAN MATCH` + `UNLINK` in batches** | `SCAN` is incremental and does not block the server; `UNLINK` frees memory off-thread where `DEL` blocks |
| Concurrent writes during a purge | **Best-effort, documented** | An entry written after the cursor passed survives until its TTL; the alternative is a lock across the whole keyspace |
| `\u{1}` in a config `id` | **Rejected at policy-compile time** | The separator is the prefix boundary; an `id` containing it lets a purge of one pair take another's entries |
| Purging by exact key | **Out of scope** | See granularity; revisit if a concrete case appears |

## 4. The trait

```rust
#[async_trait]
pub trait ResponseCache: Send + Sync {
    async fn get(&self, key: &str) -> Result<Option<CachedResponse>, CacheError>;
    async fn put(&self, key: &str, entry: &CachedResponse, ttl: Duration) -> Result<(), CacheError>;

    /// Removes every entry belonging to the pair `id` and returns how many.
    ///
    /// "Belonging to" means the key begins with `{id}\u{1}`, which is how
    /// `proxy-cache` derives every key it writes. `Ok(0)` is a normal answer:
    /// the pair had nothing cached.
    async fn purge(&self, id: &str) -> Result<u64, CacheError>;
}
```

**Local:** `entries.retain(|key, _| !key.starts_with(&prefix))`, counting removals. One pass
over the map, under the same `DashMap` sharding as every other operation.

**Redis:** the pattern `{key_prefix}:cache:{escaped_id}\x01*`, where `escaped_id` has every
glob metacharacter (`*`, `?`, `[`, `]`, `\`) backslash-escaped — an `id` is free-form config
text and must not be able to widen the match. Keys are collected through `SCAN MATCH` with a
`COUNT` hint and removed with `UNLINK` in batches of a few hundred, so neither the scan nor
the delete blocks the server for the duration of a large purge.

## 5. Discovery: which backends hold an `id`

An `id` alone does not say where a pair caches. Two pairs named `products` in two policies
may use different backends; every `policy: local` pair in the process shares one
`LocalResponseCache`.

A new `Plugin` default method, overridden only by `proxy-cache`:

```rust
/// A cache this node writes to, if it is a `proxy-cache` half.
///
/// Consulted at policy-compile time so an invalidation request can find every
/// backend that holds entries for a given pair `id`, without a process-wide
/// registry that would have to survive hot-reloads.
fn cache_target(&self) -> Option<CacheTarget> {
    None
}

pub struct CacheTarget {
    pub id: String,
    pub backend: Arc<dyn ResponseCache>,
    /// `"local"` or `"redis"`, for the response and the metric.
    pub backend_label: &'static str,
    /// The declared store's name for `redis`; empty for `local`.
    pub store: String,
}
```

`compile_policy` collects them onto `CompiledGraph.cache_targets`. A purge for `id` walks
`state.routes`, gathers every target with that `id`, **deduplicates by
`(backend_label, store)`** — otherwise the shared local cache would be purged once per pair
that uses it, and the counts would be nonsense — and purges each.

This is exactly how `reads_response_body` already lets a node describe itself for a
compile-time decision, so the codebase has one pattern for "the compiler asks nodes about
themselves", not two.

## 6. The Admin API

```
DELETE /api/cache/{id}
```

```json
{
  "id": "products",
  "purged": [
    { "backend": "local", "removed": 12 },
    { "backend": "redis", "store": "sessions", "removed": 40 }
  ]
}
```

| Status | When |
|---|---|
| `200` | at least one pair with this `id` exists; every backend was purged |
| `404` | no compiled policy has a `proxy-cache` pair with this `id` |
| `502` | a backend could not be reached; the body names which, and any that did succeed |

The `404` matters. Without it, `DELETE /api/cache/prodcuts` returns `{"purged": []}` and the
operator walks away believing the cache is flushed.

`backend: "local"` in the response is the documented reminder that this instance's local
cache was purged, not every instance's.

## 7. The MCP tool

```
purge_cache { id: string, dry_run?: bool }     scope: write
```

Same discovery and response as the Admin API. `dry_run: true` returns the `purged` array
with `removed` omitted — the targets a purge *would* hit — and deletes nothing. An agent
should be able to see what it is about to flush, and a `dry_run` is how every other write
tool in `src/mcp/tools/writes.rs` already offers that.

Unknown `id` is a `not_found` tool error, the same shape `delete_route` uses.

## 8. The policy node

`proxy-cache` gains a third phase:

```yaml
- id: forward-write
  type: upstream
  config: { targets: [{ host: products, port: 8080 }] }

- id: drop-cache
  type: proxy-cache
  config:
    phase: purge
    id: products          # the pair to invalidate
    policy: redis
    store: sessions
```

wired `forward-write.success → drop-cache.in`, so a write that reached the upstream
invalidates what the lookup half serves. Gating on the upstream's status is the author's
choice — a `condition` on `status` before it, if only a `2xx` should purge.

| Port | Meaning |
|---|---|
| `success` | the purge completed (possibly removing nothing) |
| `error` | the backend could not be reached; the request continues along the error edge |

**A failed purge takes the `error` port, unlike a failed lookup.** A lookup that cannot
reach its backend degrades to a miss, because a cache exists to save latency and losing it
should cost latency alone. A purge is different: the caller asked for state to change, and
if it did not, silently continuing leaves the cache stale in precisely the situation
invalidation exists to fix. Reads degrade; purges report.

**Pair validation extends to the new half.** `validate_cache_pairs` already groups halves by
`id` and rejects disagreement on `policy`/`store`; a purge half joins that group and is held
to the same rule — a purge pointed at a different backend than its pair would silently clear
nothing. A purge with no lookup/store half to purge *for* is reported in `cache_pairs` like
any other lone half.

`reads_response_body()` is `false`: the node reads nothing from the response.

**Ports and the per-type spec.** `PortSpec` is declared per node *type*, so a `phase: purge`
node — like a `phase: store` node today — must wire a `hit` port that can never fire. That
is a pre-existing awkwardness of the shared spec, not something this design introduces, but
a third phase makes it more visible and the plugin page should say so plainly.

## 9. Scope: `policy: local` is per instance

There is no cross-instance signalling in the gateway. The `publish()` calls in the ACME
manager are an in-process `ArcSwap` swap, not a message to other instances.

So a purge of a `policy: local` pair clears the local cache of the instance that received
the request — through whichever trigger — and no other. The plugin page and the Admin API
docs state this in those words, and the response's `backend: "local"` line is a per-call
reminder.

`policy: redis` purges are cluster-wide for free, because the store is shared. That is one
more reason to prefer it in a multi-instance deployment, and the docs say so where the
choice is made.

Fanning a local purge out to every instance would need a redis pub/sub channel, a
subscriber per instance with reconnection, and a story for the message that gets lost while
an instance is restarting. That is a design of its own and is out of scope here.

## 10. Observability

`gateway_cache_events_total{backend, store, event="purge"}` counts purge **operations**, not
entries removed — a purge that finds nothing is still a purge, and per-entry counts would
swamp the other events. The removed count goes in the log line at `info` and in the API
response.

## 11. Testing

| Level | Test |
|---|---|
| Unit | Local `purge("products")` removes every `products\u{1}…` entry and **none** of `products-v2\u{1}…` — the prefix boundary is the whole guarantee |
| Unit | Glob metacharacters in an `id` are escaped: an `id` of `a*` purges the pair `a*`, not every pair beginning with `a` |
| Unit | `purge` on an `id` with no entries returns `Ok(0)`, not an error |
| Unit | `from_config` rejects an `id` containing `\u{1}` — the test that pins the prefix boundary from the config side |
| Unit | `validate_cache_pairs` rejects a purge half whose `policy`/`store` disagree with its pair; reports a purge with no counterpart |
| Unit | `cache_targets` deduplicates two local pairs onto one target |
| Integration (gated) | Redis purge removes the pair's entries and leaves a sibling pair's untouched; more entries than one `SCAN` page, so batching is exercised |
| Admin | `DELETE /api/cache/{unknown}` is `404`; a known `id` returns per-backend counts |
| MCP | `purge_cache` with `dry_run` returns targets and removes nothing; without it, removes and counts; unknown `id` is `not_found` |
| Node | A `phase: purge` execution clears its pair; a failing backend exits `error`, not `success` |
| e2e | `E2E-CACHE-*`: populate through a real route, purge via the API, next request is a `MISS`; and the same via a `phase: purge` node on a write route |

The prefix-boundary test and the glob-escaping test are the two that pin the safety of the
feature: both describe a wrong implementation that purges *more* than asked.

## 12. Out of scope

- **Purging by exact key.** The caller would have to reproduce a pair's template rendering,
  and a mismatch is a silent no-op.
- **Cross-instance fan-out for `policy: local`** (§9).
- **Tags / surrogate keys** — purging every entry that mentions product 42 across several
  pairs. Useful, but it needs an index maintained on every write, which is a different
  cost model from a prefix match.
- **Automatic invalidation on write methods** (purge on every `POST`/`PUT`/`DELETE` through
  the store half). Tempting, but it decides on the author's behalf which writes invalidate
  what; the `phase: purge` node lets them say it.
