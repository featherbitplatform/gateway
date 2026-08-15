# Context Var Suggestions — Design Spec

Date: 2026-08-02
Status: approved (brainstormed and validated with Francesco)

## Context

Plugin configs reference live request/response data through `$var` interpolation (`src/vars/mod.rs`) — `$uri`, `$http_<header>`, `$arg_<param>`, `$msg_<key>`, and friends — but the var set is undocumented (a `match` statement), a typo silently interpolates to an empty string, and authors have no way to see what data is actually flowing through a node while they configure it. This feature adds **context-aware autocompletion with live value preview** to the UI's plugin config editors, plus a **var legend** documenting every supported var.

Decisions made during brainstorming:

- **Existing `$var` syntax** is the target — no new `$context.*` namespace. The tooling suggests and previews what plugins already resolve.
- **Close the coverage gaps** so the whole Context is addressable: add `$sent_http_<name>` (response headers) and `$request_body` (raw request body) to the engine.
- **Names always, values when available**: the dropdown and legend always offer the catalog; live values (and dynamically discovered names) appear only when debug is enabled, the node has an incoming edge, and a trace exists for the policy containing the predecessor node — values come from the latest handled request. Unavailable states are explained, never blank.
- **V1 editors**: SchemaForm fields explicitly flagged as var-accepting. The raw JSON editor and expression-rule (`[var, op, value]`) fields are out of scope for V1.
- **Catalog served by the backend** at `GET /api/vars`, drift-guarded against the resolver.

## Design

### 1. Engine — complete the var surface (`src/vars/mod.rs`)

Two additive match arms in `resolve()`:

- `sent_http_<name>` — response header lookup; `_` in `<name>` maps to `-`, compared case-insensitively (exact mirror of the `http_<name>` request-header family). First value joined like `http_*` does.
- `request_body` — lossy UTF-8 of `ctx.request.body` (exact mirror of `resp_body`).

With these, every Context field is reachable: request (method, path/uri, host, scheme, protocol, remote addr/port, query string + per-param, headers, cookies, form fields, raw body), response (status, headers, body), and message keys.

### 2. Engine — var catalog + endpoint

New `src/vars/catalog.rs`:

```rust
pub struct VarEntry {
    pub name: &'static str,        // "uri" or "http_*"
    pub kind: VarKind,             // Static | Family
    pub family_source: Option<&'static str>, // for families: "request_headers" | "query_params" | "cookies" | "form_body" | "message" | "response_headers"
    pub description: &'static str,
    pub example: &'static str,     // "$http_user_agent"
}
pub fn var_catalog() -> &'static [VarEntry];
```

- One entry per static var (incl. aliases like `method`/`request_method` — both listed, description cross-references), one per family.
- **Drift guard**: a source-parse test over `resolve()`'s match arms (same technique as `KNOWN_PLUGIN_TYPES` and the plugin catalog) asserting every arm has a catalog entry and vice versa — both directions.
- `GET /api/vars` (new tiny handler in `src/admin/`, mounted in the authed block) returns `{ "vars": [ {name, kind, family_source, description, example}, ... ] }`.

No other backend changes. Live values come from the existing debug endpoints; `ui/src/api/client.ts`'s `listTraces()` gains optional `{policy, limit}` params mapped to the already-implemented server-side `TraceFilter` query string.

### 3. UI — suggestion data flow

New module `ui/src/varSuggestions.ts` + hook `useContextSuggestions`:

- Inputs: policy name, selected node id, ReactFlow `edges`, `debugConfig`, the var catalog (fetched once via `api.listVars()`), and editor kind (policy vs supernode definition).
- Predecessor resolution: the incoming edge with `sourceHandle === 'success'` wins; fallback to any incoming edge. Snapshot = in the newest trace for the policy (`GET /api/debug/traces?policy=<p>&limit=1`, then the detail), the step whose `node_id` equals the predecessor id → its `after`; when the predecessor is the `listener` (or the node is first), `trace.initial`. Body values walk backwards through `unchanged: true` steps to the last step carrying `text`.
- Suggestion model = catalog statics with values mapped from the snapshot (`uri` → `request.path`, `status` → `response.status_code`, ...) **plus** discovered family members: one entry per actual request header (`$http_<h>`), query param (`$arg_<q>`), message key (`$msg_<k>`), response header (`$sent_http_<h>`). Cookie and form families list as names-only with a caveat (cookies are redacted at capture; form fields need `capture_bodies`).
- Insertion rule: names containing characters outside `[A-Za-z0-9_]` (e.g. dotted `msg_` keys) insert as `${name}`; others as `$name`. This encodes the resolver's real tokenization rule.
- Availability states (returned by the hook, rendered by the popover/legend): `debug-off`, `no-incoming-edge`, `no-trace`, `bodies-off`, `supernode-definition` (names-only), `ok`.
- Values display: single line, truncated ~80 chars, `<redacted>` shown verbatim.

### 4. UI — components

- **`VarInput`** (`ui/src/components/VarInput.tsx`): controlled input/textarea wrapper used by SchemaForm when a field schema has `vars: true`. Typing `$` or `${` opens a popover anchored under the field: substring-filtered list (filter = the token after `$`), ↑/↓/Enter/Tab/Esc keyboard handling, mouse click to insert, each row = mono var name + dimmed value preview (when available). A footer row shows the availability state or a "Context vars ⓘ" link opening the legend. No new npm dependencies — plain positioned div.
- **SchemaForm integration**: `FieldSchema` gains `vars?: boolean`; the four render sites (text, textarea, list items, objects sub-fields) use `VarInput` when set. Flag the verified interpolating fields in `ui/src/pluginConfig.ts`: logger `log_format` values, `redirect.uri`, `limit-count.key`, `limit-conn.key`, `proxy-cache.cache_key` items, `mocking.response_example` + `response_headers` values, `exit-transformer.body`, `forward-auth.extra_headers` values, `fault-injection` abort body/headers, `traffic-label` set_headers/set_labels values, `body-transformer.template`, `lago.event_transaction_id`/`subscription_id`, `response-rewrite.add_headers`/`set_headers` values. Env-var/secret fields stay unflagged.
- **`VarLegend`** (`ui/src/components/VarLegend.tsx`): a dialog opened from a "Context vars" button in the NodeInspector header (and from the VarInput popover footer). Contents: the full catalog grouped (request / response / message / families), descriptions + insertion examples, the `${...}`-for-dotted-keys rule, redaction & `capture_bodies` caveats, and live values inline when a snapshot is available for the selected node.
- **Prop threading**: `GraphCanvas` computes the predecessor id for the selected node (it owns `edges`) and passes `policyName`, `predecessorId`, `debugConfig`, and `kind` down to `NodeInspector`; `App` passes `debugConfig` to `GraphCanvas`.

### 5. Out of scope (V1)

- Raw JSON editor autocomplete; expression-rule (`[var, op, value]`) builders; `real-ip.source` (bare-name field).
- Multi-predecessor merge: with fan-in, the success-edge predecessor wins (first by edge order); no attempt to union contexts.
- Live values inside the supernode-definition editor (names-only there).
- Any change to redaction: `<redacted>` is displayed as-is; cookies remain unpreviewable by design.

### 6. Testing

- Rust: unit tests for `sent_http_*` (case/underscore mapping, multi-value join, absent header) and `request_body` (utf8, lossy, empty); catalog drift-guard test (both directions); `/api/vars` handler test (shape + auth mounting verified like other admin routers).
- UI: build + lint gates (no unit test infra).
- e2e (Playwright): API check of `/api/vars` (families present); browser scenario — seed policy with `mocking`, send a request with the debug trigger header, select the mocking node, focus `response_example`, type `$http_`, assert the dropdown lists a real header with its live value; assert `$` + Enter inserts the var text; open the legend and assert families render. Names-only path: with a fresh policy that has no traces, assert the popover shows the "no trace yet" state.
- Docs: new `website/docs/reference/context-vars.md` (generated content mirrors the catalog: full var table + rules/caveats), linked from `guides/debugging.md` and the plugins that interpolate; roadmap + CLAUDE.md bullet.

## Critical files

| Area | Files |
|---|---|
| Engine vars | `src/vars/mod.rs`, `src/vars/catalog.rs` (new) |
| Endpoint | `src/admin/vars.rs` (new), `src/admin/mod.rs` |
| UI data | `ui/src/api/client.ts` (listTraces filters + listVars), `ui/src/varSuggestions.ts` (new), `ui/src/types/index.ts` |
| UI components | `ui/src/components/VarInput.tsx` (new), `ui/src/components/VarLegend.tsx` (new), `ui/src/components/SchemaForm.tsx`, `ui/src/components/NodeInspector.tsx`, `ui/src/components/GraphCanvas.tsx`, `ui/src/App.tsx`, `ui/src/pluginConfig.ts` |
| E2E/docs | `e2e/tests/`, `e2e/E2E_TESTBOOK.md`, `website/docs/reference/context-vars.md` |

## Verification

- `cargo test` green (new var + catalog + endpoint tests); fmt/clippy clean
- `cd ui && npm run build` green; `cargo build --release && cd e2e && npm test` green incl. the new browser scenario
- Manual: run the gateway with `FEATHERBIT_DEBUG=true FEATHERBIT_DEBUG_TRACE_ALL=true`, send a request, open a var-flagged field, type `$` — dropdown shows discovered headers/args with values; legend lists everything with caveats
