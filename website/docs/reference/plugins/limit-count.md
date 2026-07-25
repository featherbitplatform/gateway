---
title: limit-count
description: Fixed-window request-count limiting per resolved key, using a shared counter backend.
---

<span className="plugin-chip" style={{'--chip-color': '#ef4444'}}>limit-count</span>

Counts requests per resolved key within a fixed time window and rejects those that exceed `count` requests per `time_window` seconds. Unlike the token-bucket [`rate-limit`](./rate-limit.md) plugin (smooth continuous refill), this enforces a hard cap per discrete window. Counting is delegated to a shared counter backend; only the in-memory `local` backend is available today. Place it before `upstream` to shed excess traffic early.

## Configuration

| Key | Type | Default | Description |
|---|---|---|---|
| `count` | integer | — (**required**) | Requests allowed per window; must be greater than 0. |
| `time_window` | integer | — (**required**) | Window length in seconds; must be greater than 0. |
| `key` | string | `"$remote_addr"` | A `$var` template resolved per request (e.g. `$remote_addr`, `$consumer_name`, `$http_x_api_key`). An empty resolved value falls back to the client remote address. |
| `policy` | string | `local` | Counter backend. Only `local` is available; `redis` and others are rejected at config load with the supported list. |
| `group` | string | — | Prefixes the counter key so multiple nodes share one counter. |
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

The counter key is resolved by interpolating the `key` template against the request (supported `$var` names include `$remote_addr`, `$consumer_name`, `$http_<header>`, and `$arg_<query>`). When the template resolves to empty, the key falls back to the client remote address. With `group` set, the resolved key is prefixed with `group:` so several nodes count against one shared counter.

On each request the key is counted against the fixed window:

- **Within the limit** — the request passes through the `success` port. When `show_limit_quota_header` is set, `X-RateLimit-Limit`, `X-RateLimit-Remaining`, and `X-RateLimit-Reset` (whole seconds until the window resets) are written onto `context.response`.
- **Over the limit** — the plugin writes a rejection onto `context.response` (status `rejected_code`, JSON body `{"error_msg": ...}` using `rejected_msg` or a default, `content-type: application/json`, plus the quota headers with `X-RateLimit-Remaining: 0`) and fails with error code `RATE_LIMITED`, routing the Context through the `error` port.

With the `local` policy, counts live in process memory: they are per gateway instance and are lost on restart. If the counter backend errors, the request is rejected with a `500` (code `RATE_LIMIT_UNAVAILABLE`) unless `allow_degradation` is set, in which case it passes through.

The quota headers are set on `context.response`; they are present when the final response is built and sent to the client.
