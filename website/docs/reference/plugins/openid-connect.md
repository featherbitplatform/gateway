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

On a missing token or any verification failure (bad signature, expired, unknown `kid`, wrong issuer/audience, inactive introspection result), the plugin rejects and routes through the **error** port:

- `context.response.status_code` = `401`
- `WWW-Authenticate: Bearer error="invalid_token"`
- Body: `{"error": "unauthorized", "message": "<reason>"}` with `content-type: application/json`
- Error code appended to `context.errors`: `OIDC_UNAUTHORIZED`

## Interactive login

With `bearer_only: false` the plugin runs the browser-facing **Authorization Code flow with PKCE**, keeping all state in an **encrypted client-side cookie** (see the [cookie-session codec](../../concepts/architecture.md)) — no server-side session store, so it scales horizontally as long as every instance shares `session.secret`.

The node handles three cases per request:

1. **Valid session cookie** → the sealed claims are attached to `context.message` (`jwt_claims`, `user_id`) and the request continues out the **success** port to the upstream.
2. **Callback** (request path = `redirect_uri` path, carrying `code` + `state`) → the plugin verifies `state` against the flow cookie (CSRF), exchanges the code at the token endpoint (with the PKCE `code_verifier`), validates the `id_token` against the JWKS and checks its `nonce`, seals a session cookie, and `302`-redirects to the originally requested URL.
3. **No session** → generates `state`/`nonce`/PKCE, sets a short-lived flow cookie, and `302`-redirects to the IdP authorization endpoint.

**Wiring:** in interactive mode the node exits through its **error port** for every browser redirect (`OIDC_REDIRECT`) and for auth failures (`OIDC_UNAUTHORIZED`) — the prepared `302`/`401` response is already on the context. **Wire the node's `error` edge to `client.in`.** Only a request with a valid session cookie leaves the `success` port. The node must sit on a route whose match rule also covers the `redirect_uri` path, so the callback reaches it.

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
Sessions live entirely in the encrypted cookie, so there is **no server-side revocation** before the cookie's `lifetime` expires (use short lifetimes) and **no token refresh** yet — an expired session triggers a fresh, fast redirect round-trip. Only the Authorization Code grant is implemented. Server-side sessions with instant revocation would need a shared store (a future feature).
:::
