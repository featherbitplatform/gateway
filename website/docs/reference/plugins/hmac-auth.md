---
title: hmac-auth
description: HMAC request-signature authentication (hmac-sha1/256/512) with clock-skew and per-consumer secrets.
---

<span className="plugin-chip" style={{'--chip-color': '#14b8a6'}}>hmac-auth</span>

Authenticates requests by verifying an HMAC signature over a canonical *signing string* built from the request. A client proves possession of a shared `secret_key` by signing and sending the base64 signature alongside the `access_key` that names the credential. Place it early in the request pipeline, before the upstream node.

## Configuration

| Key | Type | Default | Description |
|---|---|---|---|
| `use_consumers` | boolean | `false` | Resolve the presented `access_key` against the gateway's `consumers:` section (their `hmac-auth: {access_key, secret_key}` credentials) and attach the matched consumer. |
| `access_key` | string | unset | Inline single-credential key id. |
| `secret_key` | string | required with `access_key` | The secret paired with the inline `access_key`. |
| `algorithm` | string | `hmac-sha256` | One of `hmac-sha1`, `hmac-sha256`, `hmac-sha512`. |
| `clock_skew` | integer (s) | `300` | Maximum allowed drift between the `Date` header and now; `0` disables the check. |
| `signed_headers` | array of strings | unset | Headers the client must have included in its signature; a request omitting any is rejected. |
| `keep_headers` | boolean | `false` | Keep the `X-HMAC-*` proof headers when proxying (they are stripped by default). |
| `hide_credentials` | boolean | `false` | Strip the `Authorization` header before proxying upstream. |
| `anonymous_consumer` | string | unset | Consumer name attached when no credential matches, instead of rejecting. |
| `realm` | string | `hmac` | Realm advertised in the `WWW-Authenticate` challenge on rejection. |

At least one of `access_key` (+`secret_key`) / `use_consumers` must be provided, otherwise policy compilation fails.

```yaml
- id: auth
  type: hmac-auth
  config:
    use_consumers: true
    algorithm: hmac-sha256
    clock_skew: 300
    signed_headers: [date]
```

Consumers declare their credential under `hmac-auth`:

```yaml
consumers:
  - name: alice
    credentials:
      hmac-auth: { access_key: alice-ak, secret_key: alice-sk }
```

## Wire format

Signature parameters are read from **either** an `Authorization` header:

```text
Authorization: Signature keyId="alice-ak",algorithm="hmac-sha256",headers="date @request-target",signature="<base64>"
```

(`keyId` is the featherbit `access_key`), **or** the discrete headers `X-HMAC-ACCESS-KEY`, `X-HMAC-ALGORITHM`, `X-HMAC-SIGNED-HEADERS` (space-separated), and `X-HMAC-SIGNATURE`. The `Date` header (RFC 1123 / GMT) carries the timestamp checked against `clock_skew`.

## Signing string

The access key on the first line, then one line per signed header, with a trailing newline:

```text
<access_key>\n
<h1>: <value1>\n
<h2>: <value2>\n
```

The pseudo-header `@request-target` is rendered as `<METHOD> <request-uri>` instead of a header lookup. `signature = base64(HMAC(secret_key, signing_string))`.

## Behavior

The plugin extracts the signature parameters, enforces the algorithm/`clock_skew`/required `signed_headers`, then resolves the credential in order:

1. **Inline** — if the presented `access_key` matches the configured one, the signature is verified with the inline `secret_key`.
2. **Consumer store** (when `use_consumers: true`) — the `access_key` is looked up against the `hmac-auth` consumer credentials and the signature verified with that consumer's `secret_key`. On success the consumer identity is attached (`consumer.*` keys in `context.message` plus `X-Consumer-*` headers).
3. **Anonymous fallback** (when `anonymous_consumer` is set) — if no credential is presented or matched, the named consumer is attached instead of rejecting.

On success the context passes through the **success** port; the `X-HMAC-*` proof headers are stripped unless `keep_headers` is set, and the `Authorization` header is stripped when `hide_credentials` is set.

On a missing/invalid signature, unknown access key, stale `Date`, or a missing required signed header, the plugin routes through the **error** port:

- `context.response.status_code` = `401`
- Body: `{"error": "unauthorized", "message": "<reason>"}` with `content-type: application/json`
- `WWW-Authenticate: hmac realm="<realm>"` challenge header
- Error code appended to `context.errors`: `HMAC_INVALID`

## Behavior notes

- Consumer credentials use the field names `access_key` / `secret_key`; featherbit's `hmac-auth` consumer index is keyed on `access_key`.
- A single `algorithm` is accepted per node, not a list of allowed algorithms; the client's declared algorithm must match it.
- `@request-target`'s request URI is reconstructed from the parsed path plus a **sorted** `key=value` query string (original query byte order is not retained), so a client signing `@request-target` must canonicalise its query the same way.
- Only the RFC 1123 (`Sun, 06 Nov 1994 08:49:37 GMT`) `Date` format is parsed for clock-skew checks.
- Request-body digest validation (`validate_request_body`) is not implemented.
