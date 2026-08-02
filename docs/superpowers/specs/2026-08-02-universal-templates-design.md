# Universal Templates — Design Spec

Date: 2026-08-02
Status: approved (brainstormed and validated with Francesco)

## Context

Editing a plugin through the UI should give access to environment variables, the defined variables, and live context data in **every** text field — not just the ~15 fields that happen to interpolate `$var` today (the gap surfaced when typing `$method` into proxy-rewrite's *Add headers* produced no suggestions: that plugin sends header values literally). Extending the legacy `$` syntax everywhere is unsafe — a stored password `pa$sword4` silently corrupts to `pa`, regex `$` anchors and `$1` capture refs get eaten, JSON-Schema `$ref` breaks — so the gateway adopts **its own template syntax**, applied universally, with the legacy syntax kept for compatibility.

Decisions made during brainstorming:

- **New syntax `{{namespace.path}}`**, unambiguous by construction: non-matching `{{...}}` passes through literally (no silent-empty), so no escape syntax is needed in practice and the danger classes vanish.
- **Universal application**: every traffic-bound string config field renders templates (the ~30-plugin sweep below). Only *structural* fields are excluded (values compiled into non-strings at load). Secrets/credential fields are included — the new syntax makes that harmless and `{{env.KEY}}` in credential lists is useful.
- **Legacy `$var` unbroken**: the historically-enabled fields keep resolving `$var` (after the `{{...}}` pass); new fields never process `$`.
- **UI**: the popover triggers on `{{` in every text field; context-path suggestions with live previews where runtime templating applies, `{{env.NAME}}` suggestions everywhere (names only, values never sent); legend v2 organized by namespace with a legacy mapping.
- **Unknown references warn at load** (`tracing::warn` with coordinates) and pass through at runtime; no hard errors in V1.
- **`GET /api/env-vars`**: authed, sorted names only.

## Design

### 1. Template engine (`src/vars/template.rs`)

```rust
pub enum Segment { Literal(String), Ref(TemplateRef) }
pub struct Template { segments: Vec<Segment>, has_refs: bool }
pub enum TemplateRef {
    RequestMethod, RequestPath, RequestHost, RequestScheme, RequestBody,
    RequestHeader(String), RequestQuery(String), RequestCookie(String),
    ResponseStatus, ResponseBody, ResponseHeader(String),
    Message(String), ClientIp, ClientPort,
}
impl Template {
    pub fn parse(s: &str) -> (Template, Vec<String>); // template + warnings (unknown refs, passed through as literals)
    pub fn render(&self, ctx: &Context) -> Cow<'_, str>; // Literal-only -> Cow::Borrowed fast path
    pub fn is_literal(&self) -> bool;
}
```

- Grammar: `{{` `\s*` `<namespace path>` `\s*` `}}`. Namespaces exactly: `request.method|path|host|scheme|body`, `request.headers.<name>`, `request.query.<name>`, `request.cookies.<name>`, `response.status|body`, `response.headers.<name>`, `message.<key>` (key may contain dots — greedy to the closing braces), `client.ip|port`. Header names compared case-insensitively; written with dashes as-is (`{{request.headers.x-user-id}}`).
- `env.<NAME>` is **not** a `TemplateRef`: it is substituted at parse time from the process environment (same moment the legacy `${NAME}` pass runs — config load / compile), so rendered templates never see it. Unset `env.<NAME>` passes through literally + warning.
- Anything inside `{{...}}` that doesn't match the grammar (unknown namespace, `{{ $1 }}`, mustache leftovers) is kept as a literal segment; well-formed-but-unknown paths (e.g. `request.headres.x`) additionally produce a warning string returned by `parse`.
- Value semantics reuse the existing resolver behavior (first header/query value; lossy-UTF-8 bodies; message values stringified like `msg_*`): implemented by delegating to `vars::resolve` equivalents so the two systems cannot drift (shared internal fns extracted where needed).
- **Legacy interop**: a `Template::render_with_legacy(ctx)` variant used ONLY by the historically-`$`-enabled fields — applies the legacy `interpolate` pass to the template's LITERAL segments only; rendered `{{...}}` output is never `$`-processed (prevents runtime data from becoming a `$var` read primitive). New adopters call plain `render`.
- **Carve-out**: `body-transformer` keeps its own `{{...}}` engine untouched (documented); its field is excluded from the universal layer.

### 2. Plugin sweep (universal application)

Fields adopting `Template` (parse at `from_config`, render per request) — from the audit:

- **Request-bound**: proxy-rewrite `add_headers` values + `add_path_prefix`; attach-consumer-label `header_prefix`; request-id `header_name`; data-mask replace `value`s (regex-action `value` stays legacy-excluded — `$1` refs — but gains `{{...}}` since it's unambiguous: include); degraphql `query`/`operation_name`/`variables`; proxy-mirror `host`/`path`; FaaS `function_uri` families; forward-auth `request_headers`/`upstream_headers` names; opa `send_headers_upstream` names.
- **Response-bound**: echo `body`/`before_body`/`after_body`/`headers` values; error-page bodies + `content_type`; response-rewrite `body` (un-baked from `Bytes` to `Template`) + filter `replace`; api-breaker `break_response_body`; every `rejected_msg` (uri-blocker, ua-restriction, referer-restriction, acl, consumer-restriction, limit-count, limit-conn, workflow, request-validation, oas-validator); cors `allowed_methods`/`allowed_headers` values; basic/hmac/ldap `realm`; csrf cookie `name`; mocking `content_type`.
- **Existing 15 `$var` fields**: unified onto `Template` with `render_with_legacy` (behavior superset, zero breakage).
- **Excluded (structural — `{{env.*}}` still applies at load)**: regex/pattern fields, JSON-Schema/OpenAPI documents, Lua sources, Casbin models, balancer targets/ports, IP lists, TLS material, logger endpoints/file paths, numeric fields. Each exclusion documented with its reason in the reference page.

### 3. Validation & warnings

During `compile_routes` (and sandbox), after plugin-config resolution: walk every string leaf of node configs for policies/supernodes/plugin_configs, run `Template::parse`, and `tracing::warn!` each warning with coordinates (`policy 'p' node 'n' field 'body': unknown template reference '{{request.headres.x}}' (passed through literally)`). Plugins independently parse their own fields at construction (single source of truth for behavior; the compile-walk exists only for warnings, so it can't drift behavior).

### 4. Admin API

`GET /api/env-vars` (new `src/admin/env_vars.rs`, mounted in the authed block): `{ "names": ["HOME", "PATH", ...] }` — sorted, names only, values never serialized. `GET /api/vars` extended: catalog entries gain a `path` field (the new namespace spelling, e.g. `request.headers.*`) alongside the legacy `name`, so the UI/legend map both.

### 5. Web UI

- `VarInput` triggers on `{{` (token regex extended; suggestions filter on the path after `{{`); legacy `$` trigger remains only for fields flagged legacy.
- Field attributes flip to opt-out: `template?: 'full' | 'env-only'` (default `full` for text/textarea/list/objects-text; structural exclusions marked `env-only`; the flag replaces `vars: true`, whose fields become `full` + `legacyDollar: true`).
- Suggestion model re-keys to namespace paths (live values/availability/redaction machinery unchanged); a new `env` group lists `{{env.NAME}}` from `/api/env-vars`, no values, available in both `full` and `env-only` fields.
- Insertion: always the `{{path}}` form; dotted message keys need no special casing anymore (the braces are the syntax).
- Legend v2: sections per namespace with live values, env section (names only), pass-through/warning semantics, exclusions note, legacy `$var` → `{{path}}` mapping table.

### 6. Testing

- Unit: template parse/render/fast-path/pass-through/warnings/env-at-parse; per-plugin representative tests for every swept field (literal unchanged; `{{request.*}}` renders; `pa$sword`-style strings and `$1`/`$ref` untouched in newly-templated fields); legacy fields keep `$var` behavior (regression suite reuse).
- Admin: `/api/env-vars` (names only, no values — assert a known-set env var's value absent from the body), `/api/vars` path field.
- E2E: the original complaint end-to-end — via the browser, add a proxy-rewrite request header choosing `{{request.method}}` from the popover, send traffic, assert the upstream echo received the method; `{{env.*}}` suggestion appears in an env-only field; legend v2 renders namespaces.
- Docs: `reference/templates.md` (canonical: namespaces, load-vs-request time, pass-through, exclusions, legacy mapping); `context-vars.md` updated to defer to it; parity-deviation note; roadmap + CLAUDE.md bullet.

## Critical files

| Area | Files |
|---|---|
| Engine | `src/vars/template.rs` (new), `src/vars/mod.rs` (shared resolver internals), `src/vars/catalog.rs` (path field) |
| Sweep | ~30 files under `src/plugins/native/` per §2 |
| Warnings | `src/state.rs` (compile walk), `src/admin/debug.rs` (sandbox parity) |
| Admin | `src/admin/env_vars.rs` (new), `src/admin/vars.rs`, `src/admin/mod.rs` |
| UI | `ui/src/components/VarInput.tsx`, `ui/src/varSuggestions.ts`, `ui/src/pluginConfig.ts` (flag flip), `ui/src/components/VarLegend.tsx`, `ui/src/api/client.ts`, `ui/src/types/index.ts` |
| E2E/docs | `e2e/tests/`, `e2e/E2E_TESTBOOK.md`, `website/docs/reference/templates.md` |

## Verification

- `cargo test` green; fmt/clippy clean; `ui`/`website` builds green
- `cargo build --release && cd e2e && npm test` incl. the proxy-rewrite browser scenario
- Manual: UI popover on `{{` in a previously-suggestion-less field (proxy-rewrite add_headers) with live values; `{{env.NAME}}` suggested everywhere; password-with-`$`, `$1` replace, and `^/x$` regex configs behave identically before/after
