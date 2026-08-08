---
title: cors
description: Add Access-Control-* response headers for allowed origins and answer OPTIONS preflight requests with 204.
---

<span className="plugin-chip" style={{'--chip-color': '#06b6d4'}}>cors</span>

Applies CORS response headers based on the request's `Origin` header, and answers `OPTIONS` preflight requests with an empty 204 response. Place it early in the request pipeline so preflights are handled before auth or upstream nodes.

## Configuration

All keys are optional; the constructor never fails.

| Key | Type | Default | Description |
|---|---|---|---|
| `allowed_origins` | array of strings | `["*"]` | Origins granted CORS access; `"*"` matches any origin. Matching is exact otherwise. |
| `allowed_methods` | array of strings | `["GET", "POST", "PUT", "DELETE", "OPTIONS"]` | Methods advertised in preflight responses. |
| `allowed_headers` | array of strings | `["*"]` | Request headers advertised in preflight responses; may be `"*"`. |
| `max_age` | integer (seconds) | `3600` | Preflight cache lifetime (`access-control-max-age`). |
| `allow_credentials` | bool | `false` | Whether to emit `access-control-allow-credentials: true`. |

```yaml
type: cors
config:
  allowed_origins: ["https://app.example.com"]
  allowed_methods: ["GET", "POST"]
  max_age: 600
  allow_credentials: true
```

## Behavior

The plugin reads the request's `origin` header and checks it against `allowed_origins`. For an allowed origin it sets on `context.response`:

- `access-control-allow-origin` — `*` when the allowed list contains the wildcard, otherwise the request's origin echoed back.
- `access-control-allow-credentials: true` — only when `allow_credentials` is enabled.

When the request method is `OPTIONS` (preflight) and the origin is allowed, it additionally sets `access-control-allow-methods`, `access-control-allow-headers`, and `access-control-max-age`, writes a complete response onto the context (status `204` with an empty body), and **exits through the `preflight` output port** instead of `success`.

## Ports

`cors` declares three output ports: `success` (non-preflight requests, and preflights for disallowed origins), `preflight` (an answered `OPTIONS` preflight), and `error` (never actually used — the plugin never fails). Like `success`, `preflight` is a mandatory port: the policy compiler rejects any policy that leaves it unwired. Wire `cors.preflight` straight to `client` so the prepared `204` reaches the caller instead of continuing into `upstream`:

```yaml
edges:
  - from: cors.success
    to: upstream.in
  - from: cors.preflight
    to: client.in
```

Disallowed origins simply pass through on `success` with no CORS headers added — the request itself is not rejected.

This plugin never errors: it never exits through the `error` port, emits no error codes, and does not write to `context.message` or `context.errors`.

:::note Legacy configs
Older UI builds saved the keys `allow_origins`, `allow_methods`, and `max_age_s`, which the plugin ignores - nodes saved with them run with the defaults above. Re-save the node (the editor now uses the plugin's keys, including `allowed_headers`) or update the YAML to the keys in the table.
:::
