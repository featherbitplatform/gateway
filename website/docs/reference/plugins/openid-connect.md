---
title: openid-connect
description: OIDC/OAuth2 — bearer-token validation (JWKS or introspection) and the full interactive Authorization Code login flow with encrypted cookie sessions.
---

<span className="plugin-chip" style={{'--chip-color': '#6d28d9'}}>openid-connect</span>

Authenticates requests against an OpenID Connect provider in one of two modes, selected by `bearer_only`:

- **Resource-server / bearer mode** (`bearer_only: true`, the default) — validates an OAuth2 / OIDC **access token** presented as a `Bearer` token in the `Authorization` header, and exposes the claims to downstream nodes via `context.message`. This is the mode a gateway fronting APIs uses.
- **Interactive login** (`bearer_only: false`) — the full **Authorization Code flow with PKCE**, for browser-facing apps. See [Interactive login](#interactive-login) below.

In bearer mode a token is validated by **one** of two strategies:

- **Local JWT verification via JWKS** — preferred when `discovery` or `jwks_uri` is set. The signature is verified against the matching JWK (selected by the token's `kid`) fetched from the provider's JWKS endpoint, then `exp` and the configured issuer/audience claims are checked. The JWKS is cached in-process with a TTL; an unknown `kid` triggers a single refetch to pick up rotated keys.
- **Token introspection (RFC 7662)** — used when only `introspection_endpoint` is configured. The token is POSTed to the introspection endpoint with client credentials (HTTP Basic) and accepted only when the response contains `active: true`.

## Configuration

| Key | Type | Default | Description |
|---|---|---|---|
| `bearer_only` | boolean | `true` | `true` = validate bearer tokens; `false` = run the interactive login flow (requires the interactive keys below). |
| `discovery` | string | — | OIDC discovery URL (`.../.well-known/openid-configuration`); resolves `jwks_uri` and, in interactive mode, the authorization/token endpoints. |
| `jwks_uri` | string | — | Explicit JWKS endpoint; takes precedence over `discovery` for signature verification. |
| `introspection_endpoint` | string | — | RFC 7662 introspection endpoint; used (bearer mode) only when no JWKS source is configured. |
| `client_id` | string | — | OAuth client id (introspection auth, audience matching, and the interactive flow). |
| `client_secret` | string | — | OAuth client secret. Required for introspection and interactive mode. |
| `token_signing_alg_values_expected` | string or array | `RS256, RS384, RS512, ES256, ES384` | Permitted signature algorithms. A token signed with any other algorithm is rejected. The set is automatically narrowed to those matching the JWKS key's family before verification, so the mixed RSA/EC default works with whichever key type the IdP publishes — no need to pin it per deployment. |
| `claim_validator.issuer.valid_issuers` | array | — | Accepted `iss` values. When empty, the issuer is not validated. |
| `claim_validator.audience.claim` | string | `aud` | Claim name to read the audience from. |
| `claim_validator.audience.required` | boolean | `false` | Reject the token when the audience claim is absent. |
| `claim_validator.audience.match_with_client_id` | boolean | `false` | Require the audience to equal (or, for an array, contain) `client_id`. |
| `set_userinfo_header` | boolean | `true` | Base64-encode the validated claims into the `X-Userinfo` request header for the upstream. |
| `set_access_token_header` | boolean | `true` | Forward the validated access token to the upstream. |
| `access_token_in_authorization_header` | boolean | `false` | When forwarding, keep the token in `Authorization: Bearer …` instead of `X-Access-Token`. |
| `ssl_verify` | boolean | `true` | Verify the identity provider's TLS certificate on JWKS/introspection callouts. |
| `timeout` | integer (seconds) | `3` | Per-callout timeout. |
| `jwk_expires_in` | integer (seconds) | `86400` | TTL of the in-process JWKS cache. |

Interactive-mode keys (used only when `bearer_only: false`):

| Key | Type | Default | Description |
|---|---|---|---|
| `session.secret` (or `session_secret`) | string | — (**required**) | Secret used to seal the encrypted session cookie. Must be identical on every gateway instance. |
| `redirect_uri` | string | — (**required**) | The callback URL the IdP redirects to after login. Its path must be covered by the node's route match rule. |
| `authorization_endpoint` / `token_endpoint` | string | from `discovery` | Explicit endpoints; needed only when `discovery` is not set. |
| `scope` | string | `openid` | OAuth scopes requested. |
| `session.cookie.name` (or `session_cookie_name`) | string | `oidc_session` | Session cookie name (the transient flow cookie is `<name>_flow`). |
| `session.cookie.path` (or `session_cookie_path`) | string | `/` | `Path` attribute of the session and flow cookies. Scope it to the app's subpath (e.g. `/app_a`) so two nodes on different subpaths keep independent sessions. **Must cover the `redirect_uri` path**, or login loops (rejected at load). |
| `session.cookie.lifetime` (or `session_cookie_lifetime`) | integer (seconds) | `3600` | Session cookie lifetime. |
| `session.storage` (or `session_storage`) | string | `cookie` | `cookie` seals the whole session into the encrypted browser cookie — no server-side store. `redis` shrinks the cookie to a bare random 128-bit id and stores the sealed payload server-side in the named `session.store`, enabling listing/revocation via the [Admin API](../../guides/admin-api.md) (`GET`/`DELETE /api/sessions`) and coordinated token refresh (`session.refresh`, below). |
| `session.store` (or `session_store`) | string | — | Name of a declared `stores:` entry (redis/valkey). **Required when `session.storage: redis`**; resolved at policy-compile time — an unknown name fails compilation, never a request. |
| `session.refresh` (or `session_refresh`) | boolean | `true` | `session.storage: redis` only: transparently refresh a near-expiry access token (using the `refresh_token` captured at login) before attaching identity, coordinated across gateway instances by a short-lived per-session store lock. Ignored in `cookie` mode, which never refreshes. See [Token refresh](#token-refresh). |

The flat `session_secret` / `session_cookie_*` forms are what the **Web UI** node-config form emits (its form is flat and cannot author nested maps); the nested `session:` map is equivalent and takes precedence when both are present.
| `logout_path` | string | — | When set, a request to this path clears the session and redirects. |
| `post_logout_redirect_uri` | string | `/` | Where to send the browser after logout. |

```yaml
# JWKS verification via discovery
- id: auth
  type: openid-connect
  config:
    discovery: https://idp.example.com/.well-known/openid-configuration
    bearer_only: true
    client_id: my-api
    token_signing_alg_values_expected: RS256
    claim_validator:
      issuer:
        valid_issuers: ["https://idp.example.com/"]
      audience:
        required: true
        match_with_client_id: true
```

```yaml
# Token introspection
- id: auth
  type: openid-connect
  config:
    introspection_endpoint: https://idp.example.com/oauth2/introspect
    client_id: my-api
    client_secret: ${OIDC_CLIENT_SECRET}
    bearer_only: true
```

## Behavior

The bearer token is read from the `Authorization` header. A missing or malformed token, or any validation failure, rejects the request.

On success the context passes through the **success** port with the claims exposed to downstream nodes:

- `context.message["jwt_claims"]` = the full claims object (JWT payload, or the introspection response)
- `context.message["user_id"]` = the `sub` claim, when present
- `X-Userinfo` request header = base64-encoded claims JSON (when `set_userinfo_header`)
- `X-Access-Token` (or `Authorization`) forwarded to the upstream (when `set_access_token_header`)

Any client-supplied `X-Userinfo` header is stripped before validation so it cannot bleed through to the upstream.

On a missing token, or the token being deliberately invalid (bad signature, expired, unknown `kid`, wrong issuer/audience, an introspection response saying `active: false`), the plugin rejects and exits through the **`denied`** port:

- `context.response.status_code` = `401`
- `WWW-Authenticate: Bearer error="invalid_token"`
- Body: `{"error": "unauthorized", "message": "<reason>"}` with `content-type: application/json`

The missing-token reason names the mode — `No bearer token found in request (bearer_only is true; set bearer_only: false for interactive login)` — so a node that was *meant* to run the interactive flow but is still compiled in bearer mode (for example because the save that flipped `bearer_only` was rejected and the last-good config kept running) is recognizable from the response body alone.

A **genuine provider failure** — the discovery document, JWKS endpoint, or introspection/token endpoint being unreachable, timing out, returning a non-2xx status, or handing back unparseable data — is not a token rejection; the node could not do its job, so it exits through the ordinary **error** port instead, and the prepared response says so rather than masquerading as an authentication decision:

- `context.response.status_code` = `502`
- no `WWW-Authenticate` challenge
- Body: `{"error": "provider_error", "message": "<reason>"}` with `content-type: application/json`
- error code `OIDC_PROVIDER_ERROR` appended to `context.errors` (and a `WARN` log line naming the policy and node)

This applies in both modes. In interactive mode it is what a browser user sees when the IdP cannot even be reached to start the login: a `502`, not a `401` and not a redirect loop.

:::caution Breaking change
Before v0.8, provider failures reused the `denied` response shape (`401`, `WWW-Authenticate: Bearer`, `{"error": "unauthorized"}`). Clients that keyed on that `401` to distinguish "IdP down" from "token rejected" could not, and browser users saw an "unauthorized" JSON body instead of a login redirect. Match on the `error` port / `OIDC_PROVIDER_ERROR`, or on the `502`, instead.
:::

## Interactive login

With `bearer_only: false` the plugin runs the browser-facing **Authorization Code flow with PKCE**. By default (`session.storage: cookie`) all state stays in an **encrypted client-side cookie** (see the [cookie-session codec](../../concepts/architecture.md)) — no server-side session store, so it scales horizontally as long as every instance shares `session.secret`. Setting `session.storage: redis` (+ `session.store: <name>`) moves the sealed payload server-side instead: the cookie shrinks to a bare 128-bit id, and sessions become listable and revocable through the [Admin API](../../guides/admin-api.md) (`GET`/`DELETE /api/sessions`) — `storage: cookie` sessions remain unrevocable by design, since nothing server-side tracks them. The **transient login-flow cookie** (`<name>_flow`, carrying the pre-login `state`/`nonce`/PKCE material) stays client-side in **both** modes; it predates the session and never touches the store. A session-store failure (on load, establish, or refresh) never falls back to `401` — it exits through the ordinary **error** port as a `503` (error code `SESSION_STORE_ERROR`), because treating a store outage as "logged out" would just redirect the user into a login loop the store also can't complete.

The node handles three cases per request:

1. **Valid session cookie** → the sealed claims are attached to `context.message` (`jwt_claims`, `user_id`) and the request continues out the **success** port to the upstream.
2. **Callback** (request path = `redirect_uri` path, carrying `code` + `state`) → the plugin verifies `state` against the flow cookie (CSRF), exchanges the code at the token endpoint (with the PKCE `code_verifier`), validates the `id_token` against the JWKS and checks its `nonce`, seals a session cookie, and `302`-redirects to the originally requested URL.
3. **No session** → generates `state`/`nonce`/PKCE, sets a short-lived flow cookie, and `302`-redirects to the IdP authorization endpoint.

#### Token refresh

`session.storage: redis` only, and only when `session.refresh` (default `true`) is set: a request presenting a session whose access token is near expiry is refreshed before identity is attached. The node takes a short-lived (10s, `SET NX PX`) store lock so only one gateway instance calls the token endpoint concurrently; the loser re-reads the (usually already-refreshed) session instead of also calling the IdP. The refreshed tokens — and any re-validated `id_token` claims — replace the session in the store under the same id, so the cookie itself never changes. A refresh failure at the IdP (unreachable, non-2xx, or an invalid refreshed `id_token`) is **not** a store outage: rather than a `503`, the request simply falls back to a fresh login, the same as an expired cookie-mode session. Unlocking after a refresh attempt is best-effort (the lock self-heals via its TTL either way). `session.storage: cookie` never attempts a refresh, regardless of `session.refresh`.

**Wiring:** in interactive mode every browser move (the `302` to the IdP, the post-callback `302`, or a logout redirect) exits through the dedicated **`redirect`** port with the prepared response already on the context — **wire the node's `redirect` edge to `client.in`.** A deliberate rejection (missing/invalid flow cookie, CSRF `state` mismatch, an invalid `id_token`, a nonce mismatch) exits through **`denied`** — wire that to `client.in` too, or a custom denial handler. A genuine discovery/token-endpoint callout failure exits through the ordinary **error** port. Only a request with a valid session cookie leaves the `success` port. The node must sit on a route whose match rule also covers the `redirect_uri` path, so the callback reaches it.

Cookies are `HttpOnly`, `SameSite=Lax`, and `Secure` when the request is HTTPS.

```yaml
- id: login
  type: openid-connect
  config:
    bearer_only: false
    discovery: https://idp.example.com/.well-known/openid-configuration
    client_id: web-app
    client_secret: ${OIDC_CLIENT_SECRET}
    redirect_uri: https://app.example.com/oidc/callback
    scope: openid profile email
    session:
      secret: ${SESSION_SECRET}
      cookie:
        name: oidc_session
        lifetime: 3600
    logout_path: /logout
```

#### Independent sessions per subpath

Two `openid-connect` nodes on different routes can hold separate browser sessions by giving each its own cookie **name** and **path**. Scoping the path means the browser only sends `a_session` to `/app_a/*` and `b_session` to `/app_b/*`, so the two apps never share or clobber each other's login. Each `session.cookie.path` must cover its own `redirect_uri` path.

```yaml
# Route /app_a/* -> this node
- id: login-a
  type: openid-connect
  config:
    bearer_only: false
    discovery: https://idp.example.com/.well-known/openid-configuration
    client_id: app-a
    client_secret: ${APP_A_SECRET}
    redirect_uri: https://example.com/app_a/callback
    session:
      secret: ${SESSION_SECRET}
      cookie:
        name: a_session
        path: /app_a
        lifetime: 1800

# Route /app_b/* -> a second node
- id: login-b
  type: openid-connect
  config:
    bearer_only: false
    discovery: https://idp.example.com/.well-known/openid-configuration
    client_id: app-b
    client_secret: ${APP_B_SECRET}
    redirect_uri: https://example.com/app_b/callback
    session:
      secret: ${SESSION_SECRET}
      cookie:
        name: b_session
        path: /app_b
        lifetime: 3600
```

:::note Limitations
In the default `session.storage: cookie` mode, sessions live entirely in the encrypted cookie: there is **no server-side revocation** before the cookie's `lifetime` expires (use short lifetimes) and **no token refresh** — an expired session triggers a fresh, fast redirect round-trip. `session.storage: redis` lifts both limits (revocation via `/api/sessions`, refresh via `session.refresh`) at the cost of requiring a declared `stores:` entry. Only the Authorization Code grant is implemented.
:::

## Ports

`openid-connect` declares four output ports: `success`, `denied` (a deliberate `401` rejection is prepared — missing/invalid bearer token, or in interactive mode a CSRF/nonce/session-flow failure), `redirect` (a `302` browser move is prepared — login, callback, or logout; interactive mode only, but the port is always declared), and `error` (a genuine provider failure — discovery, JWKS, or introspection/token-endpoint callout trouble: `502`, error code `OIDC_PROVIDER_ERROR` — or, in `session.storage: redis` mode, a session-store failure: `503`, error code `SESSION_STORE_ERROR`). `denied` and `redirect` are both mandatory ports, same as `success`: the policy compiler rejects any policy that leaves either unwired, **even in bearer-only mode where `redirect` is never actually taken**. Wire both straight to `client`:

```yaml
edges:
  - from: openid-connect.success
    to: upstream.in
  - from: openid-connect.denied
    to: client.in
  - from: openid-connect.redirect
    to: client.in
```
