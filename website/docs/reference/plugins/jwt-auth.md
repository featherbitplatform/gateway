---
title: jwt-auth
description: HMAC JWT validation (HS256/HS384/HS512) with expiry enforcement, inline-secret or per-consumer verification.
---

<span className="plugin-chip" style={{'--chip-color': '#14b8a6'}}>jwt-auth</span>

Verifies an HMAC-signed JWT taken from a request header, enforcing the signature and the `exp` claim, and makes the verified claims available to downstream nodes via `context.message`. Place it early in the request pipeline, before the upstream node.

## Configuration

| Key | Type | Default | Description |
|---|---|---|---|
| `secret` | string | — | Shared HMAC secret used to verify token signatures (inline mode). When set, every token is verified with it. Optional when `use_consumers` is set. |
| `use_consumers` | boolean | `false` | Resolve the token's `key` claim against the gateway's `consumers:` section (`jwt-auth: {key, secret, algorithm}`) and verify with the matched consumer's own secret/algorithm, attaching the consumer on success. At least one of `secret` / `use_consumers` must be provided. |
| `algorithm` | string | `HS256` | One of `HS256`, `HS384`, `HS512`, used for inline verification. Any other value (including asymmetric algorithms like `RS256`) is rejected at config load. |
| `header_name` | string | `authorization` | Header the token is read from (compared case-insensitively). An optional `Bearer ` prefix is stripped. |

```yaml
# inline shared secret
- id: auth
  type: jwt-auth
  config:
    secret: ${JWT_SECRET}
    algorithm: HS256
    header_name: authorization
```

```yaml
# per-consumer verification
- id: auth
  type: jwt-auth
  config:
    use_consumers: true
    header_name: authorization
```

## Behavior

The token is read from `header_name` (stripping a `Bearer ` prefix if present), then verified. Expiry (`exp`) validation is always enabled.

- **Inline secret** (`secret` set): the token is verified with the configured HMAC secret and `algorithm`.
- **Consumer mode** (`use_consumers: true`): the token's payload is base64url-decoded (without trusting it) to read the `key` claim, which selects a consumer; the signature is then verified with *that consumer's* stored `secret` and `algorithm`. On success the consumer identity is attached (`consumer.*` keys in `context.message` plus `X-Consumer-*` headers).

Both may be enabled together: the inline secret is tried first, then consumer resolution.

On success the context passes through the **success** port with the claims exposed to downstream nodes:

- `context.message["jwt_claims"]` = the full decoded claims object
- `context.message["user_id"]` = the `sub` claim, when present (convenience copy)

On a missing token or any verification failure (bad signature, expired, malformed, unknown `key` claim), the plugin sets a rejection on the response and routes through the **error** port:

- `context.response.status_code` = `401`
- Body: `{"error": "unauthorized", "message": "<reason>"}` with `content-type: application/json`
- Error code appended to `context.errors`: `JWT_INVALID`
