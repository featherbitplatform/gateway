---
title: cas-auth
description: Validate a CAS service ticket, or run the full interactive CAS SSO login flow with an encrypted client-side session cookie.
---

<span className="plugin-chip" style={{'--chip-color': '#8b5cf6'}}>cas-auth</span>

Authenticates requests against a [CAS](https://apereo.github.io/cas/) (Central Authentication Service) server. The node runs in one of two modes:

- **Stateless ticket validation (default).** When a request carries a CAS `ticket` query parameter, the plugin validates it against the CAS server's `/serviceValidate` endpoint and, on success, attaches the authenticated user to the request. This is the pre-existing behavior and is unchanged.
- **Interactive SSO login (opt-in).** Configure a session secret to enable the full browser login flow: unauthenticated browsers are redirected to the CAS `/login` endpoint, the returned ticket is consumed at the callback, and the authenticated user is sealed into an **encrypted client-side session cookie**. Subsequent requests authenticate straight from the cookie — no server-side session store is required.

:::info Interactive mode is off by default
With **no** session secret configured the node behaves exactly as the stateless ticket validator described above. Setting `session.secret` (or `session_secret`) switches on the interactive flow.
:::

## Configuration

| Key | Type | Default | Description |
|---|---|---|---|
| `idp_uri` | string | — (**required**) | CAS server base URI. Validation goes to `<idp_uri>/serviceValidate`; interactive login goes to `<idp_uri>/login`. |
| `service` | string | — | Service URL passed to `/serviceValidate` and `/login`. When omitted it is derived from the request as `scheme://host/path`. Because CAS requires the validated service to match the login service, set this explicitly whenever the gateway sits behind a proxy. |
| `ticket_param` | string | `ticket` | Query parameter carrying the CAS service ticket. |
| `ssl_verify` | boolean | `true` | Verify the CAS server's TLS certificate. |
| `timeout_ms` | number | `3000` | Whole-call deadline for the validation callout. |

### Interactive-mode keys

Setting a session secret enables the interactive login flow.

| Key | Type | Default | Description |
|---|---|---|---|
| `session_secret` / `session.secret` | string | — | Secret used to encrypt+authenticate the session cookie. **Presence enables interactive mode.** A key is derived from it (SHA-256), so any length is accepted; the same value must be configured on every gateway instance. |
| `session.cookie.name` (or `session_cookie_name`) | string | `cas_session` | Session cookie name. |
| `session.cookie.path` (or `session_cookie_path`) | string | `/` | Session cookie `Path`. Scope it to the app's subpath (e.g. `/app_a`) so nodes on different subpaths keep independent sessions. |
| `session.cookie.lifetime` (or `session_cookie_lifetime`) | number (seconds) | `3600` | Session cookie lifetime (also the sealed payload's expiry). |
| `logout_path` | string | — | Optional. A request to this path clears the session cookie and redirects to `/`. |

```yaml
# Stateless ticket validation (unchanged default)
- id: auth
  type: cas-auth
  config:
    idp_uri: https://cas.example.org/cas
    service: https://app.example.org/
    ssl_verify: true

# Interactive SSO login
- id: auth
  type: cas-auth
  config:
    idp_uri: https://cas.example.org/cas
    service: https://app.example.org/
    session:
      secret: ${CAS_SESSION_SECRET}
      cookie:
        name: cas_session
        lifetime: 3600
    logout_path: /logout
```

`idp_uri` is required; a missing or blank value is rejected at config load.

## Behavior

### Stateless mode (no session secret)

1. The CAS ticket is read from the `ticket_param` query parameter. A missing ticket rejects immediately.
2. The plugin calls `GET <idp_uri>/serviceValidate?ticket=<ticket>&service=<service>`, where `service` is the configured value or the request-derived `scheme://host/path`.
3. The response is parsed for the authenticated user. Both the default CAS 2.0 **XML** (`<cas:authenticationSuccess>` / `<cas:user>`, with or without the `cas:` prefix) and the CAS 3.0 **JSON** (`serviceResponse.authenticationSuccess.user`) formats are supported.

On success the context passes through the **success** port, with the username exposed as `context.message["user"]` / `context.message["user_id"]` and injected onto the request as the `X-CAS-User` header. On a missing ticket, a non-200 validation response, an authentication-failure body, or a callout failure, the plugin rejects through the **error** port with `context.response.status_code = 401`, body `{"error": "unauthorized", "message": "<reason>"}`, and error code `CAS_AUTH_FAILED`.

### Interactive mode (session secret set)

Each request is resolved through three branches:

1. **Valid session cookie.** If the `<session.cookie.name>` cookie opens successfully, the sealed username is attached (`context.message["user"]` / `["user_id"]` and the `X-CAS-User` header) and the request continues through the **success** port.
2. **Callback (ticket present).** A request carrying the CAS `ticket` is validated via `/serviceValidate` (same logic as stateless mode). On success the username is sealed into a fresh session cookie and the browser is **302-redirected to the ticket-free service URL** with a `Set-Cookie`. On failure the request is rejected with `401` (`CAS_AUTH_FAILED`).
3. **No session, not a callback.** The browser is **302-redirected to `<idp_uri>/login?service=<service-url>`** to begin CAS login. (CAS returns the ticket to the same service URL, so no flow cookie is needed.)

If `logout_path` is configured and the request path matches, the session cookie is deleted and the browser is redirected to `/`.

#### Redirect wiring (important)

Every `302` in interactive mode (login redirect, post-callback redirect, logout) is emitted as an **error-port** exit with error code `CAS_REDIRECT`, carrying the prepared `302` response. This follows the same early-exit convention as the `fault-injection` and `mocking` nodes. **Wire the node's `error` edge to `client.in`** so the redirect (and its `Set-Cookie`) reaches the browser; wire the `success` edge onward to the upstream for authenticated requests.

#### Session cookie attributes

The session cookie is set with `Path=<session.cookie.path>` (default `/`), `HttpOnly`, `SameSite=Lax`, and `Max-Age=<session.cookie.lifetime>`. `Secure` is added **only when the request scheme is `https`**, so plain-HTTP local development works; run behind HTTPS in production so the cookie is marked `Secure`.

## Deviations / Limitations

- **Interactive login is now supported** via an encrypted, authenticated client-side session cookie (AES-256-GCM) — no server-side session store is required, and any gateway instance sharing the secret can open any cookie, so this works across a horizontally-scaled deployment.
- **No server-side session revocation before expiry.** Because sessions live entirely in the client cookie, there is no way to invalidate an individual session before its `lifetime` elapses (short of rotating the secret, which invalidates *all* sessions). Use short lifetimes. Server-side revocation is a possible future feature.
- **No CAS single-logout (SLO) callback.** The IdP-initiated back-channel logout POST is not handled; `logout_path` performs a simple local cookie clear + redirect only.
- **No ticket/proxy-ticket refresh.** There is no renewal handling in v1; when the cookie expires the user is redirected through CAS login again.
