---
title: proxy-cache
description: Response caching, in-memory or shared over a redis-backed store, expressed as a lookup/store node pair sharing one cache namespace by id.
---

<span className="plugin-chip" style={{'--chip-color': '#8b5cf6'}}>proxy-cache</span>

Serving a cached response means *looking up* the cache before the upstream call
and *storing* the fresh response after it — two moments a single graph node
cannot span. So `proxy-cache` is a **pair of nodes** sharing one cache, linked by
a required `id`:

- a **lookup** node placed *before* `upstream`, which serves a cache hit
  straight to the client (short-circuiting the upstream call), and
- a **store** node placed *after* `upstream`, which caches a fresh response for
  later hits.

Both nodes derive the cache key identically from the same `cache_key` template
and the request, and share one namespace via `id`, so they always agree.

## Configuration

| Key | Type | Default | Description |
|---|---|---|---|
| `phase` (or `role`) | string | — (**required**) | `lookup` (before upstream), `store` (after upstream), or `purge` (on a write route; see [Invalidating on a write](#invalidating-on-a-write)). |
| `id` | string | — (**required**) | Shared cache namespace; the lookup and store nodes of one pair must match. |
| `cache_key` | array of string templates (or a single string) | `["$request_method", "$host", "$uri"]` | Components interpolated and joined to form the key. **Both nodes must configure it identically.** |
| `cache_ttl` | integer (seconds) | `300` | Freshness lifetime for stored entries. |
| `cache_http_statuses` | array | `[200, 301, 404]` | Response statuses eligible for caching. (The singular spelling `cache_http_status` is also accepted for config compatibility.) |
| `cache_method` | array | `["GET", "HEAD"]` | Cacheable request methods; other methods bypass the cache in the lookup and store phases. |
| `hide_cache_headers` | bool | `false` | Strip `cache-control` / `expires` from served cache hits. |
| `policy` | string | `local` | `local` (an in-memory, per-instance cache) or `redis` (a shared cache over a declared [store](../../concepts/stores.md)). Both nodes of a pair must use the same policy and, for `redis`, the same `store`. |
| `store` | string | — (required when `policy: redis`) | Name of a declared `stores:` entry. |
| `max_object_bytes` | integer | `1048576` | Responses larger than this are served normally but never cached, in either backend. |

## Wiring

The lookup node goes **before** `upstream`; its `hit` port routes to
`client.in`, so a hit delivers the cached response without ever calling the
upstream. On a miss it passes through. The store node goes **after** `upstream`
and caches the fresh response.

```yaml
nodes:
  - id: cache-lookup
    type: proxy-cache
    config: { phase: lookup, id: catalog, cache_key: ["$request_method", "$host", "$uri"], cache_ttl: 300 }
  - id: upstream
    type: upstream
    config: { targets: [{ host: catalog, port: 8080 }] }
  - id: cache-store
    type: proxy-cache
    config: { phase: store, id: catalog, cache_key: ["$request_method", "$host", "$uri"],
              cache_ttl: 300, cache_http_statuses: [200, 301, 404] }
edges:
  - { from: listener.out,         to: cache-lookup.in }
  - { from: cache-lookup.success, to: upstream.in }
  - { from: cache-lookup.hit,     to: client.in }       # cache HIT → client
  - { from: upstream.success,     to: cache-store.in }
  - { from: cache-store.success,  to: client.in }
  - { from: cache-store.hit,      to: client.in }       # store never hits, but the port is still mandatory wiring
```

## Behavior

Requests whose method is not in `cache_method` bypass the cache in the lookup
and store phases (pass through untouched).

The **lookup** node derives the key and queries the cache. On a **hit** it
replaces `context.response` with the cached status, headers, and body, adds
`featherbit-cache-status: HIT` (and strips `cache-control`/`expires` when
`hide_cache_headers` is set), then exits through the `hit` port. On a **miss**
it passes through to the upstream on `success`.

The **store** node caches the response when its status is in
`cache_http_statuses`, using `cache_ttl` as the freshness lifetime, and marks the
outgoing response `featherbit-cache-status: MISS` (it came from the upstream, not the
cache).

With `policy: local` (the default) the cache is in-memory and per gateway
instance; entries expire lazily on read, and the process-wide `cache.max_entries`
setting in [`system.yaml`](../../guides/configuration.md) (default `10000`, shared
by every `policy: local` node) bounds how many it holds — once full, the
entries expiring soonest are evicted
(`gateway_cache_events_total{backend="local",event="eviction"}`). With
`policy: redis`, the pair shares one namespace in the named store, so multiple
gateway instances — or multiple policies configured with the same `store` and
`cache_key` — see each other's entries; `max_entries` does not apply, since the
store bounds itself via its own `maxmemory` policy.

:::note[This cache fails open, unlike the rest of the system]
Sessions and the `store-*` nodes fail **closed**: losing their store means
losing correctness, so the request fails rather than proceeding as though an
absent session said yes.

A cache is different in kind. Its only job is to save a trip to the upstream,
so a backend it cannot reach is treated as a **miss** and the request is served
normally. Turning a redis blip into a `503` on a route that was merely going
faster would be a worse outage than the one it reports.

It is not silent: `gateway_cache_events_total{backend="...",store="...",event="error"}`
counts every failed lookup or write, labelled by which store degraded (empty
for `policy: local`), and a cache degraded to always-miss is otherwise
invisible in every signal except the upstream's load. See
[Observability](../../guides/observability.md#prometheus-metrics) for the full
label reference.
:::

:::note[Both halves of a pair must agree on where they cache]
The lookup and store nodes are linked only by their shared `id`. If they
disagree about `policy` or `store`, the store half writes somewhere the lookup
half never reads — the policy compiles, serves traffic, and returns a permanent
100% miss with no error anywhere. It simply looks like a cache that is never
warm.

There is no configuration for which that is correct, so the compiler now
rejects it and names both nodes.

A half with **no counterpart** is reported rather than rejected: a lookup with
no store caches nothing, a store with no lookup is never read, and a purge
with nothing to purge for is a no-op every time it fires — but all three are
also what a policy looks like halfway through being built. They appear in the
`cache_pairs` array of `POST /api/policies/validate` and the MCP
`validate_policy` tool, alongside `buffering`.
:::

## Invalidating on a write

A third phase, `purge`, clears everything its pair has cached. Put it on the
route that changes the resource, after the upstream:

```yaml
- id: drop-cache
  type: proxy-cache
  config: { phase: purge, id: products, policy: redis, store: cache-store }
```

wired `forward-write.success → drop-cache.in`. Gating on the upstream's status
is yours to decide — a `condition` on `status` before it, if only a `2xx` should
purge.

**`cache_method` does not gate `phase: purge`** — a purge acts on the pair's
namespace, not on one request's cached representation, so it fires on
`POST`/`PUT`/`DELETE` too (the default `cache_method` for lookup/store is only
`GET`/`HEAD`).

**A failed purge takes `error`, unlike a failed lookup.** A lookup that cannot
reach its backend becomes a miss, because a cache only saves latency. A purge is
different: you asked for state to change, and continuing silently would leave the
cache stale in exactly the case invalidation exists to fix.

A `phase: purge` node placed on an upstream's success path buffers that
upstream's response (the same as `phase: store`) — because a failed purge can
exit `error`, and an error response cannot be produced mid-stream once bytes
have already gone out. Write responses are usually small, so this rarely
matters in practice; `POST /api/policies/validate` reports it as a buffering
reason if it does.

**A `policy: redis` purge scans the whole store, not just the pair.** It
`SCAN`s the store's entire keyspace incrementally, so its cost grows with the
store's total key count, not with the number of entries the pair actually
cached; `UNLINK` frees the matched keys' memory off-thread rather than
blocking on it. Do not wire a purge to a high-rate write path on a large
shared store. Entries written concurrently during a purge may survive it —
invalidation here is best-effort under concurrent writes, not a snapshot.

**A `policy: local` purge clears this instance only.** No message reaches other
instances. `policy: redis` purges are cluster-wide because the store is shared.

The purge half is held to the same agreement rule as the other two: it must use
the same `policy` and `store` as its pair, or the compiler rejects the policy.

`PortSpec` is per node *type*, so a `phase: purge` node — like `phase: store` —
must wire a `hit` port that never fires.

## Ports

`proxy-cache` declares three output ports: `success` (a cache miss, a non-cacheable method, or a completed purge — the request continues), `hit` (the response was served from cache; wire straight to `client`), and `error` (taken only by a failed **purge** — a lookup or store backend outage degrades to a miss or a skipped write, not a routed error). `success` and `hit` are mandatory on all three phases — the policy compiler rejects any policy that leaves either unwired, even on the store and purge nodes where `hit` is never actually emitted. See [Wiring](#wiring) above.

## Errors

The lookup and store phases never fail at execution time: they always return through `success` or `hit`, so `error` is never taken for them — see the fail-open note above. The **purge** phase is the exception: a purge that cannot reach its backend exits `error` with `CACHE_PURGE_FAILED`, because silently continuing would leave the cache stale in exactly the situation invalidation exists to fix. See [Invalidating on a write](#invalidating-on-a-write).
