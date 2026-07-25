---
title: basic-auth
description: HTTP Basic authentication against a static user map and/or the consumer store.
---

<span className="plugin-chip" style={{'--chip-color': '#14b8a6'}}>basic-auth</span>

Validates the `Authorization: Basic ...` header against a configured username/password map and/or the gateway's `consumers:` section. Place it early in the request pipeline; downstream nodes can read the authenticated username from `context.message` and, in consumer mode, the attached consumer identity.

## Configuration

| Key | Type | Default | Description |
|---|---|---|---|
| `users` | object or array | — | Username→password credentials. Accepts either a map (`{alice: s3cret}`) or an array of `{username, password}` objects — the shape the web UI's node editor emits. Optional when `use_consumers` is set. |
| `use_consumers` | boolean | `false` | Also resolve credentials against the gateway's `consumers:` section (their `basic-auth: {username, password}` credentials) and attach the matched consumer. At least one of `users` / `use_consumers` must be provided, otherwise policy compilation fails. |
| `anonymous_consumer` | string | unset | Consumer name attached when no credential matches, instead of rejecting. |
| `hide_credentials` | boolean | `false` | Strip the `Authorization` header before proxying upstream. |
| `realm` | string | `gateway` | Realm advertised in the `WWW-Authenticate` challenge on rejection. |

```yaml
- id: auth
  type: basic-auth
  config:
    use_consumers: true
    realm: internal-api
    hide_credentials: true
```

Inline users (no consumer store) still work exactly as before:

```yaml
- id: auth
  type: basic-auth
  config:
    users:
      alice: s3cret
      bob: hunter2
    realm: internal-api
```

Note: the UI node editor pre-fills `realm` with `restricted`; the plugin's own default (when the key is omitted) is `gateway`.

## Behavior

The plugin base64-decodes the `Authorization: Basic` header into a `username:password` pair and resolves it in order:

1. **Inline `users`** — a matching username/password lets the request continue.
2. **Consumer store** (when `use_consumers: true`) — the username is looked up against the `basic-auth` consumer credentials and the presented password is checked against the matched consumer's stored password. On success the consumer identity is attached (`consumer.*` keys in `context.message` plus `X-Consumer-Username`/`X-Consumer-Custom-ID` headers).
3. **Anonymous fallback** (when `anonymous_consumer` is set) — if nothing matched, the named consumer is attached instead of rejecting.

On success the context passes through the **success** port. For back-compat the authenticated username is always written to `context.message["user"]`.

On a missing header, malformed credentials, unknown user, or wrong password (and no anonymous fallback), the plugin sets a rejection on the response and routes through the **error** port:

- `context.response.status_code` = `401`
- Body: `{"error": "unauthorized", "message": "Invalid credentials"}` with `content-type: application/json`
- `WWW-Authenticate: Basic realm="<realm>"` challenge header
- Error code appended to `context.errors`: `UNAUTHORIZED`
