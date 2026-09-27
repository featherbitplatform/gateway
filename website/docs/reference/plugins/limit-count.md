---
title: limit-count
description: Fixed-window request-count limiting per resolved key, using a shared counter backend.
---

<span className="plugin-chip" style={{'--chip-color': '#ef4444'}}>limit-count</span>

Counts requests per resolved key within a fixed time window and rejects those that exceed `count` requests per `time_window` seconds. Unlike the token-bucket [`rate-limit`](./rate-limit.md) plugin (smooth continuous refill), this enforces a hard cap per discrete window. Counting is delegated to a shared counter backend: the in-memory `local` backend, or `redis` for cluster-shared counters via a named [`stores:`](../../guides/admin-api.md#endpoint-reference) entry. Place it before `upstream` to shed excess traffic early.

## Configuration

| Key | Type | Default | Description |
|---|---|---|---|
| `count` | integer | — (**required**) | Requests allowed per window; must be greater than 0. |
| `time_window` | integer | — (**required**) | Window length in seconds; must be greater than 0. |
| `key` | string | `"$remote_addr"` | A `$var` template resolved per request (e.g. `$remote_addr`, `$consumer_name`, `$http_x_api_key`). An empty resolved value falls back to the client remote address. |
| `policy` | string | `local` | Counter backend. `local` = per-instance in-memory windows; `redis` = cluster-shared windows via a named `stores:` entry. Anything else is rejected at config load with the supported list. |
| `store` | string | — | Required when `policy: redis`: the name of a declared `stores:` entry (redis or valkey). Unknown names fail policy compilation. |
| `group` | string | — | Share one counter between nodes: every `limit-count` node with the same `group` (and the same resolved key) counts against the same window. Without it, each node counts on its own. |
| `rejected_code` | integer | `503` | Status for over-limit requests (200–599). |
| `rejected_msg` | string | — | Message placed in the rejection body (`{"error_msg": ...}`). |
| `show_limit_quota_header` | bool | `true` | Emit `X-RateLimit-Limit`/`-Remaining`/`-Reset` headers onto the response. |
| `allow_degradation` | bool | `false` | On a counter-backend error, allow the request through instead of failing it. |

```yaml
type: limit-count
config:
  count: 100
  time_window: 60
  key: "$remote_addr"
  policy: local
  rejected_code: 429
  show_limit_quota_header: true
```

## Behavior

The counter key is resolved by interpolating the `key` template against the request (supported `$var` names include `$remote_addr`, `$consumer_name`, `$http_<header>`, and `$arg_<query>`). When the template resolves to empty, the key falls back to the client remote address. Each node counts on its own: without `group`, the counter is namespaced by the policy name and node id (`<policy>/<node id>:<key>`), so two `limit-count` nodes — in the same policy or in different ones — never consume each other's quota, even though they share one counter backend. The namespace is the same on every gateway instance and across hot reloads, so `policy: redis` counters stay cluster-wide and a config reload keeps the current windows. A policy attached to several routes is one node, so those routes share its limit. To make separate nodes share one counter on purpose, give them the same `group`: the key is then `group:<key>` instead.

On each request the key is counted against the fixed window:

- **Within the limit** — the request passes through the `success` port. When `show_limit_quota_header` is set, `X-RateLimit-Limit`, `X-RateLimit-Remaining`, and `X-RateLimit-Reset` (whole seconds until the window resets) are written onto `context.response`.
- **Over the limit** — the plugin writes a rejection onto `context.response` (status `rejected_code`, JSON body `{"error_msg": ...}` using `rejected_msg` or a default, `content-type: application/json`, plus the quota headers with `X-RateLimit-Remaining: 0`) and exits through the `limited` port.

With the `local` policy, counts live in process memory: they are per gateway instance and are lost on restart. If the counter backend errors — a genuine infrastructure failure — the request fails with error code `RATE_LIMIT_UNAVAILABLE` through the `error` port (a `500` response is prepared) unless `allow_degradation` is set, in which case it passes through on `success`.

With `policy: redis`, window boundaries are wall-clock aligned (`now / time_window`) and shared by every gateway instance — switching from `local` changes boundaries from first-request-aligned to clock-aligned. Backend errors respect `allow_degradation` (default `false`: reject). Errors are counted in `gateway_counter_store_errors_total{store}`.

The quota headers are set on `context.response`; they are present when the final response is built and sent to the client.

## Ports

`limit-count` declares three output ports: `success`, `limited` (a quota rejection is prepared), and `error` (a genuine counter-backend failure). `success` and `limited` are mandatory — the policy compiler rejects any policy that leaves either unwired. Wire `limit-count.limited` straight to `client` so the prepared rejection reaches the caller instead of continuing into `upstream`:

```yaml
edges:
  - from: limit-count.success
    to: upstream.in
  - from: limit-count.limited
    to: client.in
  - from: limit-count.error
    to: client.in
```

## Errors

The node returns the Context with an error, so the graph engine routes through the `error` port and appends the error to `context.errors`. The status below is the one prepared on `context.response`; wire `error` to `client` (or an [`error-handler`](error-handler.md)) for the caller to see it.

| Code | Status | When |
|---|---|---|
| `RATE_LIMIT_UNAVAILABLE` | 500 | The counter backend failed (`policy: redis`) — the node rejects rather than failing open. |
