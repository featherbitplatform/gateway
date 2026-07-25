---
title: limit-conn
description: Concurrent-request limiting via a shared in-flight counter, expressed as an acquire/release node pair around the upstream call.
---

<span className="plugin-chip" style={{'--chip-color': '#0ea5e9'}}>limit-conn</span>

Limits how many requests may be **in flight at once** for a given key (client
address by default). Concurrency is a property of the span between entering and
leaving the upstream call, which a single graph node cannot observe — so
`limit-conn` is a **pair of nodes** sharing one in-flight counter:

- an **acquire** node placed *before* `upstream`, which increments the counter
  and rejects when the ceiling is reached, and
- a **release** node placed *after* `upstream`, which decrements it.

Both nodes are configured with the same `key` (and identical `conn`/`burst`), so
they resolve to the same process-wide counter. This is the same "two phases, one
shared key" shape [`proxy-rewrite`](./proxy-rewrite.md) uses for
request/response.

## Configuration

| Key | Type | Default | Description |
|---|---|---|---|
| `phase` (or `role`) | string | — (**required**) | `acquire` (before upstream) or `release` (after upstream). |
| `conn` | integer | `20` | Maximum sustained concurrent requests. Must be `> 0`. |
| `burst` | integer | `0` | Extra concurrent requests tolerated above `conn`. The hard ceiling is `conn + burst`. |
| `key` | string template | `$remote_addr` | Concurrency key, interpolated per request. **Both nodes must use the same value.** |
| `key_type` | string | `var` | `constant` uses `key` verbatim; any other value interpolates it. |
| `rejected_code` | integer | `503` | Status returned to over-limit requests. |
| `rejected_msg` | string | — | Returned as a JSON body `{"error_msg": "..."}` on rejection. |
| `default_conn_delay` | number | — | Accepted for config compatibility but ignored — featherbit rejects rather than delays. |

## Wiring

The acquire node goes **before** `upstream`; its `error` port routes to
`client.in`, so an over-limit request short-circuits straight to the client with
the rejection response. The release node goes **after** `upstream` on **both**
the success and error paths, so a failed upstream call still frees the slot and
the counter can never leak.

```yaml
nodes:
  - id: conn-acquire
    type: limit-conn
    config: { phase: acquire, conn: 100, burst: 50, key: $remote_addr, rejected_code: 503 }
  - id: upstream
    type: upstream
    config: { targets: [{ host: backend, port: 8080 }] }
  - id: conn-release
    type: limit-conn
    config: { phase: release, conn: 100, burst: 50, key: $remote_addr }
edges:
  - { from: listener.out,        to: conn-acquire.in }
  - { from: conn-acquire.success, to: upstream.in }
  - { from: conn-acquire.error,   to: client.in }      # over-limit → 503
  - { from: upstream.success,     to: conn-release.in } # free the slot
  - { from: upstream.error,       to: conn-release.in } # …on failures too
  - { from: conn-release.success, to: client.in }
```

## Behavior

The acquire node increments the shared counter and reads the number of requests
already in flight. If that count is at or above `conn + burst`, it undoes its
increment, writes the rejection onto `context.response` (status `rejected_code`,
JSON body, `content-type: application/json`) and fails with error code
`LIMIT_CONN_EXCEEDED`, routing the Context through the `error` port. Otherwise it
passes through and the slot stays held until the paired release node runs.

The release node decrements the counter, floored at zero so a stray release can
never drive it negative.

:::caution Keep the pair in sync
The acquire and release nodes share state **only** when their `key` (and
`conn`/`burst`) match. Use a stable key such as `$remote_addr` that resolves
identically before and after the upstream call. Counters live in process memory
and are per gateway instance.
:::
