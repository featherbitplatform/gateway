---
title: proxy-cache
description: In-memory response caching, expressed as a lookup/store node pair sharing one cache namespace by id.
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

## Wiring

The lookup node goes **before** `upstream`; its `error` port routes to
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
  - { from: cache-lookup.error,   to: client.in }       # cache HIT → client
  - { from: upstream.success,     to: cache-store.in }
  - { from: cache-store.success,  to: client.in }
```

## Behavior

Requests whose method is not in `cache_method` bypass the cache in both phases
(pass through untouched).

The **lookup** node derives the key and queries the cache. On a **hit** it
replaces `context.response` with the cached status, headers, and body, adds
`featherbit-cache-status: HIT` (and strips `cache-control`/`expires` when
`hide_cache_headers` is set), then fails with error code `PROXY_CACHE_HIT` —
routing the Context through the `error` port to `client.in`. On a **miss** it
passes through to the upstream.

The **store** node caches the response when its status is in
`cache_http_statuses`, using `cache_ttl` as the freshness lifetime, and marks the
outgoing response `featherbit-cache-status: MISS` (it came from the upstream, not the
cache).

The cache is in-memory and per gateway instance; entries expire lazily on read.
