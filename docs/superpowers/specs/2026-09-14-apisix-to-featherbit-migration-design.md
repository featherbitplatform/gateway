# APISIX → featherbit migration design (featherbit-45)

**Date:** 2026-09-14
**Target:** the `featherbit-45` gateway instance (featherbit 0.8.0, debug + sandbox on)
**Source:** an Apache APISIX 3.x standalone config — `global_rules`, 18 routes, 5 services, 4 `plugin_configs`

## 1. Goal

Reproduce the behavior of the ESRA/EPG/THOT/Keycloak APISIX deployment on featherbit,
expressed natively as node-graph policies rather than as a mechanical transliteration.
Delivery is **live via the Admin MCP** onto `featherbit-45`, followed by an exported
`gateway.yaml` for the operator to persist.

## 2. Decisions taken during design

| Question | Decision |
|---|---|
| `global_rules` elasticsearch-logger | **Dropped.** Only the security-header half of the global rule is migrated. |
| `proxy-rewrite: regex_uri` | **Not needed.** All four rewrites reduce exactly to `strip_path_prefix` + `add_path_prefix`. |
| `_meta.disable: ${{FLAG}}` | **Dropped.** No `DISABLE_MONITORING` / `SWAGGER_ENABLED` toggles. |
| OIDC bearer-token acceptance | **Dropped.** featherbit's interactive mode is session-cookie only; that is accepted. |
| `plugin_config 4` (swagger 404) | **Dropped.** `/swagger/*` and `/thot/swagger/*` become authenticated passthroughs. |
| Secrets and hosts | **`${ENV}` placeholders**, resolved at graph-compile time, never served resolved by the Admin API. |
| Shared-pipeline factoring | **Supernode** (`auth-gate`) + shared `plugin_configs`. |
| OIDC callback path | **`/.featherbit/redirect`** (was `/.apisix/redirect`). Requires a Keycloak client update. |
| Pre-existing instance config | `public-fe` route and the `public-fe-policy` / `echo-policy` policies are **deleted**. |

## 3. Construct mapping

| APISIX | featherbit |
|---|---|
| `global_rules[].plugins.response-rewrite` | `plugin_configs: security-headers`, referenced by a `response-rewrite` node in every policy |
| `services[]` | an `upstream` node inside the policy that needs it |
| `plugin_configs[1]` (cors + openid-connect) | `supernodes: auth-gate` |
| `plugin_configs[1].redirect ^/login → /` | its own route + `login-redirect` policy |
| `plugin_configs[1].proxy-rewrite regex_uri` | `proxy-rewrite` nodes in the `keycloak-admin` policy |
| `plugin_configs[3]` (uri-blocker + rewrite) | the `keycloak-public` policy |
| `plugin_configs[4]` | dropped |
| `routes[]` | `routes:` entries, **ordered** — see §7 |

## 4. Shared plugin configs

```yaml
plugin_configs:
  - name: security-headers
    type: response-rewrite
    description: Global security response headers (APISIX global_rules equivalent)
    config:
      headers:
        set:
          X-Frame-Options: deny
          X-Content-Type-Options: nosniff
          X-Permitted-Cross-Domain-Policies: none
          Strict-Transport-Security: "max-age=31536000; includeSubDomains"
          Cross-Origin-Embedder-Policy: require-corp
          Cross-Origin-Resource-Policy: same-origin
          Cross-Origin-Opener-Policy: same-origin
          Cache-Control: "no-store, max-age=0"
          Referrer-Policy: no-referrer
          Permissions-Policy: "accelerometer=(), autoplay=(), camera=(), cross-origin-isolated=(), display-capture=(), encrypted-media=(), fullscreen=(), geolocation=(), gyroscope=(), keyboard-map=(), magnetometer=(), microphone=(), midi=(), payment=(), picture-in-picture=(), publickey-credentials-get=(), screen-wake-lock=(), sync-xhr=(self), usb=(), web-share=(), xr-spatial-tracking=(), clipboard-read=(), clipboard-write=(), gamepad=(), hid=(), idle-detection=(), interest-cohort=(), serial=(), unload=()"
          Content-Security-Policy: "connect-src *"
        remove:
          - Server

  - name: gateway-error
    type: error-handler
    description: Uniform 502 for upstream and IdP failures
    config:
      status_code: 502
      body_template: '{"error": "{{error.code}}", "message": "{{error.message}}"}'
```

## 5. The `auth-gate` supernode

Replaces APISIX `plugin_config 1`'s `cors` + `openid-connect` pair. Exposes three
instance ports: `success` (authenticated, continue), `exit` (a response is already
prepared — CORS preflight, OIDC denial, or an OIDC browser redirect), and `error`
(the default boundary, carrying `OIDC_PROVIDER_ERROR`).

```yaml
supernodes:
  - name: auth-gate
    description: Permissive CORS + Keycloak interactive OIDC login (APISIX plugin_config 1)
    nodes:
      - { id: input,  type: input }
      - { id: output, type: output }   # -> instance port `success`
      - { id: exit,   type: output }   # -> instance port `exit`
      - { id: error,  type: error }    # -> instance port `error` (default boundary)
      - id: cors
        type: cors
        config:
          allowed_origins: ["*"]
          allowed_headers: ["*"]
          allowed_methods: [GET, POST, PUT, DELETE, PATCH, HEAD, OPTIONS]
      - id: oidc
        type: openid-connect
        config:
          bearer_only: false
          client_id: ${CLIENT_ID}
          client_secret: ${CLIENT_SECRET}
          discovery: https://${KEYCLOAK_HOST}:${KEYCLOAK_PORT}/realms/${REALM_NAME}/.well-known/openid-configuration
          ssl_verify: false
          scope: openid
          redirect_uri: ${DEPLOYMENT_BASE_URL}${OIDC_REDIRECT_PATH:-/.featherbit/redirect}
          post_logout_redirect_uri: ${DEPLOYMENT_BASE_URL}/public/landing
          logout_path: /logout
          access_token_in_authorization_header: true
          session:
            secret: ${SESSION_SECRET}
            cookie:
              lifetime: 1800
    edges:
      - { from: input.out,      to: cors.in }
      - { from: cors.success,   to: oidc.in }
      - { from: cors.preflight, to: exit.in }
      - { from: oidc.success,   to: output.in }
      - { from: oidc.denied,    to: exit.in }
      - { from: oidc.redirect,  to: exit.in }
      # oidc.error is left unwired -> default `error` boundary (black-box rule)
```

Notes:

- `allowed_methods` must be an explicit list; featherbit's `cors` does not accept `"*"` there.
- `session.storage` is left at the default `cookie`, matching APISIX. No `stores:` entry is
  needed. Consequence: sessions are **not revocable** and tokens are **not refreshed**;
  they expire after the 1800s cookie lifetime.
- `ssl_verify: false` mirrors the APISIX comment about Keycloak's internal self-signed cert.

## 6. Policies

Every policy carries the same tail and the same catch-all (`esra-public` and `epg-stream`
each insert one extra response-phase node *after* `headers`, before `client`):

```yaml
  - id: headers
    type: response-rewrite
    config_ref: security-headers
  - id: errors
    type: error-handler
    config_ref: gateway-error
# ...
error_handler: errors
# edges:  errors.success -> headers.in ;  headers.success -> client.in
```

Unwired `error` ports fall through to `error_handler: errors`, so upstream timeouts and
Keycloak outages produce the uniform 502 with the global security headers attached.

### 6.1 `login-redirect`

`/login` → `302` to `/`. (APISIX `plugin_config 1`'s `redirect.regex_uri`.)

```
listener → go(redirect uri:/ ret_code:302) ─redirect→ headers → client
                                           ─success──→ headers → client   # never taken; mandatory port
```

### 6.2 `esra-public`

Unauthenticated frontend assets. `Clear-Site-Data` is applied only on `/public/landing`,
using `response-rewrite`'s `vars` gate.

```
listener → esra → headers → landing-csd → client
```

```yaml
  - id: esra
    type: upstream
    config:
      targets: [ { host: "${ESRA_HOST:-esra}", port: "${ESRA_PORT:-55555}" } ]
      timeout_ms: ${TIMEOUT_MS:-60000}
  - id: landing-csd
    type: response-rewrite
    config:
      vars: [["uri", "==", "/public/landing"]]
      headers:
        set:
          Clear-Site-Data: '"cache","cookies","storage"'
```

### 6.3 `esra-auth`

```
listener → gate(auth-gate) ─success→ esra → headers → client
                           ─exit───────────→ headers → client
                           ─error───────────→ errors → headers → client
```

Also serves the OIDC callback, because the catch-all route `/*` maps here.

### 6.4 `epg-auth`

Same shape, upstream `${EPG_HOST:-epg}:${EPG_PORT:-8080}`. WebSocket upgrades on `/api/*`
need no configuration — featherbit negotiates them at the server layer.

### 6.5 `epg-stream`

SSE routes. Long read deadline and `X-Accel-Buffering: no` on both request and response.

```
listener → gate ─success→ accel-req → epg-stream → headers → accel-resp → client
```

```yaml
  - id: accel-req
    type: proxy-rewrite
    config:
      phase: request
      add_headers: { X-Accel-Buffering: "no" }
  - id: epg-stream
    type: upstream
    config:
      targets: [ { host: "${EPG_HOST:-epg}", port: "${EPG_PORT:-8080}" } ]
      timeout_ms: 3600000
  - id: accel-resp
    type: response-rewrite
    config:
      headers:
        set: { X-Accel-Buffering: "no" }
```

### 6.6 `thot-auth`

Same as `epg-auth`, upstream `${THOT_HOST:-thot}:${THOT_PORT:-8080}`.

### 6.7 `keycloak-admin`

Authenticated Keycloak Admin API proxy. `/auth/users/1` → `/admin/realms/$REALM/users/1`.

```
listener → gate ─success→ admin-rewrite → keycloak → headers → client
```

```yaml
  - id: admin-rewrite
    type: proxy-rewrite
    config:
      phase: request
      strip_path_prefix: /auth
      add_path_prefix: /admin/realms/${REALM_NAME}
  - id: keycloak
    type: upstream
    config:
      targets: [ { host: "${KEYCLOAK_HOST}", port: "${KEYCLOAK_PORT}" } ]
      tls: true
      ssl_verify: false
```

### 6.8 `keycloak-public`

Unauthenticated Keycloak passthrough with the admin path blocked. `/auth/foo` → `/foo`.

```
listener → blocker ─success→ public-rewrite → keycloak → headers → client
                   ─denied──────────────────────────────→ headers → client
```

```yaml
  - id: blocker
    type: uri-blocker
    config:
      block_rules: ["admin/"]
      rejected_code: 404
  - id: public-rewrite
    type: proxy-rewrite
    config:
      phase: request
      strip_path_prefix: /auth
```

## 7. Routes

featherbit is **first match wins in declaration order**, unlike APISIX's radix tree
(most-specific wins regardless of order). The order below is therefore load-bearing:
adding a route later is not order-neutral.

| # | path | methods | policy |
|---|---|---|---|
| 1 | `/login` | any | `login-redirect` |
| 2 | `/public/*` | any | `esra-public` |
| 3 | `/service-worker.js` | any | `esra-public` |
| 4 | `/manifest.webmanifest` | any | `esra-public` |
| 5 | `/images/*` | any | `esra-public` |
| 6 | `/assets/*` | any | `esra-public` |
| 7 | `/favicon.ico` | any | `esra-public` |
| 8 | `/api/v2/risk/status-stream` | any | `epg-stream` |
| 9 | `/api/v2/builder/status-stream` | any | `epg-stream` |
| 10 | `/api/*` | any | `epg-auth` |
| 11 | `/auth/users` | any | `keycloak-admin` |
| 12 | `/auth/users/*` | any | `keycloak-admin` |
| 13 | `/auth/groups` | GET | `keycloak-admin` |
| 14 | `/auth/groups/*` | GET | `keycloak-admin` |
| 15 | `/auth/resources/*` | any | `keycloak-public` |
| 16 | `/auth/*` | any | `keycloak-public` |
| 17 | `/_api/*` | any | `esra-auth` |
| 18 | `/swagger/*` | any | `epg-auth` |
| 19 | `/thot/api/*` | any | `thot-auth` |
| 20 | `/thot/swagger/*` | any | `thot-auth` |
| 21 | `/*` | any | `esra-auth` |

Non-GET `/auth/groups` falls through to route 16 and is proxied unauthenticated to
Keycloak's public endpoint — the same outcome APISIX produces.

## 8. Environment variables

| Var | Purpose | Default |
|---|---|---|
| `ESRA_HOST` / `ESRA_PORT` | frontend upstream | `esra` / `55555` |
| `EPG_HOST` / `EPG_PORT` | API upstream | `epg` / `8080` |
| `THOT_HOST` / `THOT_PORT` | THOT upstream | `thot` / `8080` |
| `KEYCLOAK_HOST` / `KEYCLOAK_PORT` | Keycloak | — (required) |
| `REALM_NAME` | Keycloak realm | — (required) |
| `CLIENT_ID` / `CLIENT_SECRET` | OIDC client | — (required) |
| `SESSION_SECRET` | session cookie seal; identical on every instance | — (required) |
| `DEPLOYMENT_BASE_URL` | public base URL for redirect/logout URIs | — (required) |
| `OIDC_REDIRECT_PATH` | OIDC callback path | `/.featherbit/redirect` |
| `TIMEOUT_MS` | upstream deadline, **milliseconds** | `60000` |

`TIMEOUT_MS` is deliberately a new name: APISIX's `${{TIMEOUT}}` is in seconds, featherbit's
`timeout_ms` is in milliseconds. Reusing the old variable would silently shorten every
timeout by a factor of 1000.

**Open risk — `${ENV}` in numeric fields.** `port` and `timeout_ms` are integer-typed.
`gateway.yaml` stores placeholders raw and resolves them at graph-compile time, but whether
a resolved string coerces into an integer field has not been confirmed on this build. The
first implementation step is to prove it with `validate_policy`; if it is rejected, ports and
timeouts fall back to literal values and only the string-typed fields (hosts, URLs, secrets,
realm) keep their placeholders.

## 9. Known differences from the APISIX deployment

1. **No bearer-token path.** APISIX's `openid-connect` with `bearer_only: false` also
   validates an `Authorization: Bearer` token. featherbit's interactive mode does not —
   a token-bearing API client is redirected to Keycloak instead of being authenticated.
   Closing this would need a `condition` node splitting into a second, bearer-mode
   `openid-connect` node.
2. **No elasticsearch-logger.** Access logs are not shipped anywhere.
3. **`/assets/*.js` + `/assets/*.css` collapse to `/assets/*`.** featherbit has no suffix
   wildcard. Both went to `esra` anyway; the difference is only that other extensions
   under `/assets/` now match too.
4. **Route precedence is declaration order**, not specificity (§7).
5. **No `SWAGGER_ENABLED` / `DISABLE_MONITORING` toggles.**
6. **Sessions are cookie-sealed and unrevocable**, and tokens are not refreshed — the same
   as the APISIX config, but featherbit could lift both by declaring a redis `store` and
   switching `session.storage: redis`.
7. **`introspection_endpoint` is unused.** featherbit's interactive mode validates the
   `id_token` against JWKS from `discovery`; APISIX's `introspection_endpoint` and
   `introspection_endpoint_auth_method` have no equivalent in that mode.
8. **OIDC callback path changed** to `/.featherbit/redirect` — the Keycloak client's
   Valid Redirect URIs must be updated before login works.

## 10. Verification

Against the running instance, after applying:

1. `validate_policy` on each of the 8 policies, then `put_*` with `dry_run: true`, then commit.
2. `GET /public/landing` — 200, security headers present, `Clear-Site-Data` present.
3. `GET /public/other` — 200, security headers present, `Clear-Site-Data` **absent**.
4. `GET /favicon.ico` — 200, no redirect to Keycloak.
5. `GET /api/whatever` with no cookie — `302` to the Keycloak authorization endpoint.
6. `OPTIONS /api/whatever` with an `Origin` — `204` with the CORS headers, no redirect.
7. `GET /auth/admin/foo` — `404` from `uri-blocker`, not proxied.
8. `GET /auth/realms/...` — proxied to Keycloak with `/auth` stripped.
9. `GET /login` — `302` to `/`.
10. With Keycloak unreachable: `GET /api/whatever` — `502` `OIDC_PROVIDER_ERROR`, security
    headers attached. (Confirms `error_handler` wiring, not a login loop.)
11. `export_config` and diff against this document.

The debug panel's per-request traces are on for this instance, so each of the above can be
confirmed node-by-node via `list_traces` / `get_trace` rather than by header inspection alone.
