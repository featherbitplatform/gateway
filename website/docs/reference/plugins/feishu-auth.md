---
title: feishu-auth
description: Validates a Feishu/Lark authorization code via Feishu's OAuth API and attaches the resolved user identity.
---

<span className="plugin-chip" style={{'--chip-color': '#00c2a8'}}>feishu-auth</span>

Validates a Feishu / Lark authorization **code** by exchanging it — through Feishu's OAuth v2 token endpoint — for a user access token, then calls Feishu's userinfo endpoint to resolve the calling user's identity and attaches it to the request. A code that cannot be resolved is rejected with `401`.

The code is read from a request header (default `X-Feishu-Code`), falling back to a query parameter (default `code`).

The node runs in one of two modes:

- **Stateless token validation (default).** With no `session.secret` configured, every request must carry a code, which is exchanged and validated against Feishu on each request. No cookie is read or set. This is the pre-existing behavior and is unchanged.
- **Session (opt-in).** Setting `session.secret` (or `session_secret`) restores APISIX's original session flow: the first request exchanges the code and establishes a session; later requests authenticate straight from the session, skipping the Feishu callouts; a request with neither a valid session nor a code is `302`-redirected to `redirect_uri` (distinct from `auth_redirect_uri`, which stays required always as the token-exchange body field).

:::danger Breaking change
Because session mode adds a `302` browser move, `feishu-auth` moved onto the same port spec as `cas-auth`/`openid-connect`/`authz-casdoor`/`dingtalk-auth`: it now declares a **`redirect`** output port, and — like `denied` — the policy compiler requires it to be wired, even when session mode is off and `redirect` is never actually taken. **Existing policies using this node must add a `redirect` edge (typically straight to `client.in`) or they will fail to compile.**
:::

## Configuration

| Key | Type | Default | Description |
|---|---|---|---|
| `app_id` | string | — | **Required.** Feishu application id. |
| `app_secret` | string | — | **Required.** Feishu application secret. |
| `auth_redirect_uri` | string | — | **Required.** The `redirect_uri` registered with Feishu; sent in the `authorization_code` token-exchange body and must match the one used to obtain the code. Always required, in both modes — distinct from the session-mode `redirect_uri` below. |
| `code_header` | string | `X-Feishu-Code` | Header the code is read from first (matched case-insensitively). |
| `code_query` | string | `code` | Query parameter the code falls back to. |
| `access_token_url` | string | `https://open.feishu.cn/open-apis/authen/v2/oauth/token` | Feishu token endpoint. |
| `userinfo_url` | string | `https://open.feishu.cn/open-apis/authen/v1/user_info` | Feishu userinfo endpoint. |
| `set_userinfo_header` | boolean | `true` | Base64-encode the resolved userinfo into the `X-Userinfo` request header for the upstream. |
| `timeout` | integer (ms) | `6000` | Per-callout timeout. |
| `ssl_verify` | boolean | `true` | Verify Feishu's TLS certificate. |

### Session-mode keys

Setting a session secret enables the session flow described above.

| Key | Type | Default | Description |
|---|---|---|---|
| `session_secret` / `session.secret` | string | — | Secret used to encrypt+authenticate the session cookie. **Presence enables session mode.** The same value must be configured on every gateway instance. |
| `redirect_uri` | string | — | **Required in session mode.** Where to `302` a browser that has neither a valid session nor a code. Distinct from `auth_redirect_uri`, which stays required always. |
| `session.cookie.name` (or `session_cookie_name`) | string | `feishu_session` | Session cookie name. |
| `session.cookie.path` (or `session_cookie_path`) | string | `/` | Session cookie `Path`. |
| `session.cookie.lifetime` (or `session_cookie_lifetime`) | integer (seconds) | `86400` | Session cookie lifetime (APISIX's `cookie_expires_in`). |
| `session.storage` (or `session_storage`) | string | `cookie` | `cookie` seals the session payload into the encrypted browser cookie. `redis` shrinks the cookie to a bare random 128-bit id and stores the sealed payload server-side in the named `session.store`, enabling listing/revocation via the [Admin API](../../guides/admin-api.md) (`GET`/`DELETE /api/sessions`). |
| `session.store` (or `session_store`) | string | — | Name of a declared `stores:` entry (redis/valkey). **Required when `session.storage: redis`**; resolved at policy-compile time — an unknown name fails compilation, never a request. |

`secret_fallbacks` (APISIX's multi-secret key-rotation) is **not** supported — the sealer is a single `session.secret`, same as every other session plugin in this codebase.

```yaml
# Stateless token validation (unchanged default)
- id: auth
  type: feishu-auth
  config:
    app_id: ${FEISHU_APP_ID}
    app_secret: ${FEISHU_APP_SECRET}
    auth_redirect_uri: https://app.example.com/callback

# Session mode
- id: auth
  type: feishu-auth
  config:
    app_id: ${FEISHU_APP_ID}
    app_secret: ${FEISHU_APP_SECRET}
    auth_redirect_uri: https://app.example.com/callback
    session:
      secret: ${FEISHU_SESSION_SECRET}
    redirect_uri: https://login.example.com/start
```

## Behavior

### Stateless mode (no session secret)

The code is read from `code_header`, then `code_query`. With no code, the request is rejected. Otherwise the plugin POSTs an `authorization_code` grant to `access_token_url` to obtain a user access token, then GETs `userinfo_url` with `Authorization: Bearer <token>`; Feishu's `code: 0` with a `data` object indicates success.

Any client-supplied `X-Userinfo` header is stripped before authentication.

On success the context passes through the **success** port:

- `context.message["feishu_userinfo"]` = the resolved Feishu `data` object
- `context.message["user_id"]` = `user_id` (or `open_id` / `union_id`), when present
- `X-Userinfo` request header = base64-encoded userinfo JSON (when `set_userinfo_header`)

On a missing code or a code/token Feishu actively rejects (non-zero `code`), the plugin rejects and exits through the **`denied`** port:

- `context.response.status_code` = `401`
- Body: `{"error": "unauthorized", "message": "<reason>"}` with `content-type: application/json`

A Feishu callout that fails outright — network error, non-200 response, or an unparseable body, for either the token or userinfo call — is a genuine **infrastructure failure**, not a rejection. It stays on the **error** port instead (`context.response.status_code = 502`, error code `FEISHU_UPSTREAM_ERROR`).

### Session mode (`session.secret` set)

Each request is resolved through three branches:

1. **Valid session.** If the `<session.cookie.name>` cookie (default `feishu_session`) opens successfully, the sealed userinfo (plus the exchanged access token and its expiry) is attached exactly as in stateless mode and the request continues through the **success** port — no Feishu callout. An undecodable payload (corrupt/stale format) is destroyed and treated as no session, rather than failing the request.
2. **Code present, no valid session.** The existing token+userinfo callouts run unchanged; on success a new session is established (payload = userinfo JSON plus the exchanged access token and its expiry; subject = `user_id` else `open_id` else `union_id` else empty; TTL = `session.cookie.lifetime`) and the browser is **`302`-redirected to the current URL with the `code` query parameter stripped**, carrying the session `Set-Cookie`, exiting on the **`redirect`** port — deliberate denials and upstream failures behave exactly as in stateless mode. The node does **not** attach identity or exit `success` on this request: in the standard `success → upstream.in` wiring, the `upstream` node replaces `ctx.response.headers` wholesale, so a `Set-Cookie` set on that path would never reach the browser. The browser's follow-up request (now cookie-bearing, code-free) hits branch 1 above and gets identity attached there.
3. **No session, no code.** The browser is **`302`-redirected to `redirect_uri`**, exiting on the **`redirect`** port.

A session-store failure (`session.storage: redis`) on any operation never falls back to `401`: it exits through the ordinary **error** port as a `503` (error code `SESSION_STORE_ERROR`), because treating a store outage as "logged out" would just redirect the user into a login loop the store also can't complete. `storage: cookie` sessions (the default) remain unrevocable by design; `session.storage: redis` sessions can be listed and revoked through the [Admin API](../../guides/admin-api.md).

#### Redirect wiring (important)

Both session-mode `302`s — beginning login (no session, no code) and completing it (session established after a code exchange) — exit through the dedicated **`redirect`** output port, carrying the prepared response (including, for the latter, the session `Set-Cookie`). **Wire the node's `redirect` edge to `client.in`** so it reaches the browser — this is required even in stateless mode, where the port is declared but never actually taken.

## Ports

`feishu-auth` declares four output ports: `success`, `denied` (a deliberate denial is prepared — missing code, or Feishu-rejected code/token), `redirect` (a `302` browser move is prepared — session mode only, but the port is always declared), and `error` (the Feishu callout itself failed, or — in `session.storage: redis` mode — a session-store failure: `503`, error code `SESSION_STORE_ERROR`). `denied` and `redirect` are both mandatory ports, same as `success`: the policy compiler rejects any policy that leaves either unwired, **even in stateless mode where `redirect` is never actually taken**. Wire both straight to `client`:

```yaml
edges:
  - from: feishu-auth.success
    to: upstream.in
  - from: feishu-auth.denied
    to: client.in
  - from: feishu-auth.redirect
    to: client.in
```
