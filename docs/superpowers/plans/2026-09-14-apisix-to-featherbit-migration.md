# APISIX → featherbit Migration Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Stand up the ESRA/EPG/THOT/Keycloak routing, security-header, and Keycloak-SSO behavior of an existing Apache APISIX deployment on the `featherbit-45` gateway instance, applied live through the Admin MCP and exported as a `gateway.yaml`.

**Architecture:** Two shared `plugin_configs` carry the global security headers and a uniform 502 error body. One `auth-gate` supernode holds the CORS + interactive-OIDC pipeline that APISIX expressed as `plugin_config 1`, exposing `success` / `exit` / `error` instance ports. Eight small policies bind that gate (or not) to one of four upstreams, and 21 ordered routes select among them.

**Tech Stack:** featherbit 0.8.0 Admin MCP (`mcp__featherbit-45__*`); YAML policy graphs; Keycloak as the OIDC provider.

**Spec:** `docs/superpowers/specs/2026-09-14-apisix-to-featherbit-migration-design.md`

## Global Constraints

- **Target instance:** `featherbit-45`, featherbit **0.8.0**, debug + sandbox enabled. All writes go through `mcp__featherbit-45__*`. There is no local build to compile or test against.
- **Never call `reload_config`.** It re-reads `gateway.yaml` from disk and discards every live edit made in this plan.
- **Route order is creation order.** `POST /api/routes` appends (`src/admin/routes.rs:71`); `PUT /api/routes/{name}` replaces in place (`src/admin/routes.rs:105`). Create the 21 routes in the exact §7 order and never delete-then-recreate a middle route — that moves it to the end and silently changes matching.
- **Every `success` and outcome port must be wired** or policy compilation fails. Only `error` ports may be left unwired; they fall through to the policy's `error_handler`.
- **Authoring loop for every graph object:** `validate_*` → `put_*` with `dry_run: true` → `put_*` live → read it back.
- **Secrets stay as `${ENV}` placeholders.** Never substitute a real client secret or session secret into a config body. featherbit stores placeholders raw and resolves them at graph-compile time.
- **Env var names (exact):** `ESRA_HOST`, `ESRA_PORT`, `EPG_HOST`, `EPG_PORT`, `THOT_HOST`, `THOT_PORT`, `KEYCLOAK_HOST`, `KEYCLOAK_PORT`, `REALM_NAME`, `CLIENT_ID`, `CLIENT_SECRET`, `SESSION_SECRET`, `DEPLOYMENT_BASE_URL`, `OIDC_REDIRECT_PATH`, `TIMEOUT_MS`.
- **`TIMEOUT_MS` is milliseconds**, unlike APISIX's seconds-valued `TIMEOUT`. Default `60000`.
- **Do not `git commit` anything** until the operator gives an explicit go-ahead (standing rule). Task 11 is gated on that.

---

### Task 1: Prove `${ENV}` coercion into integer-typed fields

Resolves the open risk recorded in spec §8. `port` and `timeout_ms` are integer-typed; every later task's upstream YAML depends on whether a placeholder survives into them. `validate_policy` compiles without committing, so this task writes nothing to the instance.

**Objects:**
- Validate-only: a throwaway policy named `envprobe` (never `put_*`)

**Interfaces:**
- Consumes: nothing
- Produces: a decision recorded in this file — **PLACEHOLDER-OK** or **LITERALS-REQUIRED** — consumed by Tasks 4, 5, 6 and 7 for their `port` and `timeout_ms` values.

- [ ] **Step 1: Validate a policy using placeholders in both integer fields**

Call `validate_policy` with:

```yaml
name: envprobe
error_handler: errors
nodes:
  - { id: listener, type: listener, config: {} }
  - id: probe
    type: upstream
    config:
      targets:
        - { host: "${PROBE_HOST:-example.internal}", port: "${PROBE_PORT:-8080}" }
      timeout_ms: "${PROBE_TIMEOUT_MS:-60000}"
  - id: errors
    type: error-handler
    config:
      status_code: 502
      body_template: '{"error": "{{error.code}}", "message": "{{error.message}}"}'
  - { id: client, type: client, config: {} }
edges:
  - { from: listener.out, to: probe.in }
  - { from: probe.success, to: client.in }
  - { from: probe.error, to: errors.in }
  - { from: errors.success, to: client.in }
```

Expected: **either** a clean compile **or** a deserialization error naming `port` / `timeout_ms`. Both are informative; neither is a failure of this task.

- [ ] **Step 2: If Step 1 failed, validate the same policy with the integers as literals**

Replace only the two integer fields, leaving `host` as a placeholder:

```yaml
  - id: probe
    type: upstream
    config:
      targets:
        - { host: "${PROBE_HOST:-example.internal}", port: 8080 }
      timeout_ms: 60000
```

Expected: PASS. If this also fails, stop and report — the failure is not about env coercion and the rest of the plan rests on a wrong assumption.

- [ ] **Step 3: Record the decision**

Edit this plan file, replacing the line below with the outcome:

> **DECISION (Task 1): _unrecorded_**

Write either `PLACEHOLDER-OK — use "${ESRA_PORT:-55555}" and "${TIMEOUT_MS:-60000}" as written in the spec` or `LITERALS-REQUIRED — ports and timeout_ms are literal integers; only host/URL/secret/realm fields keep placeholders`.

- [ ] **Step 4: Confirm nothing was written**

Call `get_status`. Expected: still `policies: 2, routes: 1, supernodes: 0` — `validate_policy` must not have committed `envprobe`.

---

### Task 2: Shared plugin configs

**Objects:**
- Create: plugin config `security-headers` (type `response-rewrite`)
- Create: plugin config `gateway-error` (type `error-handler`)

**Interfaces:**
- Consumes: nothing
- Produces: the names `security-headers` and `gateway-error`, referenced as `config_ref` by a `headers` node and an `errors` node in all eight policies (Tasks 4–7).

- [ ] **Step 1: Dry-run the `security-headers` config**

`put_plugin_config` with `dry_run: true`:

```yaml
name: security-headers
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
```

Expected: PASS. A failure here most likely means `headers.set` rejected a value — `response-rewrite` requires the `headers: {set/add/remove}` shape, not `add_headers`.

- [ ] **Step 2: Commit `security-headers`**

Repeat the same call without `dry_run`. Expected: created.

- [ ] **Step 3: Dry-run and commit `gateway-error`**

`put_plugin_config` with `dry_run: true`, then live:

```yaml
name: gateway-error
type: error-handler
description: Uniform 502 for upstream and IdP failures
config:
  status_code: 502
  body_template: '{"error": "{{error.code}}", "message": "{{error.message}}"}'
```

Expected: PASS, then created.

- [ ] **Step 4: Read both back**

Call `list_plugin_configs`. Expected: exactly two entries, `security-headers` (type `response-rewrite`) and `gateway-error` (type `error-handler`), with the `Permissions-Policy` string intact and unescaped.

---

### Task 3: The `auth-gate` supernode

Replaces APISIX `plugin_config 1`. Three instance ports: `success`, `exit`, `error`.

**Objects:**
- Create: supernode `auth-gate`

**Interfaces:**
- Consumes: nothing (its inner nodes carry inline config, not `config_ref`)
- Produces: instance ports `gate.success`, `gate.exit`, `gate.error`, used by the five protected policies in Tasks 5–7. `success` and `exit` are **mandatory-wired**; `error` is optional.

- [ ] **Step 1: Validate the definition**

Call `validate_supernode` with:

```yaml
name: auth-gate
description: Permissive CORS + Keycloak interactive OIDC login (APISIX plugin_config 1)
nodes:
  - { id: input,  type: input }
  - { id: output, type: output }
  - { id: exit,   type: output }
  - { id: error,  type: error }
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
```

Expected: PASS. Two likely failures and their fixes:
- `allowed_methods` rejecting `"*"` → it is already an explicit list here; if it still fails, the key name is wrong (it is `allowed_methods`, not `allow_methods`).
- `session.cookie.path` must cover the `redirect_uri` path. Default path is `/`, and the callback is `/.featherbit/redirect`, so `/` covers it. Do **not** add a narrower `session.cookie.path`.

- [ ] **Step 2: Dry-run, then commit**

`put_supernode` with `dry_run: true`, expected PASS; then the same call live, expected created.

- [ ] **Step 3: Read it back and confirm the port names**

Call `get_supernode` with `name: auth-gate`. Expected: two output boundaries (`output`, `exit`) and one error boundary (`error`). Confirm `oidc.error` has **no** outgoing edge — it must reach the default `error` boundary by the black-box rule, not by an explicit edge.

---

### Task 4: The two unauthenticated policies

**Objects:**
- Create: policy `login-redirect`
- Create: policy `esra-public`

**Interfaces:**
- Consumes: `config_ref` names `security-headers` and `gateway-error` from Task 2; the Task 1 decision for `port` / `timeout_ms`.
- Produces: policy names `login-redirect` and `esra-public`, referenced by routes 1–7 in Task 9.

- [ ] **Step 1: Validate `login-redirect`**

Call `validate_policy`:

```yaml
name: login-redirect
error_handler: errors
nodes:
  - { id: listener, type: listener, config: {} }
  - id: go
    type: redirect
    config:
      uri: /
      ret_code: 302
  - { id: headers, type: response-rewrite, config_ref: security-headers, config: {} }
  - { id: errors,  type: error-handler,    config_ref: gateway-error,    config: {} }
  - { id: client,  type: client, config: {} }
edges:
  - { from: listener.out,     to: go.in }
  - { from: go.redirect,      to: headers.in }
  - { from: go.success,       to: headers.in }
  - { from: go.error,         to: errors.in }
  - { from: errors.success,   to: headers.in }
  - { from: headers.success,  to: client.in }
```

Expected: PASS. `go.success` is never taken at runtime (a `uri`-mode redirect always exits on `redirect`) but is a mandatory port, so it must be wired. `go.error` is wired explicitly even though `redirect` never fails, so that `errors` has an incoming edge — the instance's own `echo-policy` wires its error node the same way, and an orphan node may not compile. Multiple edges fan into `headers.in`; if validation rejects that fan-in, report it before continuing — every remaining policy depends on it.

- [ ] **Step 2: Dry-run and commit `login-redirect`**

`put_policy` with `dry_run: true`, then live. Expected: PASS, then created.

- [ ] **Step 3: Validate `esra-public`**

Use the Task 1 decision for `port` and `timeout_ms` (shown here in PLACEHOLDER-OK form):

```yaml
name: esra-public
error_handler: errors
nodes:
  - { id: listener, type: listener, config: {} }
  - id: esra
    type: upstream
    config:
      targets:
        - { host: "${ESRA_HOST:-esra}", port: "${ESRA_PORT:-55555}" }
      timeout_ms: "${TIMEOUT_MS:-60000}"
  - { id: headers, type: response-rewrite, config_ref: security-headers, config: {} }
  - id: landing-csd
    type: response-rewrite
    config:
      vars: [["uri", "==", "/public/landing"]]
      headers:
        set:
          Clear-Site-Data: '"cache","cookies","storage"'
  - { id: errors, type: error-handler, config_ref: gateway-error, config: {} }
  - { id: client, type: client, config: {} }
edges:
  - { from: listener.out,        to: esra.in }
  - { from: esra.success,        to: headers.in }
  - { from: esra.error,          to: errors.in }
  - { from: errors.success,      to: headers.in }
  - { from: headers.success,     to: landing-csd.in }
  - { from: landing-csd.success, to: client.in }
```

Expected: PASS. Note `landing-csd` sits **after** `headers`, so the `Clear-Site-Data` header is added on top of the global set.

- [ ] **Step 4: Dry-run and commit `esra-public`**

`put_policy` with `dry_run: true`, then live. Expected: PASS, then created.

- [ ] **Step 5: Exercise the `vars` gate in the sandbox**

Call `run_sandbox` against `esra-public` twice — once with request path `/public/landing`, once with `/public/other`. Expected: the `landing-csd` node reports the `Clear-Site-Data` header set on the first and reports a pure passthrough on the second. If the upstream is unreachable from the sandbox, read the per-node trace instead of the final response.

---

### Task 5: The three plain authenticated policies

`esra-auth`, `epg-auth`, `thot-auth` share one shape: `listener → gate → upstream → headers → client`.

**Objects:**
- Create: policies `esra-auth`, `epg-auth`, `thot-auth`

**Interfaces:**
- Consumes: supernode `auth-gate` (Task 3); `config_ref` names from Task 2; the Task 1 integer decision.
- Produces: policy names `esra-auth`, `epg-auth`, `thot-auth`, referenced by routes 10, 17, 18, 19, 20, 21 in Task 9.

- [ ] **Step 1: Validate `esra-auth`**

```yaml
name: esra-auth
error_handler: errors
nodes:
  - { id: listener, type: listener, config: {} }
  - { id: gate, type: supernode, config: { name: auth-gate } }
  - id: esra
    type: upstream
    config:
      targets:
        - { host: "${ESRA_HOST:-esra}", port: "${ESRA_PORT:-55555}" }
      timeout_ms: "${TIMEOUT_MS:-60000}"
  - { id: headers, type: response-rewrite, config_ref: security-headers, config: {} }
  - { id: errors,  type: error-handler,    config_ref: gateway-error,    config: {} }
  - { id: client,  type: client, config: {} }
edges:
  - { from: listener.out,    to: gate.in }
  - { from: gate.success,    to: esra.in }
  - { from: gate.exit,       to: headers.in }
  - { from: gate.error,      to: errors.in }
  - { from: esra.success,    to: headers.in }
  - { from: esra.error,      to: errors.in }
  - { from: errors.success,  to: headers.in }
  - { from: headers.success, to: client.in }
```

Expected: PASS. If it fails with `output port 'exit' of supernode instance 'gate' must be wired`, the supernode's second output boundary was not created — go back to Task 3 Step 3.

- [ ] **Step 2: Dry-run and commit `esra-auth`**

`put_policy` with `dry_run: true`, then live. Expected: PASS, then created.

- [ ] **Step 3: Validate and commit `epg-auth`**

Identical to Step 1 with the node id `esra` renamed to `epg` and the upstream retargeted. Full body:

```yaml
name: epg-auth
error_handler: errors
nodes:
  - { id: listener, type: listener, config: {} }
  - { id: gate, type: supernode, config: { name: auth-gate } }
  - id: epg
    type: upstream
    config:
      targets:
        - { host: "${EPG_HOST:-epg}", port: "${EPG_PORT:-8080}" }
      timeout_ms: "${TIMEOUT_MS:-60000}"
  - { id: headers, type: response-rewrite, config_ref: security-headers, config: {} }
  - { id: errors,  type: error-handler,    config_ref: gateway-error,    config: {} }
  - { id: client,  type: client, config: {} }
edges:
  - { from: listener.out,    to: gate.in }
  - { from: gate.success,    to: epg.in }
  - { from: gate.exit,       to: headers.in }
  - { from: gate.error,      to: errors.in }
  - { from: epg.success,     to: headers.in }
  - { from: epg.error,       to: errors.in }
  - { from: errors.success,  to: headers.in }
  - { from: headers.success, to: client.in }
```

`validate_policy`, then `put_policy` dry-run, then live. Expected: PASS, PASS, created. WebSocket upgrades on `/api/*` need no configuration — featherbit negotiates them at the server layer.

- [ ] **Step 4: Validate and commit `thot-auth`**

```yaml
name: thot-auth
error_handler: errors
nodes:
  - { id: listener, type: listener, config: {} }
  - { id: gate, type: supernode, config: { name: auth-gate } }
  - id: thot
    type: upstream
    config:
      targets:
        - { host: "${THOT_HOST:-thot}", port: "${THOT_PORT:-8080}" }
      timeout_ms: "${TIMEOUT_MS:-60000}"
  - { id: headers, type: response-rewrite, config_ref: security-headers, config: {} }
  - { id: errors,  type: error-handler,    config_ref: gateway-error,    config: {} }
  - { id: client,  type: client, config: {} }
edges:
  - { from: listener.out,    to: gate.in }
  - { from: gate.success,    to: thot.in }
  - { from: gate.exit,       to: headers.in }
  - { from: gate.error,      to: errors.in }
  - { from: thot.success,    to: headers.in }
  - { from: thot.error,      to: errors.in }
  - { from: errors.success,  to: headers.in }
  - { from: headers.success, to: client.in }
```

`validate_policy`, then `put_policy` dry-run, then live. Expected: PASS, PASS, created.

- [ ] **Step 5: Confirm all three compiled**

Call `list_policies`. Expected: `login-redirect`, `esra-public`, `esra-auth`, `epg-auth`, `thot-auth` present alongside the two pre-existing policies (still there until Task 8).

---

### Task 6: The SSE streaming policy

`epg-stream` adds `X-Accel-Buffering: no` on both request and response and raises the upstream deadline to one hour.

**Objects:**
- Create: policy `epg-stream`

**Interfaces:**
- Consumes: supernode `auth-gate`; `config_ref` names from Task 2.
- Produces: policy name `epg-stream`, referenced by routes 8 and 9 in Task 9.

- [ ] **Step 1: Validate `epg-stream`**

```yaml
name: epg-stream
error_handler: errors
nodes:
  - { id: listener, type: listener, config: {} }
  - { id: gate, type: supernode, config: { name: auth-gate } }
  - id: accel-req
    type: proxy-rewrite
    config:
      phase: request
      add_headers:
        X-Accel-Buffering: "no"
  - id: epg
    type: upstream
    config:
      targets:
        - { host: "${EPG_HOST:-epg}", port: "${EPG_PORT:-8080}" }
      timeout_ms: 3600000
  - { id: headers, type: response-rewrite, config_ref: security-headers, config: {} }
  - id: accel-resp
    type: response-rewrite
    config:
      headers:
        set:
          X-Accel-Buffering: "no"
  - { id: errors, type: error-handler, config_ref: gateway-error, config: {} }
  - { id: client, type: client, config: {} }
edges:
  - { from: listener.out,       to: gate.in }
  - { from: gate.success,       to: accel-req.in }
  - { from: gate.exit,          to: headers.in }
  - { from: gate.error,         to: errors.in }
  - { from: accel-req.success,  to: epg.in }
  - { from: epg.success,        to: headers.in }
  - { from: epg.error,          to: errors.in }
  - { from: errors.success,     to: headers.in }
  - { from: headers.success,    to: accel-resp.in }
  - { from: accel-resp.success, to: client.in }
```

Expected: PASS. `timeout_ms: 3600000` is a literal regardless of the Task 1 decision — the spec pins this upstream to one hour, not to `TIMEOUT_MS`. Note `accel-req` uses `proxy-rewrite`'s `add_headers` (request side) while `accel-resp` uses `response-rewrite`'s `headers.set` — the two plugins take different key shapes and each rejects the other's.

- [ ] **Step 2: Dry-run and commit**

`put_policy` with `dry_run: true`, then live. Expected: PASS, then created.

- [ ] **Step 3: Confirm the header shapes survived the round trip**

Call `get_policy` with `name: epg-stream`. Expected: `accel-req.config.add_headers` is a map containing `X-Accel-Buffering: "no"`, and `accel-resp.config.headers.set` contains the same pair. If either came back empty, the wrong key shape was silently accepted — fix before proceeding.

---

### Task 7: The two Keycloak policies

**Objects:**
- Create: policy `keycloak-admin`
- Create: policy `keycloak-public`

**Interfaces:**
- Consumes: supernode `auth-gate`; `config_ref` names from Task 2.
- Produces: policy names `keycloak-admin` and `keycloak-public`, referenced by routes 11–16 in Task 9.

- [ ] **Step 1: Validate `keycloak-admin`**

Replaces APISIX's `regex_uri` pairs: `/auth/users/1` → strip `/auth` → `/users/1` → add `/admin/realms/$REALM` → `/admin/realms/$REALM/users/1`.

```yaml
name: keycloak-admin
error_handler: errors
nodes:
  - { id: listener, type: listener, config: {} }
  - { id: gate, type: supernode, config: { name: auth-gate } }
  - id: admin-rewrite
    type: proxy-rewrite
    config:
      phase: request
      strip_path_prefix: /auth
      add_path_prefix: /admin/realms/${REALM_NAME}
  - id: keycloak
    type: upstream
    config:
      targets:
        - { host: "${KEYCLOAK_HOST}", port: "${KEYCLOAK_PORT}" }
      tls: true
      ssl_verify: false
  - { id: headers, type: response-rewrite, config_ref: security-headers, config: {} }
  - { id: errors,  type: error-handler,    config_ref: gateway-error,    config: {} }
  - { id: client,  type: client, config: {} }
edges:
  - { from: listener.out,          to: gate.in }
  - { from: gate.success,          to: admin-rewrite.in }
  - { from: gate.exit,             to: headers.in }
  - { from: gate.error,            to: errors.in }
  - { from: admin-rewrite.success, to: keycloak.in }
  - { from: keycloak.success,      to: headers.in }
  - { from: keycloak.error,        to: errors.in }
  - { from: errors.success,        to: headers.in }
  - { from: headers.success,       to: client.in }
```

Expected: PASS. `ssl_verify: false` is required — Keycloak is reached over the internal network with a self-signed certificate.

- [ ] **Step 2: Dry-run and commit `keycloak-admin`**

`put_policy` with `dry_run: true`, then live. Expected: PASS, then created.

- [ ] **Step 3: Verify the path rewrite in the sandbox**

Call `run_sandbox` against `keycloak-admin` with request path `/auth/users/abc-123`. Expected: after `admin-rewrite`, the trace shows `context.request.path` = `/admin/realms/<resolved REALM_NAME>/users/abc-123`. Also run it with path `/auth/users`. Expected: `/admin/realms/<realm>/users` — stripping `/auth` from `/auth/users` must yield `/users`, not `/`.

- [ ] **Step 4: Validate `keycloak-public`**

```yaml
name: keycloak-public
error_handler: errors
nodes:
  - { id: listener, type: listener, config: {} }
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
  - id: keycloak
    type: upstream
    config:
      targets:
        - { host: "${KEYCLOAK_HOST}", port: "${KEYCLOAK_PORT}" }
      tls: true
      ssl_verify: false
  - { id: headers, type: response-rewrite, config_ref: security-headers, config: {} }
  - { id: errors,  type: error-handler,    config_ref: gateway-error,    config: {} }
  - { id: client,  type: client, config: {} }
edges:
  - { from: listener.out,           to: blocker.in }
  - { from: blocker.success,        to: public-rewrite.in }
  - { from: blocker.denied,         to: headers.in }
  - { from: public-rewrite.success, to: keycloak.in }
  - { from: keycloak.success,       to: headers.in }
  - { from: keycloak.error,         to: errors.in }
  - { from: errors.success,         to: headers.in }
  - { from: headers.success,        to: client.in }
```

Expected: PASS. There is deliberately no `gate` here — this is APISIX's `plugin_config 3`, the unauthenticated Keycloak passthrough.

- [ ] **Step 5: Dry-run and commit `keycloak-public`**

`put_policy` with `dry_run: true`, then live. Expected: PASS, then created.

- [ ] **Step 6: Verify the blocker in the sandbox**

Call `run_sandbox` against `keycloak-public` with path `/auth/admin/serverinfo`. Expected: `blocker` exits on `denied` with status `404`, and the `keycloak` node never runs. Then with path `/auth/realms/x/protocol/openid-connect/certs`. Expected: `blocker` exits on `success` and `public-rewrite` yields `/realms/x/protocol/openid-connect/certs`.

---

### Task 8: Remove the pre-existing instance config

The `public-fe` route is declared first and would shadow the new `/public/*` route. `public-fe-policy` cannot be deleted while that route references it, so the route goes first.

**Objects:**
- Delete: route `public-fe`
- Delete: policy `public-fe-policy`
- Delete: policy `echo-policy`

**Interfaces:**
- Consumes: nothing
- Produces: an empty `routes:` list, so Task 9's creations land in a known order starting from index 0.

- [ ] **Step 1: Delete the `public-fe` route**

Call `delete_route` with `name: public-fe`. Expected: deleted.

- [ ] **Step 2: Delete both leftover policies**

Call `delete_policy` for `public-fe-policy`, then for `echo-policy`. Expected: deleted, deleted. A `400` on either means something still references it — call `list_routes` and remove the referencing route first.

- [ ] **Step 3: Confirm a clean slate**

Call `get_status`. Expected: `routes: 0`, `policies: 8`, `supernodes: 1`. If `policies` is not exactly 8, list them and reconcile against Tasks 4–7 before creating any route.

---

### Task 9: Create the 21 routes, in order

**Order is the deliverable of this task.** Create them in exactly the sequence below, one `put_route` call each, and do not delete or re-create any of them afterwards — a re-created route moves to the end of the list.

**Objects:**
- Create: 21 routes

**Interfaces:**
- Consumes: all eight policy names from Tasks 4–7.
- Produces: a live route table; nothing later depends on it except verification.

- [ ] **Step 1: Create routes 1–7 (login + unauthenticated frontend)**

Call `put_route` once per row, in this order. Every route omits `methods` (matching any method) unless stated.

| # | name | path | policy |
|---|---|---|---|
| 1 | `login` | `/login` | `login-redirect` |
| 2 | `public` | `/public/*` | `esra-public` |
| 3 | `service-worker` | `/service-worker.js` | `esra-public` |
| 4 | `manifest` | `/manifest.webmanifest` | `esra-public` |
| 5 | `images` | `/images/*` | `esra-public` |
| 6 | `assets` | `/assets/*` | `esra-public` |
| 7 | `favicon` | `/favicon.ico` | `esra-public` |

Shape of each call:

```yaml
name: public
match:
  path: /public/*
policy: esra-public
```

Expected: created, seven times.

- [ ] **Step 2: Create routes 8–10 (EPG, streams before the catch-all)**

| # | name | path | policy |
|---|---|---|---|
| 8 | `risk-status-stream` | `/api/v2/risk/status-stream` | `epg-stream` |
| 9 | `builder-status-stream` | `/api/v2/builder/status-stream` | `epg-stream` |
| 10 | `api` | `/api/*` | `epg-auth` |

Expected: created, three times. Routes 8 and 9 **must** precede 10 — `/api/*` would otherwise swallow both stream paths and they would lose the one-hour deadline and the `X-Accel-Buffering` headers.

- [ ] **Step 3: Create routes 11–16 (Keycloak, specific before general)**

| # | name | path | methods | policy |
|---|---|---|---|---|
| 11 | `auth-users` | `/auth/users` | — | `keycloak-admin` |
| 12 | `auth-users-sub` | `/auth/users/*` | — | `keycloak-admin` |
| 13 | `auth-groups` | `/auth/groups` | `[GET]` | `keycloak-admin` |
| 14 | `auth-groups-sub` | `/auth/groups/*` | `[GET]` | `keycloak-admin` |
| 15 | `auth-resources` | `/auth/resources/*` | — | `keycloak-public` |
| 16 | `auth-public` | `/auth/*` | — | `keycloak-public` |

Routes 13 and 14 carry a method filter:

```yaml
name: auth-groups
match:
  path: /auth/groups
  methods: [GET]
policy: keycloak-admin
```

Expected: created, six times. A non-GET `/auth/groups` deliberately falls through to route 16 and is proxied unauthenticated — the same outcome APISIX produces.

- [ ] **Step 4: Create routes 17–21 (remaining APIs and the catch-all)**

| # | name | path | policy |
|---|---|---|---|
| 17 | `esra-api` | `/_api/*` | `esra-auth` |
| 18 | `swagger` | `/swagger/*` | `epg-auth` |
| 19 | `thot-api` | `/thot/api/*` | `thot-auth` |
| 20 | `thot-swagger` | `/thot/swagger/*` | `thot-auth` |
| 21 | `catch-all` | `/*` | `esra-auth` |

Expected: created, five times. Route 21 must be last; it also serves the OIDC callback at `/.featherbit/redirect`.

- [ ] **Step 5: Verify the order**

Call `list_routes`. Expected: exactly 21 routes in the numbered order above. Check specifically that `risk-status-stream` and `builder-status-stream` precede `api`, that `auth-users*` and `auth-groups*` precede `auth-public`, and that `catch-all` is last. If any route is out of place, delete **every** route from that position onward and re-create them in order — do not try to fix one in place.

---

### Task 10: End-to-end verification and export

**Objects:**
- Read: traces via `list_traces` / `get_trace`
- Create: `dev/featherbit-45-gateway.yaml` (the exported config, for the operator to persist)

**Interfaces:**
- Consumes: the complete live config from Tasks 2–9.
- Produces: an exported `gateway.yaml` and a pass/fail report against spec §10.

- [ ] **Step 1: Walk the spec's verification list**

Run spec §10 items 2–10 against the instance. For each, read the per-node trace rather than trusting the final status alone — debug mode is on for this instance. Record actual vs expected for each of the nine checks.

Expected outcomes, restated so they need not be looked up:
- `GET /public/landing` → 200, security headers present, `Clear-Site-Data` present
- `GET /public/other` → 200, security headers present, `Clear-Site-Data` absent
- `GET /favicon.ico` → 200, no Keycloak redirect
- `GET /api/whatever` with no cookie → 302 to the Keycloak authorization endpoint
- `OPTIONS /api/whatever` with an `Origin` → 204 with CORS headers, no redirect
- `GET /auth/admin/foo` → 404 from `uri-blocker`, `keycloak` node never runs
- `GET /auth/realms/...` → proxied with `/auth` stripped
- `GET /login` → 302 to `/`
- Keycloak unreachable → 502 `OIDC_PROVIDER_ERROR` with security headers, not a login loop

- [ ] **Step 2: Export the config**

Call `export_config` and write the returned YAML verbatim to `dev/featherbit-45-gateway.yaml`.

- [ ] **Step 3: Diff the export against the spec**

Compare the exported YAML against spec §4–§7. Expected: two `plugin_configs`, one `supernode`, eight `policies`, 21 `routes` in the §7 order, and **every** `${ENV}` placeholder still unresolved — in particular `${CLIENT_SECRET}` and `${SESSION_SECRET}` must appear literally. If any secret came back resolved, stop and report it: that is a disclosure bug, not a config error.

- [ ] **Step 4: Report gaps**

Summarize for the operator: which of the nine checks passed, the required Keycloak client change (add `${DEPLOYMENT_BASE_URL}/.featherbit/redirect` to Valid Redirect URIs), the full env-var list from spec §8, and the eight known differences from spec §9.

---

### Task 11: Commit the spec, plan, and export — GATED

**Do not start this task without an explicit go-ahead from the operator.** Standing rule: commits wait for their word. The current branch is `feature/compose-examples`, which is unrelated to this work.

**Objects:**
- Create: branch `feature/apisix-migration-config` off `develop`
- Commit: the spec, this plan, and `dev/featherbit-45-gateway.yaml`

**Interfaces:**
- Consumes: the export from Task 10.
- Produces: a branch ready for a PR into `develop`.

- [ ] **Step 1: Ask for the go-ahead**

Confirm with the operator that they want these three files committed, and on which branch. Do not proceed on assumption.

- [ ] **Step 2: Branch off `develop`**

```bash
git checkout develop
git pull
git checkout -b feature/apisix-migration-config
```

- [ ] **Step 3: Commit**

```bash
git add docs/superpowers/specs/2026-09-14-apisix-to-featherbit-migration-design.md \
        docs/superpowers/plans/2026-09-14-apisix-to-featherbit-migration.md \
        dev/featherbit-45-gateway.yaml
git commit -m "docs(migration): apisix-to-featherbit config design, plan, and exported gateway.yaml"
```

No `Co-Authored-By` trailer and no Claude Code footer — standing rule.

---

## Decisions recorded during execution

> **DECISION (Task 1): _unrecorded_**
