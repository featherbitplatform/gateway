# featherbit — E2E Testbook

The scenario catalog for the end-to-end suite in `e2e/`. Every scenario below has
an ID that appears verbatim in the test title, so a CI failure names the row you
are reading.

## Running it locally

The suite runs the **release binary**, and the binary embeds `ui/dist`, so both
must be current. First time:

```bash
cargo build --release
cd e2e && npm run setup        # npm install + playwright install chromium
npm test                       # boots the gateway + echo backend itself
```

Day to day:

```bash
cd e2e
npm test                       # ~25s, headless
npm run test:headed            # watch it drive the browser
npm run test:all               # rebuild the UI + gateway first, then run
npm run report                 # open the HTML report of the last run
npx playwright test -g E2E-LOOP-01     # a single scenario, by its testbook ID
```

Reach for `test:all` after touching `ui/src` or Rust: `cargo build` embeds
`ui/dist` **as it finds it** and never rebuilds the frontend, so a plain `npm test`
can happily exercise a stale UI. Nothing needs to be running first, and no port of
yours is touched — the suite owns 18081 / 19091 / 3010.

In CI, `.github/workflows/ci.yml` runs `cargo test` + `clippy` and this suite on
every push and PR, and uploads the Playwright report (traces, screenshots) when it
fails.

## What this suite is for (and what it is not)

The 621 inline Rust tests already cover units and protocol behavior with **real
sockets** — TLS and mTLS handshakes, h2c, the WebSocket relay, L4 TCP/UDP. This
suite deliberately does **not** re-test those: Playwright cannot speak raw UDP or
present a client certificate, and duplicating that coverage would only produce a
slower second copy of it.

What nothing tested before this suite is the **assembled product**: the loop from
the browser, through the admin API, through validation and recompilation, into a
hot-swapped route table, and out into what the data plane actually does to live
traffic. That loop is the point of the gateway, and it spans three processes.
`E2E-LOOP-*` are the headline scenarios; everything else exists to make their
failures diagnosable.

Several scenarios are regression guards for bugs the suite itself found (July
2026): the admin plugin catalog advertising only 14 of 86 node types
(`E2E-API-04`, `E2E-UI-03`); CORS preflight never actually short-circuiting
(`E2E-DP-09`); and **openid-connect rejecting every JWKS-verified token under its
default config** (`E2E-OIDC-02`), because the default algorithm list mixed RSA and
EC families and `jsonwebtoken` rejects a mixed-family list outright — so the plugin
did not work out of the box at all.

## Fixture

One gateway process on **:18081** (admin **:19091**), one Python echo backend on
**:3010**, seeded from `e2e/fixtures/gateway.yaml`. Isolated ports so a run never
collides with a dev gateway on 8080/9090. `playwright.config.ts` stages the config
into a throwaway `e2e/.tmp/` before the gateway boots, and fails fast with a build
instruction if the release binary is missing.

Two facts about the file config store shape these tests, and both are easy to get
wrong:

- **Admin writes are in-memory only.** `FileConfigStore::commit` validates,
  recompiles and hot-swaps, but never rewrites `gateway.yaml` — a route created
  through the API is live immediately and gone on restart. Tests therefore assert
  against the API and live traffic, never against the file.
- **Touching `gateway.yaml` while the gateway runs wipes those writes.** The file
  watcher reloads from disk, and disk has no record of them.

Those two combine into a trap that cost two debugging rounds, which is why the
staging code is where it is and carries a comment:

- It **cannot** live in `globalSetup` — Playwright starts `webServer` *before*
  `globalSetup`, so on a clean checkout the gateway boots pointing at an
  `e2e/.tmp/` that does not exist yet, and dies.
- It **cannot** run unguarded at config-import time either — Playwright re-imports
  the config in every worker, so the copy re-runs *while the gateway is live*, the
  watcher reloads from disk, and every route the tests created through the admin
  API silently disappears mid-run.

So it runs at import time, guarded by `TEST_WORKER_INDEX` being unset (true only in
the main process).

Seeded routes:

| Route | Match | Policy | Shape |
|---|---|---|---|
| `echo-api` | `/api/*` | `echo-policy` | listener → cors → strip `/api` → upstream(echo) → client; upstream errors → `error-handler` (502) |
| `secure-api` | `/secure/*` | `secure-policy` | listener → key-auth → rate-limit → strip `/secure` → upstream(echo) → client |
| `dead-api` | `/dead/*` | `dead-policy` | listener → upstream(127.0.0.1:9, closed) → client; upstream errors → `error-handler` (502) |
| `bearer-api` | `/bearer/*` | `bearer-policy` | listener → openid-connect (bearer, JWKS) → strip `/bearer` → upstream(echo) → client |
| `app-api` | `/app/*` | `app-policy` | listener → openid-connect (interactive) → strip `/app` → upstream(echo) → client; the match covers the `/app/callback` redirect_uri |

The two OIDC routes run against a **hermetic mock IdP** (`e2e/mock-idp/`, port
3011): a discovery document, a JWKS, `/authorize`, and `/token`, issuing genuinely
RS256-signed tokens via `jose`. No Keycloak, no container — but the gateway fetches
the JWKS and verifies signatures for real, so it cannot tell the mock from a real
provider. `/mint` hands a spec a token of a chosen shape (valid, expired,
wrong-key, wrong-audience) for the negative cases. The mock is reusable for the
other SSO plugins (`authz-keycloak`, `authz-casdoor`, `cas-auth`) when they get
e2e coverage.

Both OIDC policies wire `oidc.error → client.in`: the plugin prepares its response
(a `401`, or the `302` to the IdP) and then returns an *error*, so the error edge
to `client` is what carries it to the caller — the same graph semantic as
`key-auth`, and just as easy to get wrong.

`secure-policy` wires `key-auth.error` and `rate-limit.error` **straight to
`client.in`**. That is deliberate and worth stating, because it is the one piece
of graph semantics people get wrong: a rejecting plugin sets the 401/429 on the
context and *then* returns an error. The status only survives if the error edge
reaches `client`. Wire it to an `error-handler` instead and the handler's own
status code overwrites it; leave it unwired and the generic 500 fallback does.

## Admin API — `tests/admin-api.spec.ts`

| ID | Scenario | Expected |
|---|---|---|
| E2E-API-01 | Request with no credentials | `401` + `WWW-Authenticate: Basic` challenge |
| E2E-API-02 | Request with wrong password | `401` |
| E2E-API-03 | `GET /api/routes` | Lists the three seeded routes |
| E2E-API-04 | `GET /api/plugins` **(regression guard)** | Advertises the **full** catalog — `proxy-cache`, `hmac-auth`, `traffic-split`, the loggers — not just the original 13. Fails if the catalog regresses to a hardcoded subset |
| E2E-API-05 | `POST /api/routes` then `GET` | Route is created and listed |
| E2E-API-06 | `PUT /api/routes/{name}` | Route is updated in place |
| E2E-API-07 | `DELETE /api/routes/{name}` | Route is gone; a request to its path now 404s |
| E2E-API-08 | `POST` a route referencing an unknown policy | Rejected (4xx); route table unchanged |
| E2E-API-09 | `PUT` a policy with an edge to a nonexistent node | Rejected; **the previous policy keeps serving traffic** (last-good guarantee) |
| E2E-API-10 | `/healthz`, `/readyz`, `/metrics` | `200`; metrics render Prometheus text |
| E2E-API-11 | UI static assets | Served **without** auth, unlike `/api/*` |
| E2E-API-12 | `GET /api/config/export` | `200` `text/yaml`; contains the live routes/policies; behind auth |

## Data plane — `tests/data-plane.spec.ts`

| ID | Scenario | Expected |
|---|---|---|
| E2E-DP-01 | `GET /api/hello` | `200`; echo reports `path: /hello` — the `/api` prefix was stripped by `proxy-rewrite` |
| E2E-DP-02 | `GET /nope` (no route matches) | `404` |
| E2E-DP-03 | Method not in the route's `methods` | Not routed (`404`) |
| E2E-DP-04 | `POST /api/echo` with a body | Body and method reach the upstream intact |
| E2E-DP-05 | `/secure/*` with no API key | `401`, body `{"error": "unauthorized"}` — proves the error edge to `client` preserves the plugin's status |
| E2E-DP-06 | `/secure/*` with a valid key | `200` |
| E2E-DP-07 | `/secure/*` past the burst allowance | `429` |
| E2E-DP-08 | `/dead/*` (upstream refuses connections) | `502` with the `error-handler`'s JSON template — not a raw 500 |
| E2E-DP-09 | `OPTIONS /api/*` preflight | `204` + `access-control-allow-origin` |
| E2E-DP-10 | Traffic then `GET /metrics` | Per-route request counter incremented |

## Web UI — `tests/editor.spec.ts`

| ID | Scenario | Expected |
|---|---|---|
| E2E-UI-01 | Open the admin port | Editor loads; sidebar lists the seeded routes; status shows the version and route count |
| E2E-UI-02 | Select `echo-api` | Canvas renders the policy's nodes and edges — the same graph the YAML declares |
| E2E-UI-03 | Open the **Add Node** drawer **(regression guard)** | Docs-mirrored category headers are shown; quick-search surfaces the full catalog (incl. `proxy-cache`, `hmac-auth`, `traffic-split`), not 14 entries |
| E2E-UI-04 | Search for a plugin in the drawer and add it | Node appears on the canvas |
| E2E-UI-05 | Click a node | Inspector opens with that plugin's schema-driven form |
| E2E-UI-06 | Create a route via the **New** dialog | Route appears in the sidebar **and** in `GET /api/routes` |
| E2E-UI-07 | Delete a route | Gone from the sidebar and the API |
| E2E-UI-08 | Toggle the theme | Theme flips and survives a reload (persisted) |

## openid-connect — `tests/openid-connect.spec.ts`

Bearer validation and interactive login against the mock IdP. The plugin's 14 unit
tests cover the pieces in isolation; these cover the wire — a token verified
against a fetched JWKS, and a browser actually bounced through the IdP.

| ID | Scenario | Expected |
|---|---|---|
| E2E-OIDC-01 | `/bearer/*` with no token | `401` |
| E2E-OIDC-02 | `/bearer/*` with a valid token **(regression guard)** | `200`; claims decoded into `x-userinfo` for the upstream. Fails if the default-algorithm bug returns |
| E2E-OIDC-03 | Token signed by a key not in the JWKS | `401` — the gateway verifies for real |
| E2E-OIDC-04 | Expired token | `401` |
| E2E-OIDC-05 | Token for a different audience | `401` (`match_with_client_id`) |
| E2E-OIDC-06 | Malformed bearer value | `401`, no crash |
| E2E-OIDC-07 | `/app/*` unauthenticated | `302` to the IdP `/authorize` with `code_challenge` (PKCE) + `state` |
| E2E-OIDC-08 | A browser completes login | Bounced IdP → callback → token exchange → lands on the upstream; `oidc_session` cookie set (sealed, not plaintext); claims in `x-userinfo` |
| E2E-OIDC-09 | Second request with the session cookie | Served **without** another trip to the IdP |
| E2E-OIDC-10 | A forged session cookie | Not accepted — redirected to log in |

## External auth — `tests/external-auth.spec.ts`

The nine HTTP/LDAP-delegation auth plugins — OIDC's siblings. Each delegates the
decision to an external service, and each was covered only by unit tests that
exercised request-building and response-parsing in isolation, never the actual
callout. OIDC proved that gap can hide a total-failure bug; these close it for the
rest of the family. Decisions are path-driven where possible (`.../allow/...`
permits) so a test needs only the URL it hits.

Two mock services back them, each speaking the plugins' real contracts:
`e2e/mock-auth/` (one Node HTTP server with an endpoint per plugin) and
`e2e/mock-ldap/` (a real LDAP-over-TCP server via `ldapjs`, since `ldap-auth` does
an actual simple bind, not an HTTP call).

| ID | Plugin | Scenario | Expected |
|---|---|---|---|
| E2E-FAUTH-01/02/03 | forward-auth | allow / deny / deny-doesn't-reach-upstream | 200 + `X-Auth-User` upstream; 403 + `X-Deny-Reason` to client; no upstream call on deny |
| E2E-OPA-01/02 | opa | allow / deny | 200 + `X-Opa-User` upstream; 403 with the OPA `reason` as body |
| E2E-CASDOOR-01/02/03 | authz-casdoor | no token / active / inactive | 403; 200; 403 (decided by RFC 7662 introspection) |
| E2E-WOLF-01/02/03 | wolf-rbac | allow / deny / malformed token | 200; ≥400; ≥400 before any callout |
| E2E-KC-01/02/03 | authz-keycloak | permit / refuse / no bearer | 200; 403 via the real UMA callout; 403 before the callout |
| E2E-CAS-01/02/03 | cas-auth | no ticket / valid / invalid | 401; 200; 401 |
| E2E-DINGTALK-01/02/03 | dingtalk-auth | valid code / bad code / no code | 200; 401; ≥400 |
| E2E-FEISHU-01/02 | feishu-auth | valid code / bad code | 200; 401 |
| E2E-LDAP-01..04 | ldap-auth | valid / wrong password / unknown user / no creds | 200; 401; 401; 401 + Basic challenge |
| E2E-BASIC-01..03 | basic-auth | valid / wrong password / no creds, with `users` in the UI array shape | 200; 401; 401 + Basic challenge |

All nine wire the plugin's `error` port to `client.in`: like `key-auth` and OIDC,
these plugins prepare the rejection (or redirect) response and then return an
*error*, so the error edge is what carries it to the caller.

## Editor round-trip — `tests/editor-roundtrip.spec.ts`

The existing editor tests prove the UI *renders*; these prove the other
direction — **what you build on the canvas is what gets saved**. Each edits the
throwaway `rt-policy` (a `beforeEach` resets it to the seed, so the tests are
order-independent), clicks Save Policy, and reads the policy back through the
admin API.

| ID | Scenario | Expected |
|---|---|---|
| E2E-UI-09 | Edit a `number` field (error-handler status) and save | Stored as a JSON **number** `507`, not the string `"507"` |
| E2E-UI-10 | Toggle a `switch` (cors allow-credentials) and save | Stored as a JSON **boolean** `true`, not `"true"` |
| E2E-UI-11 | Delete a node in the inspector and save | The node **and every edge that touched it** are gone from the policy — no dangling references |
| E2E-UI-12 | Save an unchanged graph | Nodes and edges round-trip identically (see the port-normalization note below) |
| E2E-UI-13 | Add a user to a basic-auth node **via the editor form**, save | The credential lands in the policy as the UI's array shape **and authenticates real traffic** (`alice:secret` → 200, wrong password → 401) — the end-to-end proof of the users-shape fix, driven from the actual UI |
| E2E-UI-14 | Expand/collapse an Add-Node drawer category | Plugins are hidden while the category is collapsed and revealed on expand (docs-mirrored grouping) |

Two things this surfaced, both benign but worth recording:

- **The UI normalizes the listener's port.** Hand-written YAML uses `listener.out`;
  the UI serializes it as `listener.success`. The engine treats the two as the
  same success edge (`engine.rs`: "`success` and `out` become success edges"), so
  a UI round-trip is semantically faithful but not byte-identical on that one port
  name. E2E-UI-12 normalizes before comparing.
- **basic-auth's form shape vs. the plugin's (fixed).** The UI's basic-auth form
  serializes `users` as an array of `{username, password}` objects, but the plugin
  originally parsed only a `{name: password}` map — so a basic-auth node with users
  built in the editor was rejected at save time (verified: HTTP 400). The plugin
  now accepts both shapes, mirroring `proxy-rewrite`'s `add_headers`. Guarded by
  `basic_auth.rs` unit tests and by `E2E-BASIC-01..03` (a fixture route using the
  array shape).

**Not automated: drag-to-create an edge.** ReactFlow v12 drives connections off
pointer-event hit-testing on ~10px handles that Playwright cannot reliably
reproduce headless (the connection never registers, or the target handle sits
outside the fitted viewport). Node deletion (E2E-UI-11) exercises the same
UI→policy edge serialization in the reliable direction — deleting a node removes
its edges from the saved policy — so edge *removal* is covered; edge *creation* by
dragging is left to manual QA.

## The loop — `tests/editor.spec.ts`

The scenarios that justify the suite. Each starts in the browser and ends with an
assertion about **live traffic on the data-plane port**.

| ID | Scenario | Expected |
|---|---|---|
| E2E-LOOP-01 | In the inspector, change `dead-policy`'s `error-handler` status from `502` to `599`, click **Save Policy**, then request `/dead/x` | The live response status changes `502` → `599`. Proves: UI form → admin API → validate → recompile → hot-swap → data plane, with no restart |
| E2E-LOOP-02 | Save an **invalid** policy from the UI | The API rejects it, the UI surfaces the error, and `/api/*` **keeps serving** on the last-good config |

## Debug & sandbox — `tests/debug.spec.ts`

Per-request policy tracing and the plugin sandbox. The fixture sets
`debug.enabled: true` with `max_traces: 20`; since that switch is static, **every
other spec in the suite also runs with debug mode on**, which is deliberate —
they collectively prove tracing stays inert for traffic that does not opt in
(`E2E-DEBUG-03` asserts it directly). The *disabled* path cannot be covered here
for the same reason, and is unit-tested in `src/admin/debug.rs` instead.

| ID | Scenario | Expected |
|---|---|---|
| E2E-DEBUG-01 | `/api/debug/*` without credentials | `401` |
| E2E-DEBUG-02 | `GET /api/debug/config` authed | `enabled: true`, reports the trigger header, `capture_bodies: false` |
| E2E-DEBUG-03 | Request **without** `x-featherbit-debug` | Nothing recorded — opt-in tracing really is opt-in |
| E2E-DEBUG-04 | Request **with** the header | Byte-identical response to the untraced one, plus `x-featherbit-trace-id`; the untraced response has no such header |
| E2E-DEBUG-05 | Fetch that trace | Steps in graph order (`cors → strip-prefix → echo-backend → client`); the `strip-prefix` step's `changes` attribute `/api/hello` → `/hello` **to that node** |
| E2E-DEBUG-06 | Trace a request carrying `x-api-key` and `authorization` | Both read `<redacted>`; neither secret appears anywhere in the trace JSON |
| E2E-DEBUG-07 | Trace a POST body with `capture_bodies` off | `body.len` recorded, `body.text` absent, payload absent from the trace |
| E2E-DEBUG-08 | Trace `/dead/*` (upstream refuses) | The failing node is recorded with `edge: "error"`; final status `502` |
| E2E-DEBUG-09 | Exceed `max_traces`, then `DELETE` | List capped at the configured capacity; clear empties it |
| E2E-DEBUG-09b | Trace a request that matches **no route** | Captured with policy `(no route matched)`, status `404`, zero steps, and a trace id — unrouted incoming requests are still visible |
| E2E-DEBUG-10 | Unknown trace id | `404` |
| E2E-DEBUG-11b | List filtered by `?route=`, `?policy=`, `?status=` | Only matching traces; a bare empty filter is ignored — the "browse recent requests on one policy" flow |
| E2E-DEBUG-11 | Sandbox a named policy | Same node sequence as live; the echo backend really answers (`200`); `source: "sandbox"` |
| E2E-DEBUG-12 | Sandbox an ad-hoc `proxy-rewrite` node | One user step; `request.path` rewritten |
| E2E-DEBUG-13 | Sandbox with no `context` at all | `200` — the synthetic context is fully defaulted |
| E2E-DEBUG-14 | Sandbox an unknown plugin type | `400` naming the offending type |
| E2E-DEBUG-15 | Sandbox an unknown policy | `404` |
| E2E-DEBUG-16 | Sandbox with neither/both of `nodes` and `policy` | `400` |

## Deliberately out of scope

Covered by the Rust suite with real sockets, or unreachable from Playwright:

- TLS/mTLS handshakes, SNI cert selection, cert hot-reload (`src/server/tls.rs`)
- HTTP/2 (ALPN + h2c) and the WebSocket relay, incl. RFC 8441
- L4 TCP/UDP stream proxying (Playwright cannot speak raw UDP)
- Graceful-shutdown drain on SIGTERM
- etcd cluster convergence (needs `docker-compose.etcd.yaml`)

These are candidates for a Rust `tests/e2e.rs` target later; they are not gaps in
*this* suite.
