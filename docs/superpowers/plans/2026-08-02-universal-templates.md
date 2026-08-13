# Universal Templates Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** The gateway's own `{{namespace.path}}` template syntax, rendered per request in every traffic-bound string config field (~30 plugins), with env-var substitution, load-time warnings, `GET /api/env-vars`, and UI suggestions in every text field.

**Architecture:** A pre-parsed `Template` type in `src/vars/template.rs` (literal fast path; refs resolved against `Context` via the existing resolver internals; `env.*` substituted at parse). Plugins adopt it field-by-field per the sweep tables below; the 15 legacy `$var` fields unify via `render_with_legacy`. UI flips suggestions from opt-in to opt-out and re-keys the suggestion model to namespace paths.

**Tech Stack:** Rust (no new deps), React 19 (no new deps), Playwright.

**Spec:** `docs/superpowers/specs/2026-08-02-universal-templates-design.md` — read it first. The field tables in Tasks 2-5 are the authoritative sweep inventory (derived from a full audit of `src/plugins/native/`).

## Global Constraints

- Conventional Commits, **no Co-Authored-By trailer**. Branch `feature/universal-templates` (exists, off develop).
- No new Rust or npm dependencies.
- Syntax: `{{` `\s*` `path` `\s*` `}}`. Namespaces exactly: `request.method|path|host|scheme|body`, `request.headers.<name>`, `request.query.<name>`, `request.cookies.<name>`, `response.status|body`, `response.headers.<name>`, `message.<key-may-contain-dots>`, `client.ip|port`, `env.<NAME>` (parse-time).
- Pass-through rule: `{{...}}` whose FIRST path segment is not a known namespace passes through silently as a literal; a known namespace with an unparseable/unknown remainder passes through AND yields a parse warning. A parseable ref whose subject is absent AT RENDER TIME (missing header, unset message key) renders as the empty string (matches legacy `$var` semantics; documented).
- Legacy `$var` processing runs ONLY in the 15 historically-enabled fields (Task 2 table), always AFTER the `{{...}}` pass. New fields never process `$` — that is the safety property (passwords with `$`, `$1` refs, `$ref` untouched).
- `body-transformer`'s template fields are carved out entirely (own `{{...}}` engine).
- Every Rust task: targeted tests, full `cargo test`, `cargo fmt --check`, `cargo clippy --all-targets --locked -- -D warnings` green at every commit; paste actual outputs. UI tasks: `cd ui && npm run build && npm run lint`.

---

### Task 1: Template engine — `src/vars/template.rs`

**Files:**
- Create: `src/vars/template.rs`
- Modify: `src/vars/mod.rs` (add `pub mod template;`; make `cookie_value`, `message_str`, `split_remote_addr`, `query_string` helpers `pub(crate)` if not already)

**Interfaces:**
- Produces (consumed by every later task):

```rust
pub enum TemplateRef {
    RequestMethod, RequestPath, RequestHost, RequestScheme, RequestBody,
    RequestHeader(String), RequestQuery(String), RequestCookie(String),
    ResponseStatus, ResponseBody, ResponseHeader(String),
    Message(String), ClientIp, ClientPort,
}
pub enum Segment { Literal(String), Ref(TemplateRef) }
pub struct Template { /* segments, has_refs */ }
impl Template {
    pub fn parse(s: &str) -> (Template, Vec<String>);                    // uses std::env for env.*
    pub fn parse_with_env(s: &str, env: &dyn Fn(&str) -> Option<String>) -> (Template, Vec<String>);
    pub fn render(&self, ctx: &Context) -> Cow<'_, str>;                 // Cow::Borrowed for literal-only
    pub fn render_with_legacy(&self, ctx: &Context) -> String;           // render() then vars::interpolate over the result
    pub fn is_literal(&self) -> bool;
    pub fn source_warnings_only(s: &str) -> Vec<String>;                 // parse + discard template (compile-walk helper)
}
```

- [ ] **Step 1: Create the file with module doc + FULL test suite first.** Tests to write (all in `#[cfg(test)] mod tests`, using a small `test_ctx()` builder mirroring `src/vars/mod.rs`'s test helper):

```rust
    // Parsing / structure
    test_literal_only_fast_path            // no "{{" -> is_literal(), render returns Cow::Borrowed same ptr
    test_parse_known_refs                  // "{{request.method}} {{ response.status }}" -> 2 refs, whitespace inside braces ok
    test_unknown_namespace_passes_silently // "{{ $1 }}", "{{mustache}}", "{{body.x}}" -> literal, NO warnings
    test_known_namespace_bad_leaf_warns    // "{{request.headres.x}}", "{{client.mac}}" -> literal + 1 warning each
    test_env_substituted_at_parse          // parse_with_env("{{env.REGION}}", REGION=eu) -> literal "eu", no refs
    test_env_unset_passes_and_warns        // "{{env.NOPE}}" -> literal "{{env.NOPE}}" + warning
    test_unclosed_braces_literal           // "{{request.method" -> literal, no warning, no hang
    test_message_key_with_dots             // "{{message.consumer.key_id}}" -> Message("consumer.key_id")

    // Rendering
    test_render_request_basics             // method/path/host/scheme
    test_render_headers_case_insensitive_dashes  // {{request.headers.X-User-ID}} matches header "x-user-id"; first value
    test_render_response_header_and_status
    test_render_query_first_value
    test_render_cookie                     // via Cookie header
    test_render_message_stringified       // non-string values JSON-stringified like $msg_*
    test_render_client_ip_port
    test_render_bodies_lossy
    test_absent_subject_renders_empty      // missing header -> ""
    test_dollar_untouched_in_render        // "pa$sword4 {{request.method}}" -> "pa$sword4 GET" — plain render never touches $

    // Legacy interop
    test_render_with_legacy_applies_dollar_after  // "{{request.method}} $uri" -> "GET /path"
    test_render_plain_ignores_dollar              // "$uri" via render() -> "$uri" literally
```

Write each with concrete asserts (the implementer writes real bodies from these names + the semantics in Global Constraints; every behavior named here is normative).

- [ ] **Step 2: Verify failure** — wire `pub mod template;` in `src/vars/mod.rs`; `cargo test template` → COMPILE ERROR.

- [ ] **Step 3: Implement.** Parser: scan for `{{`, find matching `}}` (no nesting; unclosed → literal to end), trim inner, split FIRST segment on `.`:
  - `request` → match remainder: `method|path|host|scheme|body` → unit refs; `headers.<rest>` → `RequestHeader(rest)` (store as-written; lowercase at render); `query.<rest>`; `cookies.<rest>`; anything else → literal + warning `unknown template reference '{{...}}' (passed through literally)`.
  - `response` → `status|body|headers.<rest>` likewise.
  - `message` → `Message(<entire remainder>)` (empty remainder → warning).
  - `client` → `ip|port` else warning.
  - `env` → parse-time: `env(NAME)` Some → push resolved value as Literal; None → keep raw `{{env.NAME}}` literal + warning.
  - any other first segment → literal, silent.
  Renderer: delegate to the same logic `vars::resolve` uses — call the now-`pub(crate)` helpers (`cookie_value`, `message_str`, `split_remote_addr`) and the same first-value header/query lookups (headers: `replace` nothing — lowercase the stored name, compare against snapshot-lowercased map keys as `ctx.request.headers` already stores lowercase). Merge adjacent literals at parse; `render` on literal-only returns `Cow::Borrowed(&self.single_literal)`.

- [ ] **Step 4: Run gates** — `cargo test template` all green; full suite, fmt, clippy (add repo-convention `#[allow(dead_code)] // consumed by the plugin sweep (next tasks)` where clippy demands, to be removed as tasks consume).

- [ ] **Step 5: Commit** — `feat(vars): {{namespace.path}} template engine with parse-time env and warnings`

---

### Task 2: Unify the 15 legacy `$var` fields onto `Template::render_with_legacy`

**Files (the complete legacy inventory — field, use site):**

| Plugin | Field | Legacy use site |
|---|---|---|
| redirect | `uri` | `src/plugins/native/redirect.rs:149,153` |
| exit-transformer | `body` | `exit_transformer.rs:132` |
| mocking | `response_example` | `mocking.rs:213` |
| mocking | `response_headers` values | `mocking.rs:217` |
| limit-count | `key` | `limit_count.rs:166-168` |
| limit-conn | `key` | `limit_conn.rs:200` |
| lago | `event_transaction_id`, `subscription_id` | `lago.rs:236-237` |
| workflow | nested limit-count `key` | `workflow.rs:311` |
| proxy-cache | `cache_key` components | `proxy_cache.rs:243` |
| fault-injection | `abort.body`, `abort.headers` values | `fault_injection.rs:312,317` |
| traffic-label | `set_headers`/`set_labels` values | `traffic_label.rs:225,230` |
| forward-auth | `extra_headers` values | `forward_auth.rs:228` |
| response-rewrite | `add_headers`/`set_headers` values | `response_rewrite.rs:452,460` |
| loggers (shared) | `log_format` values | `src/plugins/util/log_entry.rs:65` (`build_custom`) |
| real-ip | `source` — **DO NOT TOUCH** (bare var name, not a template) | excluded |

**Recipe** (worked example: redirect): the plugin struct field stays `String`; at the use site replace `vars::interpolate(&ctx, &self.uri)` with a parsed-at-construction `Template` field (`uri_tpl: Template`) rendered via `self.uri_tpl.render_with_legacy(&ctx)`. Parse in `from_config` with `Template::parse(&uri)` — DISCARD warnings there (the compile-walk in Task 6 reports them); store the template alongside/instead of the raw string. `log_entry.rs::build_custom` swaps its per-value `interpolate` for per-value pre-parsed templates held in the parsed `LogFormat` (parse once where `parse_log_format` runs).

- [ ] **Step 1: Failing tests** — for redirect, mocking, limit-count add one test each asserting the SUPERSET behavior: `{{request.path}}` renders AND `$uri` still renders in the same field value (e.g. redirect uri `"{{request.scheme}}://x$uri"` → `"http://x/p"`). Place next to each plugin's existing tests, reusing their context builders.
- [ ] **Step 2: Verify failures** (`{{...}}` currently passes through as literal in those fields).
- [ ] **Step 3: Apply the recipe to all 15** (skip real-ip). Remove Task 1's leftover `dead_code` allows that these consumptions make redundant.
- [ ] **Step 4: Gates** — the plugins' existing `$var` regression tests must ALL still pass untouched (that is the compat proof); full suite, fmt, clippy.
- [ ] **Step 5: Commit** — `feat(plugins): unify legacy $var fields onto Template with legacy rendering`

---

### Task 3: Sweep A — request-bound fields (plain `render`, no legacy)

**Fields (complete):**

| Plugin | Field(s) | Use site |
|---|---|---|
| proxy-rewrite | `add_headers` values | `proxy_rewrite.rs:194-200` (and response phase `:209-215`) |
| proxy-rewrite | `add_path_prefix` | `proxy_rewrite.rs:189-191` |
| attach-consumer-label | `header_prefix` | `attach_consumer_label.rs:73` |
| request-id | `header_name` | `request_id.rs:102,108,120` |
| data-mask | replace-action `value` AND regex-action `value` (`{{...}}` only — `$1` stays literal) | `data_mask.rs:151,187,169-173` |
| degraphql | `query`, `operation_name`, `variables[]` | `degraphql.rs:197,199,212,245` |
| proxy-mirror | `host`, `path` | `proxy_mirror.rs:136-139` |
| FaaS family | `function_uri` (openfunction/azure-functions/openwhisk via `faas.rs:24-62`), aws-lambda `function_uri` | respective plugins |
| forward-auth | `request_headers[]`, `upstream_headers[]` (header NAMES) | `forward_auth.rs:240,253` |
| opa | `send_headers_upstream[]` (names) | `opa.rs:213` |

**Recipe**: parse each field to `Template` at `from_config` (warnings discarded), render with plain `render(&ctx)` at the use site. For Vec/map fields parse each element. `strip_path_prefix` and `remove_headers` are matchers, not emitted values — DO NOT template them.

- [ ] **Step 1: Failing tests** — proxy-rewrite: `{{request.method}}` in an add_headers value reaches the upstream request; `pa$sword4` in a value stays byte-identical (the safety property, MUST be asserted); request-id custom header name with `{{request.headers.x-tenant}}`; data-mask regex-action value keeps `$1` working while `{{request.path}}` renders.
- [ ] **Step 2-4: standard TDD + gates.**
- [ ] **Step 5: Commit** — `feat(plugins): template rendering for request-bound config fields`

---

### Task 4: Sweep B — response bodies & rejection messages (plain `render`)

**Fields (complete):**

| Plugin | Field(s) | Use site |
|---|---|---|
| echo | `body`, `before_body`, `after_body` | `echo.rs:151,173,177` |
| error-page | `error_XXX.body` (un-bake `Bytes`→`Template`) | `error_page.rs:110-113,138` |
| response-rewrite | `body` (un-bake; respect the base64 path — a base64 body is decoded FIRST, then NOT templated: only the plain-text path templates; document in code) + filter `replace` (`$1` stays literal, `{{...}}` renders) | `response_rewrite.rs:38,405-407,442-443` |
| api-breaker | `break_response_body` | `api_breaker.rs:272` |
| uri-blocker | `rejected_msg` | `uri_blocker.rs:130-131,145` |
| ua-restriction | `rejected_msg` | `ua_restriction.rs:133,143` |
| referer-restriction | `message` | `referer_restriction.rs:161,171` |
| acl | `rejected_msg` | `acl.rs:102,147` |
| consumer-restriction | `rejected_msg` | `consumer_restriction.rs:197,243` |
| limit-count / limit-conn / workflow | `rejected_msg` | `limit_count.rs:256-261`, `limit_conn.rs:229-233`, `workflow.rs:339-341` |
| request-validation / oas-validator | `rejected_msg` | `request_validation.rs:182-186`, `oas_validator.rs:357-361` |

Out of scope, stated for the reviewer: `error-handler.body_template` keeps its own `{{error.*}}` mini-engine (collision — same reasoning as body-transformer; note it in the docs task).

- [ ] Steps: failing tests (echo body with `{{request.path}}` + `$` money string untouched; one rejected_msg plugin, e.g. limit-count, rendering `{{request.method}}`; error-page body un-bake keeps existing byte-equality tests green), implement recipe, gates, commit — `feat(plugins): template rendering for response bodies and rejection messages`

---

### Task 5: Sweep C — response header values/names & realms (plain `render`)

**Fields (complete):**

| Plugin | Field(s) | Use site |
|---|---|---|
| echo | `headers` values | `echo.rs:187-191` |
| error-page | `content_type` | `error_page.rs:141` |
| mocking | `content_type` | `mocking.rs:224` |
| cors | `allowed_methods[]`, `allowed_headers[]` values | `cors.rs:162,166` |
| basic-auth / hmac-auth / ldap-auth | `realm` | `basic_auth.rs:125-126`, `hmac_auth.rs:337-338`, `ldap_auth.rs:145-146,259-260` |
| csrf | cookie `name` | `csrf.rs:214,220,277,286` |

DO NOT template: `cors.allowed_origins` (semantic tokens `*`/origin-echo), any pattern/secret/schema/endpoint field (spec §2 exclusions).

- [ ] Steps: failing tests (echo header value, basic-auth realm with `{{request.host}}`), recipe, gates, commit — `feat(plugins): template rendering for header values, names, and realms`

---

### Task 6: Compile-walk warnings

**Files:** Modify `src/state.rs` (in `compile_routes`, after `resolve_plugin_configs`), `src/admin/debug.rs` (sandbox parity).

Walk every string leaf of every node `config` (policies + supernodes + plugin_configs) with `Template::source_warnings_only(s)`; `tracing::warn!("policy '{p}' node '{n}' key '{k}': {warning}")` (supernode/plugin-config variants name their container). Recursive walk over `serde_json::Value` (arrays/objects). Pure logging — never fails the config.

- [ ] Steps: test with `tracing_test` or by asserting the walk function returns the collected warnings (make the walker return `Vec<String>` and have the caller log — testable without capturing logs: `fn collect_template_warnings(gw: &GatewayConfig) -> Vec<String>`); failing test: config with `{{request.headres.x}}` in a node config yields one warning naming policy/node/key; implement; gates; commit — `feat(state): load-time warnings for unknown template references`

---

### Task 7: `GET /api/env-vars` + catalog `path` field

**Files:** Create `src/admin/env_vars.rs`; modify `src/admin/mod.rs` (mount), `src/vars/catalog.rs` (+`path` field), `src/admin/vars.rs` (serialization picks it up automatically).

- `env_vars.rs`: `GET /api/env-vars` → `{ "names": [ ...sorted std::env::vars() keys... ] }`. Test asserts: 200, names sorted, and for a test-set env var (`std::env::set_var("FB_TEST_SECRET_VALUE_CANARY", "s3cr3t")` in the test) the VALUE string does not appear anywhere in the body.
- `catalog.rs`: `VarEntry` gains `pub path: &'static str` (serialized always) — the namespace spelling: `uri`→`request.path`, `method`/`request_method`→`request.method`, `host`→`request.host`, `scheme`→`request.scheme`, `protocol`→`""` (no template equivalent — document), `remote_addr`→`client.ip`, `remote_port`→`client.port`, `query_string`→`""`, `status`→`response.status`, `resp_body`→`response.body`, `request_body`→`request.body`, `consumer_name`→`message.consumer.name`, `consumer_group_id`→`message.consumer.group`, families: `http_*`→`request.headers.*`, `arg_*`→`request.query.*`, `cookie_*`→`request.cookies.*`, `post_arg_*`→`""` (no template namespace in V1 — document), `msg_*`→`message.*`, `sent_http_*`→`response.headers.*`. Update the drift test only if it inspects fields (it compares names — unaffected).
- [ ] Steps: failing tests, implement, mount inside authed block, gates, commit — `feat(admin): env-var name catalog endpoint and template paths in /api/vars`

---

### Task 8: UI — `{{` trigger, path suggestions, env group

**Files:** Modify `ui/src/components/VarInput.tsx`, `ui/src/varSuggestions.ts`, `ui/src/api/client.ts`, `ui/src/types/index.ts`.

- Types/client: `VarEntry` gains `path: string`; `listEnvVars: () => request<{names: string[]}>('/api/env-vars').then(r => r.names)`.
- `varSuggestions.ts`: suggestions re-key to paths — statics map via catalog `path` (skip entries with empty path); discovered members become `request.headers.<h>` / `request.query.<q>` / `response.headers.<h>` / `message.<k>` (live values unchanged; the underscore-header guard DROPS — dashes are legal in the new syntax, so header discovery no longer mangles names: `{{request.headers.x_trace_id}}` works verbatim — delete the old guard for the path-keyed entries and keep names as-is); new `Suggestion.insert` = `{{path}}`; new group `'env'` fed by `listEnvVars` (module-cached like the catalog), entries `{name: 'env.HOME', insert: '{{env.HOME}}', group: 'env'}` with NO value. The hook always returns ALL groups (context + env); per-field filtering happens in `VarInput` via its `templateMode` prop (Task 9) — one hook instance serves fields with different modes.
- `VarInput.tsx`: token regex becomes `/(\{\{\s*([A-Za-z0-9_.\-]*)|\$\{?([A-Za-z0-9_.]*))$/` — the `{{` branch always active; the `$` branch only when a new prop `legacyDollar: boolean` is true. Insertion for `{{` tokens replaces from the `{{` through caret with `suggestion.insert` (which includes braces) and consumes a trailing `}}`/`}` at the caret if present (same guard pattern as the `${` fix). Availability footer unchanged.
- [ ] Steps: build+lint; hand-trace insertion cases in the report ( `{{re` → pick `request.method` → `{{request.method}}` exactly once); commit — `feat(ui): {{path}} suggestions with env group and legacy-$ gating`

---

### Task 9: UI — flag flip in `pluginConfig.ts` + SchemaForm plumbing

**Files:** Modify `ui/src/pluginConfig.ts`, `ui/src/components/SchemaForm.tsx`, `ui/src/components/NodeInspector.tsx` (pass templateMode through varContext).

- `FieldSchema`: replace `vars?: boolean` with `template?: 'full' | 'env-only' | 'none'` (default when absent: `'full'` for `text`/`textarea`/list-text/objects-text; `radio/select/switch/number` are structurally `'none'`) plus `legacyDollar?: boolean` on the 15 legacy fields (Task 2 table). Mark `'env-only'` on: regex/pattern fields (uri-blocker `block_rules`, ua-restriction lists, response-rewrite filter `regex`, data-mask `regex`), schema/DSL textareas (request-validation schemas, oas-validator spec, authz-casbin model/policy, serverless `functions`), endpoint/path/secret fields (logger endpoints incl. http/loki/clickhouse/elasticsearch etc., file paths, all `secret`/`password`/`key`-bearing credential fields, upstream targets host, tls paths), `cors.allowed_origins`, `body-transformer` templates, `error-handler.body_template`. Every current `vars: true` becomes `template: 'full', legacyDollar: true`.
- SchemaForm: `varContext` carries `suggestionsFor(mode)` or simpler — NodeInspector runs the hook twice? NO: hook returns both groups; SchemaForm/VarInput filter by the field's mode (pass `templateMode` prop to VarInput; VarInput filters out non-env groups when env-only). All four render sites attach VarInput whenever mode !== 'none' and varContext present.
- [ ] Steps: build+lint; verify by grep that no `vars: true` remains; commit — `feat(ui): opt-out template suggestions with env-only field classes`

---

### Task 10: UI — Legend v2

**Files:** Modify `ui/src/components/VarLegend.tsx`.

Sections become: Request / Response / Message & consumer / Client / Environment (names from `listEnvVars`, no values, count shown) / Legacy `$var` mapping (from catalog `name`+`path`, entries with empty path marked "legacy only"). Keep live values + family member sub-rows keyed by the new paths; caveats updated (pass-through rule, warnings at load, `$` only in legacy fields, structural exclusions are env-only).
- [ ] Steps: build+lint; commit — `feat(ui): legend v2 organized by template namespaces`

---

### Task 11: E2E

**Files:** Create `e2e/tests/templates.spec.ts`; modify `e2e/E2E_TESTBOOK.md`.

Scenarios (reuse helpers/idioms from `var-suggestions.spec.ts` + `editor.spec.ts`):
- `E2E-TPL-01` (API+data plane): policy with proxy-rewrite `add_headers: [{name: x-tpl, value: "m={{request.method}} $keep"}]` in front of the echo upstream → request `/…` → echo response proves the upstream received `x-tpl: m=GET $keep` (dollar untouched, template rendered). Requires reading how the echo backend reflects headers (check `e2e` seed config / `data-plane.spec.ts`).
- `E2E-TPL-02` (browser): open the proxy-rewrite node (previously suggestion-less), focus an add_headers value input, type `{{re` → popover lists `request.method` with live value when a trace exists; Enter inserts `{{request.method}}`; save.
- `E2E-TPL-03` (browser): an env-only field (e.g. uri-blocker `block_rules` item) — typing `{{` lists ONLY `env.*` entries; typing `{{env.` filters env names (assert a name from `GET /api/env-vars` appears; no `request.*` rows).
- `E2E-TPL-04` (API): `GET /api/env-vars` names sorted; a canary env value string absent from the body (set via the e2e gateway launch env if the harness allows; else assert a well-known var's name present and response contains no `=`).
- [ ] Steps: write; `cargo build --release`; targeted run; FULL `npm test`; testbook; commit — `test(e2e): universal template rendering, suggestions, env-only fields`

---

### Task 12: Docs

**Files:** Create `website/docs/reference/templates.md`; modify `website/sidebars.ts`, `website/docs/reference/context-vars.md` (defer banner + legacy mapping link), `website/docs/guides/web-ui.md` or `debugging.md` (suggestion UX update), `docs/apisix-parity.md` (deviation note: universal templating), `website/docs/reference/roadmap.md`, `CLAUDE.md` (bullet: `- **Universal config templates** — {{namespace.path}} rendering in all traffic-bound plugin config, with env vars and live-preview suggestions everywhere`).

`templates.md` content: syntax + namespaces table; load-time (`env.*`, `${VAR}` legacy) vs request-time resolution; pass-through + warning semantics; absent-subject → empty; exclusions table with reasons (structural fields, patterns, `cors.allowed_origins`, body-transformer/error-handler own engines); legacy `$var` mapping table + "legacy processed only in these fields" list; migration notes (no action needed; `${name}` env quirk documented).
- [ ] Steps: write; `cd website && npm run build`; commit — `docs: universal templates reference, parity deviation, roadmap`

---

### Task 13: Final verification

- [ ] Full `cargo test`, fmt, clippy; `ui` + `website` builds; `cargo build --release && cd e2e && npm test` (FULL).
- [ ] Manual sweep: gateway with debug on; popover on `{{` in proxy-rewrite headers with live values; `{{env.NAME}}` suggested in an env-only regex field; config containing `pa$sword4`, `^/x$`, `$1` replace behaves identically before/after (diff a traced request).
- [ ] superpowers:requesting-code-review, then superpowers:finishing-a-development-branch (PR → develop).
