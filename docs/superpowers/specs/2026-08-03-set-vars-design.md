# set-vars Node + env-only Field Explanation — Design Spec

Date: 2026-08-03
Status: approved (brainstormed and validated with Francesco)

## Context

Two gaps surfaced after universal templates shipped:

1. There is no way to compute a named value once in a policy and reuse it across
   downstream nodes. `ctx.message` is exactly the right storage — free-form
   scratch space that flows between nodes, is visible to Lua, appears in debug
   traces (so autocomplete already previews `message.*` keys), and resolves as
   `{{message.<key>}}` in every engine-templated field — but only Lua scripts
   can write to it today.
2. In fields that only accept `${ENV}` (env-only), the popover silently shows
   fewer suggestions with no explanation. Users don't understand *why* context
   data isn't offered ("I only have access to env.* — why?").

Decisions made during brainstorming:

- **Reuse `message.*`** — no new Context field or template namespace. The node
  writes `ctx.message`; reads are `{{message.<name>}}`. Zero new plumbing in
  traces, Lua, redaction, suggestions, or the legend.
- **Values are templated strings** rendered at the node's position in the graph
  (plain `Template::render` — new adopter, no legacy `$` pass). No JSON-typed
  values, no expression language in V1.
- **Reach is unchanged in this spec**: vars are readable wherever `{{...}}`
  already renders per request. The follow-up sweep spec (based on
  `2026-08-03-env-only-field-audit.md`, committed alongside this spec) converts
  the ~103 GROUP-1 per-request string fields to full templating under the
  approved security rule: trust anchors, network destinations/file paths,
  secrets/signing material, and registry ids stay env-only permanently.
- **Explanatory footer ships now** with one generic message; the per-class
  copy split ("compiled at load" vs. "fixed at load for security") happens in
  the sweep spec when fields get classified.
- **Stacking**: this branch (`feature/set-vars`) depends on the Template
  engine and UI field modes from `feature/universal-templates` (PR #11); it
  stacks on that branch and its PR targets develop after #11 merges.

## Design

### 1. Plugin: `set-vars` (`src/plugins/native/set_vars.rs`, new)

```yaml
- id: mkvars
  type: set-vars
  config:
    vars:
      tenant: "{{request.headers.x-tenant-id}}"
      backend-key: "${API_KEY_PREFIX}-{{request.query.env}}"
```

- Registered in the `create_plugin` factory as `set-vars`. Category:
  transformation.
- `from_config`: `vars` is a required, non-empty JSON object of string values.
  Each **name** must match `[A-Za-z0-9_.-]+` (the template token charset) — a
  name that couldn't be referenced or autocompleted is rejected at load with a
  clear error naming the offending key. Each **value** is parsed with
  `Template::parse`; parse warnings surface through the existing load-time
  warning walk (`collect_template_warnings`), which already covers string
  leaves of node config. Scalar non-string values (number/bool) in `vars` are
  stringified like `add_headers` does; arrays/objects error at load.
- `execute`: renders each entry against the current context and inserts
  `serde_json::Value::String(rendered)` into `ctx.message` under the raw name.
  Insertion order is alphabetical by name (deterministic; entries are
  independent — one var cannot reference another var set by the same node).
  Last write wins against existing message keys, same as any plugin writing
  message keys. Runtime is infallible: always exits the `success` port; the
  `error` port exists per the node contract but never fires.
- ENV interpolation (`${NAME:-default}`) applies at config load like every
  other string field — before `Template::parse` sees the value.

### 2. UI

- `pluginMeta.tsx`: `set-vars` entry — own icon/color, "Transformation"
  category, ports `in`/`success`/`error`.
- `pluginConfig.ts`: schema for `set-vars` — an `objects` list `vars` with
  fields `name` (plain text, no template attribute → env-only by default, which
  is honest: the engine never re-renders names) and `value` (text,
  `template: 'full'`, `data-`safe placeholder like
  `{{request.headers.x-tenant-id}}`). Serialization maps the objects list to
  the YAML map shape `vars: {name: value}` the same way existing map-shaped
  fields do (follow `add_headers`' name/value objects pattern; reject duplicate
  names in the UI at save with an inline error).
- Downstream autocomplete needs zero new work: once a request flows, trace
  snapshots contain the new `message.*` keys, so every templated field
  downstream suggests `message.tenant` with its live value preview.

### 3. env-only explanatory footer

- New shared constant (single source, imported by both consumers — do not
  duplicate the string): in env-only fields the suggestion popover footer and
  the template editor modal footer show:
  > `Context data isn't available here — this value is fixed when
  > configuration loads. ${ENV} references still apply.`
  followed by the existing "Context vars reference" legend link.
- Full-template fields keep their current availability messaging unchanged.
- The legend's existing caveat text about non-templated fields is checked for
  consistency with the new copy and aligned if it drifts (the deferred
  AVAILABILITY_MESSAGE unification note from the modal feature lands here).

### 4. Testing

- Unit (`set_vars.rs` inline tests): renders-and-inserts happy path; overwrite
  of an existing message key; multiple vars in one node; invalid name rejected
  at load with the key named; empty/missing `vars` rejected; array value
  rejected; number value stringified; unknown template ref passes through
  literally (engine semantics) — and downstream read via a second node's
  templated field resolves the var.
- E2E (Playwright, new scenario in `templates.spec.ts` or a sibling spec):
  build a policy in the UI with set-vars (`tenant` =
  `{{request.headers.x-tenant-id}}`) → proxy-rewrite `add_headers` value
  `{{message.tenant}}` → send a request with the header → echo backend shows
  the composed header; assert the downstream popover suggests `message.tenant`
  with the live preview after the first request. Env-only footer scenario:
  open the popover in an env-only field (e.g. proxy-rewrite Remove headers)
  and assert the explanation text renders in the popover and in the expanded
  modal.
- Docs: plugin doc page for set-vars on the website; `reference/templates.md`
  gains a "compose once with set-vars, reuse via `{{message.*}}`" section;
  testbook entry.

## Critical files

| Area | Files |
|---|---|
| Plugin | `src/plugins/native/set_vars.rs` (new), `src/plugins/native/mod.rs`, `src/plugins/mod.rs` (factory + KNOWN_PLUGIN_TYPES) |
| UI | `ui/src/pluginMeta.tsx`, `ui/src/pluginConfig.ts`, `ui/src/components/VarInput.tsx` (footer), `ui/src/components/TemplateEditorModal.tsx` (footer), `ui/src/components/VarLegend.tsx` (copy alignment) |
| E2E/docs | `e2e/tests/`, `e2e/E2E_TESTBOOK.md`, `website/docs/` |

## Verification

- `cargo test` green; fmt/clippy clean; `ui`/`website` builds green
- `cargo build --release && cd e2e && npm test` incl. the new scenarios
- Manual: build the tenant-header policy in the UI, watch `message.tenant`
  appear in downstream suggestions with a live value; open Remove headers and
  read the explanation footer

## Follow-up (separate spec)

The GROUP-1 sweep: convert ~103 per-request string fields to `Template`
rendering + `template: 'full'` per `2026-08-03-env-only-field-audit.md`,
apply the security carve-out rule (trust anchors, destinations/paths,
secrets, registry ids stay env-only), and split the footer copy into
structural vs. security variants.
