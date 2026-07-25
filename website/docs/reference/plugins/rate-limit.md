---
title: rate-limit
description: Token bucket rate limiting per client key — remote address or a request header.
---

<span className="plugin-chip" style={{'--chip-color': '#f97316'}}>rate-limit</span>

Enforces a per-client request rate using the token bucket algorithm. Each client key gets its own in-memory bucket (stored in a concurrent `DashMap`, created lazily on the client's first request); requests that find the bucket empty are rejected with 429 through the node's `error` port. Place it before `upstream` to shed excess traffic early.

## Configuration

All keys are optional; the constructor never fails.

| Key | Type | Default | Description |
|---|---|---|---|
| `requests_per_second` | integer | `100` | Sustained refill rate per client key: tokens added per second. |
| `burst` | integer | = `requests_per_second` | Bucket capacity: maximum requests allowed in a burst. |
| `key_from` | string | remote address | Use `"header:<name>"` to key on a request header instead; any other value keys on the remote address. |

```yaml
type: rate-limit
config:
  requests_per_second: 10
  burst: 20
  key_from: "header:x-api-key"
```

## Behavior

The per-client key is `context.request.remote_addr` by default, or the first value of the configured header; when the header is absent from a request, the key falls back to the remote address.

Each bucket starts full at `burst` tokens and is refilled continuously based on elapsed time at `requests_per_second`, capped at `burst`. Every request consumes one token:

- **Token available** — the request passes through the `success` port with the Context untouched.
- **Bucket empty** — the plugin writes a rejection onto `context.response` (status `429`, JSON body `{"error": "rate_limited", "message": "Too many requests"}`, `content-type: application/json`, and a `retry-after: 1` header) and fails with error code `RATE_LIMITED`, routing the Context through the `error` port.

Buckets live in process memory: counts are per gateway instance and are lost on restart. The plugin does not write to `context.message`.

:::note Legacy configs
Older UI builds saved the keys `limit`, `window_s`, `strategy`, and `key_by`, which the plugin ignores — nodes saved with them run with the defaults above. Re-save the node (the editor now uses the correct keys) or update the YAML to `requests_per_second`/`burst`/`key_from`.
:::
