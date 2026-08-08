---
title: csrf
description: Double-submit-cookie CSRF protection with HMAC-signed tokens issued as a cookie and verified against a request header.
---

<span className="plugin-chip" style={{'--chip-color': '#6366f1'}}>csrf</span>

Protects state-changing requests with the double-submit-cookie pattern. Safe methods (`GET`, `HEAD`, `OPTIONS`) pass through and receive a signed token cookie; unsafe methods must echo the cookie token back in a request header of the same name, with a valid HMAC signature and unexpired timestamp. Failures return 401 through the node's `denied` port.

## Configuration

| Key | Type | Default | Description |
|---|---|---|---|
| `key` | string | **required** | HMAC secret used to sign tokens. Non-empty. |
| `expires` | integer (seconds) | `7200` | Token lifetime. `0` disables the expiry check and issues a session cookie. |
| `name` | string | `"featherbit-csrf-token"` | Cookie **and** request-header name carrying the token. |
| `phase` | `request` \| `response` | `request` | `request` validates unsafe methods; `response` only issues the token cookie. |

```yaml
type: csrf
config:
  key: edd1c9f034335f136f87ad84b625c8f1
  expires: 3600
  name: featherbit-csrf-token
```

## Behavior

In `phase: request`:

1. **Safe methods** (`GET`/`HEAD`/`OPTIONS`) pass through the `success` port; a fresh token `Set-Cookie` (`path=/;SameSite=Lax;Max-Age=<expires>`) is appended to `context.response`.
2. **Unsafe methods** are validated: the header token must be present (`no csrf token in headers`), the cookie must be present (`no csrf cookie`), both must match (`csrf token mismatch`), and the token's signature and expiry must verify (`Failed to verify the csrf token signature`). Any failure writes a 401 JSON response (`{"error_msg": ...}`) and exits through the `denied` port. On success a fresh cookie is appended and the request continues.

In `phase: response` the node never validates — it only appends the token cookie.

The plugin does not write to `context.message`.

## Pipeline placement

The `upstream` node replaces `context.response.headers` with the upstream's headers, so a cookie set before proxying never reaches the client. Wire **two** nodes sharing the same `key`: a `phase: request` node before the upstream (validation) and a `phase: response` node after it (cookie issuance). A single default node is sufficient only for pipelines that never proxy (e.g. short-circuit responses).

## Token format

`base64(json{random, expires, sign})` where `random` is 16 random bytes hex-encoded, `expires` is the issuance unix timestamp, and `sign = hex(HMAC-SHA256(key, random || expires))`. Validation checks `now - expires <= <configured expires>` (skipped when `expires` is `0`) and verifies the signature in constant time.

:::note Behavior notes
Validation and cookie issuance are split by the `phase` option (see pipeline placement above). The token signature is HMAC-SHA256 over `random || expires`, with `random` a hex string — tokens round-trip against featherbit only. The cookie uses `Max-Age` rather than an `Expires` date.
:::

## Ports

`csrf` declares three output ports: `success`, `denied` (a rejection is prepared), and `error` (never actually used — the plugin never fails). Like `success`, `denied` is a mandatory port: the policy compiler rejects any policy that leaves it unwired. Wire `csrf.denied` straight to `client` so the prepared `401` reaches the caller instead of continuing into `upstream`:

```yaml
edges:
  - from: csrf.success
    to: upstream.in
  - from: csrf.denied
    to: client.in
```
