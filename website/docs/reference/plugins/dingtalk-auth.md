---
title: dingtalk-auth
description: Validates a DingTalk authorization code against DingTalk's OAuth API and attaches the resolved user identity.
---

<span className="plugin-chip" style={{'--chip-color': '#1493ff'}}>dingtalk-auth</span>

Validates a DingTalk authorization **code** by exchanging it — through DingTalk's OAuth API — for the calling user's identity, then attaches that identity to the request for downstream nodes. A request whose code cannot be resolved to a DingTalk user is rejected with `401`.

The code is read from a request header (default `X-DingTalk-Code`), falling back to a query parameter (default `code`). The plugin fetches an app-level access token (cached in-process, ~7000s TTL) and calls DingTalk's `getuserinfo` endpoint with the code.

The node runs in one of two modes:

- **Stateless token validation (default).** With no `session.secret` configured, every request must carry a code, which is validated against DingTalk on each request. No cookie is read or set. This is the pre-existing behavior and is unchanged.
- **Session (opt-in).** Setting `session.secret` (or `session_secret`) restores APISIX's original session flow: the first request exchanges the code and establishes a session; later requests authenticate straight from the session, skipping the DingTalk callout; a request with neither a valid session nor a code is `302`-redirected to `redirect_uri`.

:::danger Breaking change
Because session mode adds a `302` browser move, `dingtalk-auth` moved onto the same port spec as `cas-auth`/`openid-connect`/`authz-casdoor`: it now declares a **`redirect`** output port, and — like `denied` — the policy compiler requires it to be wired, even when session mode is off and `redirect` is never actually taken. **Existing policies using this node must add a `redirect` edge (typically straight to `client.in`) or they will fail to compile.**
:::

## Configuration

| Key | Type | Default | Description |
|---|---|---|---|
| `app_key` | string | — | **Required.** DingTalk application key. |
| `app_secret` | string | — | **Required.** DingTalk application secret. |
| `code_header` | string | `X-DingTalk-Code` | Header the authorization code is read from first (matched case-insensitively). |
| `code_query` | string | `code` | Query parameter the code falls back to when the header is absent. |
| `access_token_url` | string | `https://api.dingtalk.com/v1.0/oauth2/accessToken` | DingTalk access-token endpoint. |
| `userinfo_url` | string | `https://oapi.dingtalk.com/topapi/v2/user/getuserinfo` | DingTalk userinfo endpoint. |
| `set_userinfo_header` | boolean | `true` | Base64-encode the resolved userinfo into the `X-Userinfo` request header for the upstream. |
| `timeout` | integer (ms) | `6000` | Per-callout timeout. |
| `ssl_verify` | boolean | `true` | Verify DingTalk's TLS certificate. |

### Session-mode keys

Setting a session secret enables the session flow described above.

| Key | Type | Default | Description |
|---|---|---|---|
| `session_secret` / `session.secret` | string | — | Secret used to encrypt+authenticate the session cookie. **Presence enables session mode.** The same value must be configured on every gateway instance. |
| `redirect_uri` | string | — | **Required in session mode.** Where to `302` a browser that has neither a valid session nor a code. |
| `session.cookie.name` (or `session_cookie_name`) | string | `dingtalk_session` | Session cookie name. |
| `session.cookie.path` (or `session_cookie_path`) | string | `/` | Session cookie `Path`. |
| `session.cookie.lifetime` (or `session_cookie_lifetime`) | integer (seconds) | `86400` | Session cookie lifetime (APISIX's `cookie_expires_in`). |
| `session.storage` (or `session_storage`) | string | `cookie` | `cookie` seals the userinfo JSON into the encrypted browser cookie. `redis` shrinks the cookie to a bare random 128-bit id and stores the sealed payload server-side in the named `session.store`, enabling listing/revocation via the [Admin API](../../guides/admin-api.md) (`GET`/`DELETE /api/sessions`). |
| `session.store` (or `session_store`) | string | — | Name of a declared `stores:` entry (redis/valkey). **Required when `session.storage: redis`**; resolved at policy-compile time — an unknown name fails compilation, never a request. |

`secret_fallbacks` (APISIX's multi-secret key-rotation) is **not** supported — the sealer is a single `session.secret`, same as every other session plugin in this codebase.

```yaml
# Stateless token validation (unchanged default)
- id: auth
  type: dingtalk-auth
  config:
    app_key: ${DINGTALK_APP_KEY}
    app_secret: ${DINGTALK_APP_SECRET}
    code_header: X-DingTalk-Code

# Session mode
- id: auth
  type: dingtalk-auth
  config:
    app_key: ${DINGTALK_APP_KEY}
    app_secret: ${DINGTALK_APP_SECRET}
    session:
      secret: ${DINGTALK_SESSION_SECRET}
    redirect_uri: https://login.example.com/start
```

## Behavior

### Stateless mode (no session secret)

The code is read from `code_header`, then `code_query`. With no code, the request is rejected. Otherwise the plugin obtains an access token (from cache or by calling `access_token_url`) and POSTs the code to `userinfo_url`; DingTalk's `errcode: 0` with a `result` object indicates success.

Any client-supplied `X-Userinfo` header is stripped before authentication.

On success the context passes through the **success** port:

- `context.message["dingtalk_userinfo"]` = the resolved DingTalk `result` object
- `context.message["user_id"]` = `userid` (or `unionid`), when present
- `X-Userinfo` request header = base64-encoded userinfo JSON (when `set_userinfo_header`)

On a missing code or a code DingTalk actively rejects (`errcode != 0`), the plugin rejects and exits through the **`denied`** port:

- `context.response.status_code` = `401`
- Body: `{"error": "unauthorized", "message": "<reason>"}` with `content-type: application/json`

A DingTalk callout that fails outright — network error, non-200 response, or an unparseable body, for either the access-token or userinfo call — is a genuine **infrastructure failure**, not a rejection. It stays on the **error** port instead (`context.response.status_code = 502`, error code `DINGTALK_UPSTREAM_ERROR`).

### Session mode (`session.secret` set)

Each request is resolved through three branches:

1. **Valid session.** If the `<session.cookie.name>` cookie (default `dingtalk_session`) opens successfully, the sealed userinfo is attached exactly as in stateless mode and the request continues through the **success** port — no DingTalk callout. An undecodable payload (corrupt/stale format) is destroyed and treated as no session, rather than failing the request.
2. **Code present, no valid session.** The existing token+userinfo callouts run unchanged; on success a new session is established (payload = the userinfo `result` JSON; subject = `userid` else `unionid` else empty; TTL = `session.cookie.lifetime`) and identity is attached with a `Set-Cookie` on the **success** response — deliberate denials and upstream failures behave exactly as in stateless mode.
3. **No session, no code.** The browser is **`302`-redirected to `redirect_uri`**, exiting on the **`redirect`** port.

A session-store failure (`session.storage: redis`) on any operation never falls back to `401`: it exits through the ordinary **error** port as a `503` (error code `SESSION_STORE_ERROR`), because treating a store outage as "logged out" would just redirect the user into a login loop the store also can't complete. `storage: cookie` sessions (the default) remain unrevocable by design; `session.storage: redis` sessions can be listed and revoked through the [Admin API](../../guides/admin-api.md).

#### Redirect wiring (important)

The session-mode `302` (no session, no code) exits through the dedicated **`redirect`** output port, carrying the prepared response. **Wire the node's `redirect` edge to `client.in`** so it reaches the browser — this is required even in stateless mode, where the port is declared but never actually taken.

## Ports

`dingtalk-auth` declares four output ports: `success`, `denied` (a deliberate denial is prepared — missing code, or DingTalk-rejected code), `redirect` (a `302` browser move is prepared — session mode only, but the port is always declared), and `error` (the DingTalk callout itself failed, or — in `session.storage: redis` mode — a session-store failure: `503`, error code `SESSION_STORE_ERROR`). `denied` and `redirect` are both mandatory ports, same as `success`: the policy compiler rejects any policy that leaves either unwired, **even in stateless mode where `redirect` is never actually taken**. Wire both straight to `client`:

```yaml
edges:
  - from: dingtalk-auth.success
    to: upstream.in
  - from: dingtalk-auth.denied
    to: client.in
  - from: dingtalk-auth.redirect
    to: client.in
```
