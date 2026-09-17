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
| `phase` (or `role`) | string | — (**required**) | `lookup` (before upstream) or `store` (after upstream). |
| `id` | string | — (**required**) | Shared cache namespace; the lookup and store nodes of one pair must match. |
| `cache_key` | array of string templates (or a single string) | `["$request_method", "$host", "$uri"]` | Components interpolated and joined to form the key. **Both nodes must configure it identically.** |
| `cache_ttl` | integer (seconds) | `300` | Freshness lifetime for stored entries. |
| `cache_http_statuses` | array | `[200, 301, 404]` | Response statuses eligible for caching. (The singular spelling `cache_http_status` is also accepted for config compatibility.) |
| `cache_method` | array | `["GET", "HEAD"]` | Cacheable request methods; other methods bypass the cache. |
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

Requests whose method is not in `cache_method` bypass the cache in both phases
(pass through untouched).

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

## Ports

`proxy-cache` declares three output ports: `success` (a cache miss, or a non-cacheable method — the request continues), `hit` (the response was served from cache; wire straight to `client`), and `error` (never actually used — a backend outage or an oversized response degrades to a miss or a skipped write, not a routed error). `success` and `hit` are mandatory on both the lookup and store nodes — the policy compiler rejects any policy that leaves either unwired, even on the store node where `hit` is never actually emitted. See [Wiring](#wiring) above.

## Errors

This node never fails at execution time: it always returns through `success` or `hit`, so its `error` port is never taken — see the fail-open note above.
