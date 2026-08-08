---
title: key-auth
description: API-key authentication against a static key list, read from a header or query parameter.
---

<span className="plugin-chip" style={{'--chip-color': '#14b8a6'}}>key-auth</span>

Authenticates requests by matching an API key against a configured list of valid keys. Place it early in the request pipeline, before the upstream node, so unauthenticated requests never reach your backend.

## Configuration

| Key | Type | Default | Description |
|---|---|---|---|
| `keys` | array of strings | — (required) | Exact-match list of accepted API keys. Must contain at least one entry, otherwise policy compilation fails. |
| `header_name` | string | `x-api-key` | Header the key is read from (compared case-insensitively). |
| `query_param` | string | unset | Query parameter checked as a fallback when the header is absent. No fallback if unset. |

```yaml
- id: auth
  type: key-auth
  config:
    keys: ["key-one", "key-two"]
    header_name: x-api-key
    query_param: api_key
```

Note: the UI node editor exposes `keys` and `header_name` only; set `query_param` directly in YAML.

## Behavior

The plugin reads the key from `header_name` first, then falls back to `query_param` if configured. If the key matches an entry in `keys`, the context passes through the **success** port unchanged.

Unlike the other auth plugins, key-auth does **not** write any identity information into `context.message` — a valid key simply lets the request continue.

On a missing or invalid key the plugin sets a rejection on the response and exits through the **`denied`** port:

- `context.response.status_code` = `401`
- Body: `{"error": "unauthorized", "message": "Invalid or missing API key"}` with `content-type: application/json`

## Ports

`key-auth` declares three output ports: `success`, `denied` (a rejection is prepared), and `error` (never actually used — the plugin never fails). Like `success`, `denied` is a mandatory port: the policy compiler rejects any policy that leaves it unwired. Wire `key-auth.denied` straight to `client` so the prepared `401` reaches the caller instead of continuing into `upstream`:

```yaml
edges:
  - from: key-auth.success
    to: upstream.in
  - from: key-auth.denied
    to: client.in
```
