# Distributed response cache for `proxy-cache`

**Date:** 2026-09-17
**Status:** approved design, not yet implemented
**Target version:** 0.11.0 (new capability)
**Origin:** the `proxy-cache` follow-up recorded on `website/docs/reference/roadmap.md` — *"a shared/distributed cache backend (Redis) for multi-instance deployments"*

## 1. Problem

`proxy-cache` stores responses in a process-local `DashMap`
(`crate::traffic::CacheRegistry`). Every gateway instance therefore keeps its own
independent cache.

On a three-instance deployment that means three cold caches, three times the upstream
load for the same working set, and a cache hit rate that falls as instances are added —
the opposite of what scaling out should do. A restart or a rolling deploy empties a
cache entirely.

There is a second, quieter problem in the same code. The registry's own doc says entries
*"expire lazily on read"*: an entry that is written and never read again is never
removed. A key space that churns — anything keyed on a user, a tenant, or a path segment
— grows the map for the life of the process. Nothing bounds it.

## 2. What already exists

This design adds a backend and a bound. It does not add a store subsystem, a connection
pool, or a config surface.

| Piece | Where | Note |
|---|---|---|
| `CacheRegistry` | `src/traffic/mod.rs:135` | `DashMap<String, CacheEntry>`, sync `get`/`put`, `Instant` expiry |
| `CacheEntry` | `src/traffic/mod.rs:124` | `status: u16`, `headers: HashMap<String, Vec<String>>`, `body: Bytes`, `expires_at` |
| `proxy-cache` node | `src/plugins/native/proxy_cache.rs` | lookup/store pair; reaches the registry through `resources.traffic.cache` |
| `stores:` + `StoreRegistry` | `src/stores/` | named redis/valkey connections, already backing sessions, `limit-count` and ACME |
| Namespace registry | `src/stores/namespaces.rs` | `cnt`, `acme`, `sess`, `lock`, `subj`, `kv`, with a drift test over the real key builders |
| `policy: local \| redis` + `store:` | `src/plugins/native/limit_count.rs:131` | the established config shape for exactly this choice |

## 3. Decisions

| Question | Decision | Rationale |
|---|---|---|
| Config shape | **`policy: local \| redis` + `store: <name>`** | `limit-count` already settled this for the same local-or-shared choice; a second spelling would be gratuitous |
| Abstraction | **A `ResponseCache` trait, two implementations** | Mirrors `CounterStore`; keeps `proxy-cache` unaware of which backend it has |
| Trait returns | **`Result`, not a bare `Option`** | The degrade-to-miss decision belongs in the plugin, visible and testable, not buried in a backend that swallows its own errors |
| Backend unreachable | **Treat as a miss; log and meter** | A cache protects latency, not correctness — deliberately opposite to sessions and `store-*` |
| Redis encoding | **A hash: `status`, `headers` (JSON), `body` (raw bytes)** | Redis is binary-safe, so the body avoids base64's 33% inflation |
| Redis expiry | **Redis's own TTL on the key** | No `expires_at` to check, and no clock comparison across instances |
| Key namespace | **`{key_prefix}:cache:{key}`**, declared in the registry | Policy and subsystem keys must stay disjoint; the registry exists to enforce it |
| Oversized responses | **`max_object_bytes`, default 1 MiB, both backends** | One large body must not be able to fill a store other subsystems share |
| Local growth | **`max_entries`, default 10,000, with a freshness-ordered bound** | The existing lazy-expiry-on-read leaks; see §7 |
| Two-tier (local in front of redis) | **Out of scope** | A local copy stays fresh after another instance's entry changes; coherence is a separate design |

## 4. The abstraction

```rust
/// A backend for `proxy-cache`'s stored responses.
#[async_trait]
pub trait ResponseCache: Send + Sync {
    /// A fresh entry for `key`, or `None` on a miss.
    ///
    /// `Err` means the backend could not answer — distinct from `Ok(None)`,
    /// which means it answered and had nothing. The caller decides what a
    /// failure means; see §6.
    async fn get(&self, key: &str) -> Result<Option<CacheEntry>, CacheError>;

    /// Stores `entry` under `key` for `ttl`.
    async fn put(&self, key: &str, entry: &CacheEntry, ttl: Duration) -> Result<(), CacheError>;
}
```

Two implementations:

- **`LocalResponseCache`** — today's `DashMap`, plus the bound from §7. Its methods are
  async to satisfy the trait but never await.
- **`RedisResponseCache`** — a `StoreHandle` over a declared `stores:` entry.

`proxy-cache` resolves one at construction from `policy`/`store` and holds the
`Arc<dyn ResponseCache>`, exactly as `limit-count` resolves its `CounterStore`. Nothing
reads the store registry on the request path.

**Why `Result` rather than an `Option` that folds errors into misses.** Folding them
inside the backend would make a fully-degraded cache indistinguishable from a working
one at every call site, and would put a policy decision ("an outage is a miss") somewhere
no test can observe it. The plugin maps `Err` to a miss in one place, and that mapping is
a line of code a reviewer can find.

## 5. Redis encoding

One hash per entry:

| Field | Contents |
|---|---|
| `status` | the status code, as an integer |
| `headers` | the header map, JSON-encoded |
| `body` | the raw response bytes |

Written with `HSET` plus `EXPIRE` in one pipeline; read with `HGETALL`. Redis values are
binary-safe, so the body is stored as-is — base64 would inflate every cached response by
a third for no benefit, and a JSON wrapper would additionally escape it.

Expiry is the key's own TTL. The local backend compares `Instant`s because it must; the
redis backend must not, because two instances do not share a monotonic clock and an entry
written by one would be judged against the other's.

A hash whose fields do not parse (a truncated write, a key colliding with something
else's data) is treated as a **miss**, not an error: the gateway cannot use it either
way, and a miss keeps the request moving. It is metered separately from a backend error
so the two are distinguishable in operation.

## 6. A backend failure is a miss

When `get` returns `Err`, `proxy-cache` logs at `warn`, increments an error counter, and
proceeds as though the key were absent. When `put` returns `Err`, it logs and meters;
there is nothing to degrade.

**This is deliberately opposite to every other store consumer.** Sessions and the
`store-*` nodes fail closed: losing the store there means losing correctness, and
continuing would mean serving a request as though an absent session or counter said yes.
A cache is different in kind. Its only job is to save a trip to the upstream, so losing
it should cost latency and nothing else. Turning a redis blip into a `503` on a route
that was merely going faster would be a worse outage than the one it reports.

The docs must state this contrast explicitly, because "stores fail closed" is otherwise a
reasonable thing to assume from the rest of the system.

**It must not be silent.** A cache that has quietly degraded to always-miss looks exactly
like a cache that is working, from every angle except the upstream's load graph. §8 is
what makes it visible.

## 7. Bounds

### 7.1 `max_object_bytes` (new, default 1 MiB, both backends)

A response whose body exceeds this is served normally and **not** cached. Without it a
single large response can consume a shared store that sessions, counters and ACME also
live in. The skip is metered so a route that never caches is explicable rather than
mysterious.

### 7.2 `max_entries` (new, default 10,000, local only)

The local registry currently removes an expired entry only when something reads it, so
anything written and never read again is retained for the life of the process.

On insert at capacity: sweep expired entries first; if still at capacity, evict the entry
with the **earliest** `expires_at`.

This is not an LRU, and the spec says so rather than implying otherwise. No LRU crate is in the dependency tree -- `dashmap` is
the only concurrent-map dependency -- and adding one for this is not worth the
supply-chain surface it would bring through `cargo-deny`. For a cache whose entries all carry TTLs, evicting
the soonest-to-expire is a defensible proxy: it discards what was about to become
worthless anyway. Where it differs from an LRU is a hot entry with a short TTL, which is
evicted ahead of a cold entry with a long one — acceptable, and documented.

Redis needs no equivalent: it expires keys itself and has its own `maxmemory` policy,
which is the operator's to set.

## 8. Observability

Per-backend counters, labelled by store name where applicable:

| Metric | Answers |
|---|---|
| hits / misses | is the cache doing anything |
| errors | has it degraded to always-miss |
| evictions | is `max_entries` too small for the working set |
| skipped-too-large | is `max_object_bytes` silently excluding a route |

The error counter is the one that matters. A degraded cache is invisible in every other
signal — the gateway keeps serving correct responses, just slower and with more upstream
load. This is the same shape of problem the trace-retention work fixed: an absence that
reads as a normal result.

## 9. Registration and config

`proxy-cache` gains three keys, documented on its plugin page:

| Key | Type | Default | Meaning |
|---|---|---|---|
| `policy` | string | `local` | `local` (per instance) or `redis` (shared) |
| `store` | string | — | Required when `policy: redis`; names a declared `stores:` entry |
| `max_object_bytes` | integer | `1048576` | Responses larger than this are not cached |

`max_entries` is a property of the local registry rather than of a node, since every
`local` node shares one registry; it belongs in `system.yaml` beside the other
process-wide limits.

Unknown `policy` values are rejected at policy-compile time with the supported list, as
`limit-count` already does.

## 10. Testing

| Level | Test |
|---|---|
| Unit | The local bound evicts at capacity, expired-first, then soonest-to-expire |
| Unit | A response over `max_object_bytes` is not stored, by either backend |
| Unit | `policy: redis` without `store` is a compile-time error naming the supported policies |
| Unit | A `CacheEntry` round-trips through the redis encoding unchanged, including a binary body and a multi-value header |
| Integration (gated) | **Two `RedisResponseCache` handles over the same store: one `put`s, the other `get`s it.** Cross-instance sharing is the entire point of this work, and nothing else demonstrates it |
| Integration (gated) | An entry disappears after its TTL, without the gateway comparing clocks |
| Integration (gated) | A backend outage produces a miss and increments the error counter — not a failed request |
| Integration (gated) | A malformed hash is a miss, metered separately from an error |
| e2e | `E2E-CACHE-*`: a route caches through a real request, a second request is served from the cache, and a `policy: redis` route shares with a second handle |

Gated tests use `FEATHERBIT_TEST_REDIS_URL`, which CI already provides for both `redis:7`
and `valkey:8`.

## 11. Out of scope

- **Two-tier caching** (local in front of redis). Coherence between the tiers is its own
  design: a local copy remains fresh after another instance's entry is replaced.
- **Explicit invalidation** (purge by key or tag). Entries expire; nothing evicts on
  demand. Worth doing, but it is a separate feature with its own API surface.
- **Caching streamed responses.** `proxy-cache` reads the response body and therefore
  already forces buffering; that limitation is unchanged here.
- **Redis `maxmemory` policy.** The operator's to configure; the gateway bounds what it
  writes, not what the store keeps.
