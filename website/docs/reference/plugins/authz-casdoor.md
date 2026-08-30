---
title: authz-casdoor
description: Validate a Casdoor access token via OAuth introspection, or run the full interactive Casdoor SSO login flow with an encrypted client-side session cookie.
---

<span className="plugin-chip" style={{'--chip-color': '#f59e0b'}}>authz-casdoor</span>

Authenticates requests against [Casdoor](https://casdoor.org/). The node runs in one of two modes:

- **Stateless bearer-token validation (default).** A Casdoor-issued access token presented in the `Authorization` header is validated by calling Casdoor's OAuth **token introspection** endpoint (RFC 7662). An `active: true` response allows the request; anything else denies it. This is the pre-existing behavior and is unchanged.
- **Interactive SSO login (opt-in).** Configure a session secret to enable the full **OAuth Authorization Code** flow: unauthenticated browsers are redirected to Casdoor's authorize URL, the callback exchanges the `code` for an access token, and the token (plus decoded claims) is sealed into an **encrypted client-side session cookie**. Subsequent requests authenticate straight from the cookie — no server-side session store is required.

:::info Interactive mode is off by default
With **no** session secret configured the node behaves exactly as the stateless bearer-token validator described above. Setting `session_secret` (or `session.secret`) switches on the interactive flow, and `callback_url` then becomes required.
:::

## Configuration

| Key | Type | Default | Description |
|---|---|---|---|
| `endpoint_addr` | string | — | **Required.** Casdoor base URL (a trailing `/` is trimmed). |
| `client_id` | string | — | **Required.** Casdoor application client id. |
| `client_secret` | string | — | **Required.** Casdoor application client secret (HTTP Basic auth for introspection; client credentials for the code exchange). |
| `callback_url` | string | — | OAuth `redirect_uri`. **Required in interactive mode** (its path is matched against incoming requests to detect the callback); accepted but unused in stateless mode. |
| `ssl_verify` | boolean | `true` | Verify the endpoint's TLS certificate. |
| `timeout` | integer (ms) | `3000` | Callout timeout. |

### Interactive-mode keys

Setting a session secret enables the interactive login flow.

| Key | Type | Default | Description |
|---|---|---|---|
| `session_secret` / `session.secret` | string | — | Secret used to encrypt+authenticate the session and flow cookies. **Presence enables interactive mode.** A key is derived from it (SHA-256), so any length is accepted; the same value must be configured on every gateway instance. |
| `session.storage` (or `session_storage`) | string | `cookie` | `cookie` seals the session into the encrypted browser cookie. `redis` shrinks the session cookie to a bare random 128-bit id and stores the sealed payload server-side in the named `session.store`, enabling listing/revocation via the [Admin API](../../guides/admin-api.md) (`GET`/`DELETE /api/sessions`). The transient flow cookie (below) is unaffected — it always stays client-side. |
| `session.store` (or `session_store`) | string | — | Name of a declared `stores:` entry (redis/valkey). **Required when `session.storage: redis`**; resolved at policy-compile time — an unknown name fails compilation, never a request. |
| `session.cookie.name` (or `session_cookie_name`) | string | `casdoor_session` | Session cookie name. The transient login-flow cookie is `<name>_flow`. |
| `session.cookie.path` (or `session_cookie_path`) | string | `/` | Session/flow cookie `Path`. Scope it to the app's subpath (e.g. `/app_a`) so nodes on different subpaths keep independent sessions. **Must cover the `callback_url` path**, or login loops (rejected at load). |
| `session.cookie.lifetime` (or `session_cookie_lifetime`) | number (seconds) | `3600` | Session cookie lifetime (also the sealed payload's expiry). |
| `scope` | string | `read` | OAuth `scope` requested at the authorize step. |
| `logout_path` | string | — | Optional. A request to this path clears the session cookie and redirects to `/`. |

```yaml
# Stateless bearer-token validation (unchanged default)
- id: authz
  type: authz-casdoor
  config:
    endpoint_addr: https://casdoor.example.com
    client_id: ${CASDOOR_CLIENT_ID}
    client_secret: ${CASDOOR_CLIENT_SECRET}
    ssl_verify: true
    timeout: 3000

# Interactive SSO login
- id: authz
  type: authz-casdoor
  config:
    endpoint_addr: https://casdoor.example.com
    client_id: ${CASDOOR_CLIENT_ID}
    client_secret: ${CASDOOR_CLIENT_SECRET}
    callback_url: https://app.example.com/casdoor/callback
    session_secret: ${CASDOOR_SESSION_SECRET}
    scope: read
    logout_path: /logout
```

## Behavior

### Stateless mode (no session secret)

The token is read from the `Authorization` header (a `Bearer ` prefix is stripped if present). The plugin POSTs `token=<token>&token_type_hint=access_token` to `{endpoint_addr}/api/login/oauth/introspect`, authenticated with `Authorization: Basic base64(client_id:client_secret)`.

- HTTP `200` **and** `active: true` → **success** port, request continues.
- An inactive token, a non-`200` status, or a missing token → deliberate rejection, exits on the **`denied`** port with `context.response.status_code = 403`, body `{"error":"access_denied"}`.
- A genuine introspection-callout failure (unreachable, timed out, or otherwise untransportable) → the node could not do its job, so it exits on the ordinary **error** port instead (a `502` `{"error": "provider_error", "message": "<reason>"}` response — not the `403` `denied` shape — error code `AUTHZ_CASDOOR_ERROR`).

:::caution Breaking change
Before v0.8 this provider-failure response reused the `denied` shape (`403` `{"error": "access_denied"}`), so an unreachable Casdoor was indistinguishable from a denied token. It is now the shared `502 {"error": "provider_error", "message": "<reason>"}` response with no challenge header, the same shape every provider-backed auth plugin prepares (`openid-connect`, `cas-auth`, `ldap-auth`, `authz-keycloak`, `authz-casdoor`). Match on the `error` port / the error code, or on the `502`, instead of the old status.
:::

### Interactive mode (session secret set)

Each request is resolved through three branches:

1. **Callback.** When the request path matches the `callback_url` path **and** carries `code` + `state`, the plugin opens the short-lived flow cookie, checks the `state` matches, then exchanges the `code` at `{endpoint_addr}/api/login/oauth/access_token` (`grant_type=authorization_code`, with `client_id`/`client_secret`). The returned access token is sealed into a session cookie (its JWT claims, if any, are decoded and stored for identity), the flow cookie is deleted, and the browser is **302-redirected to the original URI** recovered from the flow cookie. A missing/invalid flow cookie or a `state` mismatch is a deliberate rejection and exits on `denied` with `403`. A failed code exchange (the token endpoint unreachable, timed out, returning non-`200`, or handing back unparseable data) is a genuine callout failure and exits on the ordinary **error** port instead (`502` `provider_error`, error code `AUTHZ_CASDOOR_ERROR`).
2. **Valid session cookie.** If the `<session.cookie.name>` cookie opens and its `client_id` matches, the access token is placed back on the upstream request as `Authorization: Bearer <token>`, the decoded claims are exposed as `context.message["user_id"]` (from `sub`) and `context.message["jwt_claims"]`, and the request continues through the **success** port.
3. **No session, not a callback.** A random `state` is generated and, together with the original request URI, sealed into a short-lived (300s) **flow cookie**; the browser is **302-redirected to Casdoor's authorize URL** (`{endpoint_addr}/login/oauth/authorize?response_type=code&client_id=…&redirect_uri=<callback_url>&state=…&scope=<scope>`) with the flow cookie set.

If `logout_path` is configured and the request path matches, the session cookie is deleted and the browser is redirected to `/`.

#### Server-side sessions (`session.storage: redis`)

Setting `session.storage: redis` (+ `session.store: <name>`) moves the sealed session payload into the named store; the browser only ever holds a bare 128-bit id for the session cookie. Sessions become listable and revocable through the [Admin API](../../guides/admin-api.md) (`GET`/`DELETE /api/sessions`) — `storage: cookie` sessions (the default) remain unrevocable by design, since nothing server-side tracks them. The short-lived **flow cookie** (state + original URI, used only across the redirect to Casdoor and back) stays client-side in **both** storage modes — it predates the session and never touches the store. A session-store failure on any operation (open, establish, revoke) never falls back to `403`: it exits through the ordinary **error** port as a `503` (error code `SESSION_STORE_ERROR`), because treating a store outage as "logged out" would just redirect the user into a login loop the store also can't complete.

#### Redirect wiring (important)

Every `302` in interactive mode (login redirect, post-callback redirect, logout) exits through the dedicated **`redirect`** output port, carrying the prepared `302` response. This follows the same convention as the standalone `redirect` node. **Wire the node's `redirect` edge to `client.in`** so the redirect (and its `Set-Cookie`) reaches the browser; wire `denied` to `client.in` too (or a custom denial handler) for deliberate rejections; wire the `success` edge onward to the upstream for authenticated requests.

#### Session cookie attributes

Both the session cookie and the transient flow cookie are set with `Path=<session.cookie.path>` (default `/`), `HttpOnly`, `SameSite=Lax`, and a `Max-Age` (the session lifetime, and 300s for the flow cookie). `Secure` is added **only when the request scheme is `https`**, so plain-HTTP local development works; run behind HTTPS in production so the cookies are marked `Secure`.

## Ports

`authz-casdoor` declares four output ports: `success`, `denied` (a deliberate `403` rejection is prepared — missing/inactive/invalid token, or an OAuth state mismatch), `redirect` (a `302` browser move is prepared — login, callback, or logout; interactive mode only, but the port is always declared), and `error` (a genuine Casdoor callout failure — introspection or token exchange unreachable — or, in `session.storage: redis` mode, a session-store failure: `503`, error code `SESSION_STORE_ERROR`). `denied` and `redirect` are both mandatory ports, same as `success`: the policy compiler rejects any policy that leaves either unwired, **even in stateless mode where `redirect` is never actually taken**. Wire both straight to `client`:

```yaml
edges:
  - from: authz-casdoor.success
    to: upstream.in
  - from: authz-casdoor.denied
    to: client.in
  - from: authz-casdoor.redirect
    to: client.in
```

## Deviations / Limitations

- **Interactive login is now supported** via an encrypted, authenticated client-side session cookie (AES-256-GCM) — no server-side session store is required, and any gateway instance sharing the secret can open any cookie, so this works across a horizontally-scaled deployment.
- **No server-side session revocation before expiry — in `session.storage: cookie` mode (the default).** Because those sessions live entirely in the client cookie, there is no way to invalidate an individual one before its `lifetime` elapses (short of rotating the secret, which invalidates *all* sessions). Use short lifetimes, or switch to `session.storage: redis` for revocation via the [Admin API](../../guides/admin-api.md) (`/api/sessions`).
- **No refresh-token handling in v1, in either storage mode.** The access token is stored as issued; when the session cookie expires the user is redirected through Casdoor login again. Refresh-token exchange is out of scope for this version.
- **Access-token signature is not re-verified.** The token comes directly from Casdoor's token endpoint over TLS and is trusted; its claims are decoded (not signature-checked) purely to surface identity to the upstream.
