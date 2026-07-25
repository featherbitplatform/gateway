---
title: forward-auth
description: Delegates the access decision to an external HTTP authorization service, forwarding request metadata and mirroring the verdict back to the client or upstream.
---

<span className="plugin-chip" style={{'--chip-color': '#0ea5e9'}}>forward-auth</span>

Delegates the authorization decision for each request to an external HTTP service. The plugin issues a callout carrying the request's forwarding metadata (`X-Forwarded-*`) plus any configured client headers; a 2xx reply lets the request continue, while a non-2xx reply rejects it and mirrors the auth service's status, body, and selected headers back to the client. Place it early in the request pipeline, before the upstream node.

## Configuration

| Key | Type | Default | Description |
|---|---|---|---|
| `uri` | string | — | External authorization endpoint. **Required**; a missing or empty value is a config-load error. |
| `request_method` | string | `GET` | Callout method, `GET` or `POST`. When `POST`, the buffered client body is forwarded and the client's `Content-Encoding` header is preserved on the callout. Any other value is rejected at config load. |
| `request_headers` | array of strings | `[]` | Client header names copied onto the callout (looked up case-insensitively). |
| `upstream_headers` | array of strings | `[]` | Auth-response header names copied onto the request forwarded upstream on success. A configured name absent from the auth response removes any client-supplied value. |
| `client_headers` | array of strings | `[]` | Auth-response header names copied onto the client-facing response on failure. |
| `extra_headers` | object (string→string) | — | Additional callout headers; values support `$var` / `${var}` interpolation (e.g. `$remote_addr`, `$request_uri`). |
| `ssl_verify` | boolean | `true` | Verify TLS certificates for `https` callouts. |
| `timeout` | integer (ms) | `3000` | Whole-call callout deadline (connect + request + response). |
| `status_on_error` | integer | `403` | Status returned when the callout fails and `allow_degradation` is false. |
| `allow_degradation` | boolean | `false` | When true, a callout failure lets the request continue instead of rejecting it (fail-open). |

```yaml
- id: auth
  type: forward-auth
  config:
    uri: http://auth-service:8080/verify
    request_method: GET
    request_headers: [authorization, cookie]
    upstream_headers: [x-user-id]
    client_headers: [www-authenticate]
    ssl_verify: true
    timeout: 3000
    status_on_error: 403
    allow_degradation: false
```

## Behavior

On each request the plugin builds a callout to `uri` carrying:

- `X-Forwarded-Proto` = request scheme
- `X-Forwarded-Method` = request method
- `X-Forwarded-Host` = request host
- `X-Forwarded-Uri` = request path plus query string
- `X-Forwarded-For` = client IP
- `Content-Encoding` (only when `request_method: POST`), plus the buffered client body
- any `extra_headers` (with `$var` values resolved), and each configured `request_headers` copied from the client request

The callout's reply determines routing:

- **2xx** → the request passes through the **success** port. Each configured `upstream_headers` is copied from the auth response onto the request forwarded upstream; a configured header absent from the auth response is removed.
- **Non-2xx** (status ≥ 300) → the request is rejected through the **error** port. The auth service's status and body are mirrored onto `context.response`, the configured `client_headers` are copied from the auth response, and the error `FORWARD_AUTH_DENIED` is appended to `context.errors`.
- **Callout failure** (timeout / transport error) → if `allow_degradation` is true the request continues unchanged (fail-open); otherwise the request is rejected with `context.response.status_code = status_on_error` and the error `FORWARD_AUTH_ERROR`.
