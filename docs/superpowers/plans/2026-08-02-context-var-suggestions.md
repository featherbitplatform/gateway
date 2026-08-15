# Context Var Suggestions Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Autocomplete `$var` references with live value previews (from the latest debug trace, per-node) in the UI's plugin config editors, plus a var legend — and close the engine's context-coverage gaps (`$sent_http_<name>`, `$request_body`).

**Architecture:** Two additive match arms in the resolver; a drift-guarded var catalog (`src/vars/catalog.rs`) served at `GET /api/vars`; UI-side suggestion building from the catalog + the predecessor node's trace snapshot (existing `/api/debug/traces` endpoints, now called with the already-supported `?policy=&limit=` filters); a from-scratch `VarInput` popover attached to schema fields flagged `vars: true`; a `VarLegend` dialog.

**Tech Stack:** Rust (no new deps), React 19 (no new deps), Playwright.

**Spec:** `docs/superpowers/specs/2026-08-02-context-var-suggestions-design.md` — read it first.

## Global Constraints

- Conventional Commits, **no Co-Authored-By trailer**. Branch `feature/context-var-suggestions` (exists, off develop).
- No new Rust or npm dependencies. The popover is a plain positioned `<div>`.
- Insertion rule: names containing characters outside `[A-Za-z0-9_]` insert as `${name}`; others as `$name` (mirrors `interpolate()`'s tokenizer at `src/vars/mod.rs:148`).
- Names always; live values only when debug enabled + incoming edge + trace for the policy contains the predecessor. Unavailable states are explained, never blank. `<redacted>` is displayed verbatim.
- V1 scope guard: NO autocomplete in the raw JSON editor, expression-rule fields, or `real-ip.source`; no multi-predecessor merge (success edge wins); names-only in the supernode-definition editor.
- Every Rust task: targeted test filter, then full `cargo test`, `cargo fmt --check`, `cargo clippy --all-targets --locked -- -D warnings` — green at every commit; paste actual outputs. UI tasks: `cd ui && npm run build` + `npm run lint`.

---

### Task 1: Engine vars — `sent_http_<name>` + `request_body`

**Files:**
- Modify: `src/vars/mod.rs` (resolve() match ~line 50-111, doc comment ~line 28-49, tests module at bottom)

**Interfaces:**
- Produces: `resolve(ctx, "sent_http_<name>")` → first response-header value (`_`→`-`, case-insensitive like `http_*`); `resolve(ctx, "request_body")` → lossy UTF-8 of the request body. Task 2's catalog lists both.

- [ ] **Step 1: Write the failing tests** — append inside `src/vars/mod.rs`'s existing `#[cfg(test)] mod tests` (read the existing tests first and reuse their context-builder helper; if they build `Context` literally, follow that pattern):

```rust
    #[test]
    fn test_sent_http_resolves_response_header() {
        let mut ctx = test_ctx(); // reuse/adapt the module's existing context helper
        ctx.response
            .headers
            .insert("x-cache-status".to_string(), vec!["HIT".to_string(), "second".to_string()]);
        assert_eq!(
            resolve(&ctx, "sent_http_x_cache_status").as_deref(),
            Some("HIT"),
            "underscore->dash mapping and first-value pick must mirror http_*"
        );
        assert!(resolve(&ctx, "sent_http_missing").is_none());
    }

    #[test]
    fn test_request_body_lossy_utf8() {
        let mut ctx = test_ctx();
        ctx.request.body = bytes::Bytes::from_static(b"hello=world");
        assert_eq!(resolve(&ctx, "request_body").as_deref(), Some("hello=world"));

        ctx.request.body = bytes::Bytes::from_static(&[0xff, 0x61]);
        assert_eq!(resolve(&ctx, "request_body").as_deref(), Some("\u{fffd}a"));
    }

    #[test]
    fn test_interpolate_sent_http_and_request_body() {
        let mut ctx = test_ctx();
        ctx.response.headers.insert("x-id".to_string(), vec!["42".to_string()]);
        ctx.request.body = bytes::Bytes::from_static(b"B");
        assert_eq!(interpolate(&ctx, "h=$sent_http_x_id b=$request_body"), "h=42 b=B");
    }
```

NOTE: if the tests module has no reusable context helper, add one small `fn test_ctx() -> Context` following how `src/graph/engine.rs` tests construct a `Context` (empty maps, `Bytes::new()` bodies, `Protocol::Http1`).

- [ ] **Step 2: Run to verify failure** — `cargo test vars` → the new tests FAIL (`sent_http_*` and `request_body` resolve to `None`).

- [ ] **Step 3: Implement.** In `resolve()`:

(a) Static arm after `resp_body` (line ~83):

```rust
        "request_body" => Some(Cow::Owned(
            String::from_utf8_lossy(&ctx.request.body).into_owned(),
        )),
```

(b) Family branch in the `_ =>` chain, placed BEFORE the `http_` branch (`sent_http_` also starts with... it does not — but place it before `http_` anyway for symmetry and to avoid any future prefix shadowing; note `strip_prefix("http_")` would not match `sent_http_...` since it doesn't start with `http_`, so order is safe either way):

```rust
            } else if let Some(header) = name.strip_prefix("sent_http_") {
                let header = header.replace('_', "-").to_lowercase();
                ctx.response
                    .headers
                    .get(&header)
                    .and_then(|v| v.first())
                    .map(|v| Cow::Borrowed(v.as_str()))
```

(c) Extend the `resolve()` doc comment's list with the two new names (match its existing phrasing, e.g. `- \`sent_http_<name>\` — first response header value (underscores map to dashes)` and `- \`request_body\` — request body (lossy UTF-8)`).

- [ ] **Step 4: Run tests** — `cargo test vars` → PASS; full `cargo test`, `cargo fmt --check`, `cargo clippy --all-targets --locked -- -D warnings` → green.

- [ ] **Step 5: Commit**

```bash
git add src/vars/mod.rs
git commit -m "feat(vars): add sent_http_<name> and request_body variables"
```

---

### Task 2: Var catalog — `src/vars/catalog.rs` + drift guard

**Files:**
- Create: `src/vars/catalog.rs`
- Modify: `src/vars/mod.rs` (add `pub mod catalog;` — note vars is currently a single-file module `src/vars/mod.rs`, so the new file sits next to it and is declared there)

**Interfaces:**
- Produces: `crate::vars::catalog::{VarEntry, VarKind, var_catalog()}`:

```rust
pub enum VarKind { Static, Family }
pub struct VarEntry {
    pub name: &'static str,               // "uri" | "http_*"
    pub kind: VarKind,
    pub family_source: Option<&'static str>, // families only: "request_headers"|"query_params"|"cookies"|"form_body"|"message"|"response_headers"
    pub description: &'static str,
    pub example: &'static str,            // "$http_user_agent"
}
pub fn var_catalog() -> Vec<VarEntry>
```

Task 3 serializes it; the UI mirrors the shape.

- [ ] **Step 1: Create the file** with module doc, the catalog, serde-friendly serialization helper, and the drift test:

```rust
//! Machine-readable catalog of every variable [`super::resolve`] supports —
//! the single source the Admin API (`GET /api/vars`), the UI autocomplete,
//! and the var legend consume. Guarded against drift from the resolver by
//! `test_catalog_matches_resolver`, which parses `resolve()`'s source.

use serde::Serialize;

/// Whether an entry is a fixed name or a `prefix_*` family.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum VarKind {
    Static,
    Family,
}

/// One catalog row. `family_source` names the context collection that
/// populates a family's live suggestions in the UI.
#[derive(Debug, Serialize)]
pub struct VarEntry {
    pub name: &'static str,
    pub kind: VarKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub family_source: Option<&'static str>,
    pub description: &'static str,
    pub example: &'static str,
}

const S: VarKind = VarKind::Static;
const F: VarKind = VarKind::Family;

fn e(
    name: &'static str,
    kind: VarKind,
    family_source: Option<&'static str>,
    description: &'static str,
    example: &'static str,
) -> VarEntry {
    VarEntry { name, kind, family_source, description, example }
}

/// Every variable `resolve()` accepts, statics first, then families.
pub fn var_catalog() -> Vec<VarEntry> {
    vec![
        e("uri", S, None, "Request path (no query string)", "$uri"),
        e("request_uri", S, None, "Path plus ?query when query params exist", "$request_uri"),
        e("method", S, None, "HTTP method (alias: request_method)", "$method"),
        e("request_method", S, None, "HTTP method (alias of method)", "$request_method"),
        e("host", S, None, "Request Host", "$host"),
        e("scheme", S, None, "http or https", "$scheme"),
        e("protocol", S, None, "HTTP protocol version (http1, http2, ...)", "$protocol"),
        e("remote_addr", S, None, "Client IP without port", "$remote_addr"),
        e("remote_port", S, None, "Client port", "$remote_port"),
        e("query_string", S, None, "Full query string, rebuilt and sorted", "$query_string"),
        e("status", S, None, "Response status code", "$status"),
        e("resp_body", S, None, "Response body (lossy UTF-8)", "$resp_body"),
        e("request_body", S, None, "Request body (lossy UTF-8)", "$request_body"),
        e("consumer_name", S, None, "Authenticated consumer name (set by auth plugins)", "$consumer_name"),
        e("consumer_group_id", S, None, "Authenticated consumer group", "$consumer_group_id"),
        e("arg_*", F, Some("query_params"), "First value of a query parameter", "$arg_page"),
        e("http_*", F, Some("request_headers"), "First value of a request header (underscores map to dashes)", "$http_user_agent"),
        e("cookie_*", F, Some("cookies"), "Value from the Cookie request header", "$cookie_session"),
        e("post_arg_*", F, Some("form_body"), "Form field from an application/x-www-form-urlencoded body", "$post_arg_username"),
        e("msg_*", F, Some("message"), "Any context.message key, stringified; dotted keys need ${msg_key.with.dots}", "${msg_consumer.name}"),
        e("sent_http_*", F, Some("response_headers"), "First value of a response header (underscores map to dashes)", "$sent_http_content_type"),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    /// The catalog must track resolve()'s source exactly, both directions.
    /// Statics are quoted names on match-arm lines containing "=>"; families
    /// are the strip_prefix("...") literals. Same source-parsing guard style
    /// as KNOWN_PLUGIN_TYPES.
    #[test]
    fn test_catalog_matches_resolver() {
        let src = include_str!("mod.rs");
        // Limit the scan to resolve()'s body: from `pub fn resolve` to the
        // next `pub fn` after it.
        let start = src.find("pub fn resolve").expect("resolve fn present");
        let rest = &src[start..];
        let end = rest[10..].find("pub fn ").map(|i| i + 10).unwrap_or(rest.len());
        let body = &rest[..end];

        let mut from_source: BTreeSet<String> = BTreeSet::new();
        for line in body.lines() {
            let t = line.trim();
            if t.contains("=>") {
                // every quoted token on an arm line is a static var name
                let mut s = t;
                while let Some(open) = s.find('"') {
                    let after = &s[open + 1..];
                    if let Some(close) = after.find('"') {
                        let name = &after[..close];
                        if !name.is_empty()
                            && name.chars().all(|c| c.is_ascii_lowercase() || c == '_')
                        {
                            from_source.insert(name.to_string());
                        }
                        s = &after[close + 1..];
                    } else {
                        break;
                    }
                }
            }
            if let Some(idx) = t.find("strip_prefix(\"") {
                let after = &t[idx + 14..];
                if let Some(close) = after.find('"') {
                    from_source.insert(format!("{}*", &after[..close]));
                }
            }
        }
        // message_str constants referenced by consumer arms appear as quoted
        // strings on arm lines ("consumer.name"/"consumer.group") — they are
        // lookup keys, not var names; strip them.
        from_source.remove("consumer.name");
        from_source.remove("consumer.group");

        let from_catalog: BTreeSet<String> =
            var_catalog().iter().map(|v| v.name.to_string()).collect();

        assert_eq!(from_catalog, from_source, "catalog drifted from resolve()");
    }

    #[test]
    fn test_catalog_families_have_sources_and_statics_do_not() {
        for v in var_catalog() {
            match v.kind {
                VarKind::Family => {
                    assert!(v.family_source.is_some(), "{} missing family_source", v.name);
                    assert!(v.name.ends_with("_*"), "{} family name must end in _*", v.name);
                }
                VarKind::Static => assert!(v.family_source.is_none(), "{}", v.name),
            }
        }
    }
}
```

CAVEAT for the implementer: the drift test's quoted-token scan will also pick up `"-"` / `"_"` literals if `replace('_', "-")` ever appears on an arm line — the `all(lowercase|_)` + non-empty filter excludes those. If the test's source-parse produces surprising extras, print the sets and adjust the FILTER (never the catalog) so it isolates exactly the arm names + strip_prefix families; the two consumer.* removals show the pattern.

- [ ] **Step 2: Wire + verify failure** — add `pub mod catalog;` at the top of `src/vars/mod.rs` (after the imports). Run `cargo test catalog_matches` → the drift test should PASS immediately if the catalog above is complete; force one temporary red run by commenting out any entry (e.g. `status`), observing the assert fire, then restoring it — paste both outputs (this proves the guard actually guards).

- [ ] **Step 3: Run gates** — full `cargo test`, fmt, clippy → green. (serde is already a dependency; `Serialize` on the catalog compiles without new crates. If clippy flags `var_catalog` as dead code — Task 3 consumes it — add the repo-convention `#[allow(dead_code)]` + `// consumed by GET /api/vars (next task)` comment and REMOVE it in Task 3.)

- [ ] **Step 4: Commit**

```bash
git add src/vars/catalog.rs src/vars/mod.rs
git commit -m "feat(vars): machine-readable var catalog with resolver drift guard"
```

---

### Task 3: `GET /api/vars` endpoint

**Files:**
- Create: `src/admin/vars.rs`
- Modify: `src/admin/mod.rs` (`mod vars;` + `.merge(vars::router())` inside the authed block)

**Interfaces:**
- Produces: `GET /api/vars` → `{ "vars": [ {name, kind, family_source?, description, example}, ... ] }`. UI Task 4 consumes it.

- [ ] **Step 1: Create `src/admin/vars.rs`:**

```rust
//! Admin API endpoint serving the context-variable catalog
//! (src/vars/catalog.rs) — consumed by the UI's autocomplete and var legend.

use std::sync::Arc;

use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router};

use crate::state::SharedState;

/// Builds the router for `/api/vars`.
pub fn router() -> Router<Arc<SharedState>> {
    Router::new().route("/api/vars", get(list_vars))
}

/// `GET /api/vars` — the static catalog of `$var` names plugins can
/// interpolate, with kinds, family sources, and descriptions. Always `200 OK`.
async fn list_vars() -> impl IntoResponse {
    Json(serde_json::json!({ "vars": crate::vars::catalog::var_catalog() }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    #[tokio::test]
    async fn test_list_vars_shape() {
        // Stateless handler; a state-free router instance suffices.
        let app: Router = Router::new().route("/api/vars", get(list_vars));
        let resp = app
            .oneshot(Request::get("/api/vars").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let vars = v["vars"].as_array().unwrap();
        assert!(vars.iter().any(|e| e["name"] == "uri" && e["kind"] == "static"));
        assert!(vars
            .iter()
            .any(|e| e["name"] == "http_*"
                && e["kind"] == "family"
                && e["family_source"] == "request_headers"));
        assert!(vars.iter().any(|e| e["name"] == "sent_http_*"));
        assert!(vars.iter().any(|e| e["name"] == "request_body"));
    }
}
```

- [ ] **Step 2: Verify failure** — `cargo test list_vars` → COMPILE ERROR (module not declared). Mount: `mod vars;` in `src/admin/mod.rs` + `.merge(vars::router())` after the other merges INSIDE the authed block (before the auth layer). Remove Task 2's temporary `#[allow(dead_code)]` if one was added.

- [ ] **Step 3: Run gates** — `cargo test list_vars` PASS; full suite + fmt + clippy green.

- [ ] **Step 4: Commit**

```bash
git add src/admin/vars.rs src/admin/mod.rs src/vars/catalog.rs
git commit -m "feat(admin): serve the var catalog at GET /api/vars"
```

---

### Task 4: UI — types, client, suggestion engine (`varSuggestions.ts`)

**Files:**
- Modify: `ui/src/types/index.ts`, `ui/src/api/client.ts`
- Create: `ui/src/varSuggestions.ts`

**Interfaces:**
- Produces (consumed by Tasks 5-6):

```ts
// types/index.ts
export interface VarEntry { name: string; kind: 'static' | 'family'; family_source?: string; description: string; example: string; }

// api/client.ts
listVars: () => Promise<VarEntry[]>                       // GET /api/vars, unwrapped
listTraces: (filter?: { policy?: string; limit?: number }) => Promise<TraceSummary[]>  // now passes query params

// varSuggestions.ts
export type Availability = 'ok' | 'debug-off' | 'no-incoming-edge' | 'no-trace' | 'supernode-definition';
export interface Suggestion { name: string; insert: string; group: string; description?: string; value?: string; note?: string; }
export function insertionText(name: string): string;      // $name or ${name} per the tokenizer rule
export function predecessorSnapshot(trace: TraceDetail, predecessorId: string | null): ContextSnapshot | null;
export function bodyText(trace: TraceDetail, stepIndex: number, which: 'request' | 'response'): string | null; // walks unchanged:true backwards
export function buildSuggestions(catalog: VarEntry[], snapshot: ContextSnapshot | null, capturedBodies: boolean): Suggestion[];
export function useContextSuggestions(args: { policyName: string | null; nodeId: string | null; predecessorId: string | null; kind: 'policy' | 'supernode'; debugEnabled: boolean; captureBodies: boolean; }): { suggestions: Suggestion[]; availability: Availability; catalog: VarEntry[] };
```

- [ ] **Step 1: Types + client.** `VarEntry` interface in `ui/src/types/index.ts` (doc-commented, mirrors `src/vars/catalog.rs::VarEntry`). In `ui/src/api/client.ts`: add `listVars` (`request<{ vars: VarEntry[] }>('/api/vars').then(r => r.vars)`), and change `listTraces` to `listTraces: (filter?: { policy?: string; limit?: number }) => { const q = new URLSearchParams(); if (filter?.policy) q.set('policy', filter.policy); if (filter?.limit) q.set('limit', String(filter.limit)); const qs = q.toString(); return request<{ traces: TraceSummary[] }>(`/api/debug/traces${qs ? '?' + qs : ''}`).then(r => r.traces); }` — backward compatible (existing DebugPanel callers pass no args; verify with the build).

- [ ] **Step 2: Create `ui/src/varSuggestions.ts`** — pure functions + the hook. Key logic (write real code following these exact rules; module doc comment explaining the data flow):

- `insertionText(name)`: `/^[A-Za-z0-9_]+$/.test(name) ? '$' + name : '${' + name + '}'`.
- `predecessorSnapshot(trace, predecessorId)`: `predecessorId === null || predecessorId === undefined` → `trace.initial`; if the predecessor is of the listener (the caller passes `null` for listener predecessors); else `trace.steps.find(s => s.node_id === predecessorId)?.after ?? null`.
- `bodyText(trace, stepIndex, which)`: starting at `stepIndex`, walk backwards through `steps[i].after[which === 'request' ? 'request' : 'response'].body` while `unchanged` is true and `text` is absent, ending at `trace.initial` — return the first `text` found or null. (Export it for testing even though `buildSuggestions` consumes snapshots; the hook resolves body text before calling buildSuggestions and injects them — simplest: `buildSuggestions` takes optional `requestBodyText`/`responseBodyText` args instead. Choose ONE approach and keep the signature list above updated in code comments.)
- `buildSuggestions(catalog, snapshot, capturedBodies)`:
  - statics: map to snapshot fields — `uri`→`request.path`, `request_uri`→path+(`?`+sorted query rebuild) when params exist, `method`/`request_method`→`request.method`, `host`, `scheme`, `status`→`String(response.status_code)`, `query_string`→sorted `k=v&...` rebuild or note "empty", `consumer_name`→`message['consumer.name']`, `consumer_group_id`→`message['consumer.group']`, `resp_body`/`request_body`→ the provided body texts or `note: capturedBodies ? 'no body captured yet' : "enable debug.capture_bodies"`; `protocol`/`remote_addr`/`remote_port`→ `note: 'not captured in traces'` (snapshots don't carry them — verified against ContextSnapshot).
  - families with discovered members (only when snapshot present): request.headers → one suggestion per header `http_<name with - → _>` valued at first element; response.headers → `sent_http_<...>`; query_params → `arg_<k>`; message keys → `msg_<k>` (value = JSON-stringified if not a string, single-line truncated ~80 chars).
  - families without members: `cookie_*` always gets a single family row with `note: 'cookies are redacted in traces — values never previewable'`; `post_arg_*` a family row noted "form-urlencoded bodies only" + parsed members when the request body text exists AND `request.headers['content-type']` starts with `application/x-www-form-urlencoded`.
  - without a snapshot: statics with no values, family rows with no members.
  - groups: `request` | `response` | `message` (used by the legend's sections and the popover's ordering).
- `useContextSuggestions(...)`: module-level catalog cache (`let catalogPromise: Promise<VarEntry[]> | null`); state for suggestions/availability. Effects: fetch catalog once; when (`kind === 'supernode'`) → availability `supernode-definition`, suggestions = names-only; else if `!debugEnabled` → `debug-off`, names-only; else if `predecessorId === undefined` (no incoming edge — caller encodes: `undefined` = no edge, `null` = listener predecessor) → `no-incoming-edge`, names-only; else fetch `api.listTraces({ policy: policyName, limit: 1 })` → empty → `no-trace`; else `api.getTrace(id)` → snapshot via `predecessorSnapshot` (missing predecessor step → `no-trace`) → `ok` + full suggestions. Re-runs when `nodeId` changes. Errors (network/404) degrade to `no-trace` silently.

- [ ] **Step 3: Verify + commit**

Run: `cd ui && npm run build && npm run lint` → green (nothing renders yet; this is the data layer).

```bash
git add ui/src/types/index.ts ui/src/api/client.ts ui/src/varSuggestions.ts
git commit -m "feat(ui): var catalog client and trace-derived suggestion engine"
```

---

### Task 5: UI — `VarInput` + SchemaForm integration + field flags

**Files:**
- Create: `ui/src/components/VarInput.tsx`
- Modify: `ui/src/components/SchemaForm.tsx`, `ui/src/pluginConfig.ts`

**Interfaces:**
- Consumes: `Suggestion`, `Availability` (Task 4).
- Produces: `VarInput` props `{ value: string; onChange: (v: string) => void; placeholder?: string; multiline?: boolean; rows?: number; suggestions: Suggestion[]; availability: Availability; onOpenLegend: () => void; style?: React.CSSProperties }`. `SchemaFormProps` gains optional `varContext?: { suggestions: Suggestion[]; availability: Availability; onOpenLegend: () => void }`. `FieldSchema` gains `vars?: boolean` (also honored on `item` and on `objects` sub-fields).

- [ ] **Step 1: `VarInput.tsx`.** A controlled `<input>`/`<textarea>` with a suggestion popover. Behavior contract (implement exactly):
- Token detection: on every change/selection, take `value.slice(0, selectionStart)` and match `/\$\{?([A-Za-z0-9_.]*)$/`; a match opens the popover with the captured prefix as filter (case-insensitive substring over `Suggestion.name`); no match closes it.
- Rendering: absolutely-positioned div directly under the field (`position: relative` wrapper, popover `top: 100%; left: 0; right: 0; zIndex: 50; maxHeight: 240px; overflowY: auto`), rows = mono name + dimmed truncated value (or italic `note`), highlighted active row, groups separated by tiny eyebrow headers. Footer row: availability message when not `ok` (exact copy — `debug-off`: "Debug is off — enable debug.enabled for live values", `no-incoming-edge`: "No incoming edge — connect this node to preview values", `no-trace`: "No trace yet — send a request through this route", `supernode-definition`: "Live values unavailable while editing a supernode definition") plus a "Context vars reference" button calling `onOpenLegend`.
- Keyboard: when open — ArrowDown/ArrowUp move, Enter/Tab insert active, Escape closes (all with `preventDefault`); when closed, keys pass through.
- Insertion: replace the matched token (`$...` through the caret) with the suggestion's `insert` text, restore focus, place the caret after the inserted text, fire `onChange` with the new full value, close the popover.
- Blur closes the popover after a ~120 ms timeout (so row clicks land first); `onMouseDown` on rows uses `preventDefault` to keep focus.
- Styling: reuse the module's field look — accept `style` and spread over the same `inputStyle` values SchemaForm uses.

- [ ] **Step 2: SchemaForm integration.** Add `vars?: boolean` to `FieldSchema` (and document it: "field accepts $var interpolation — enables context autocomplete"), plus to the `item?: {...}` descriptor type and sub-field entries in `fields`. Add the optional `varContext` prop to `SchemaFormProps`. At the four render sites, when the flag is set AND `varContext` is provided, render `VarInput` instead of the plain element:
  - `text` (~line 242): `<VarInput value={...} onChange={(v) => set(field.key, v)} placeholder={field.placeholder} style={inputStyle} {...varContext} />`
  - `textarea` (~line 266): same with `multiline rows={field.rows ?? 4}`
  - `list` items (~line 317): when `field.item?.vars`
  - `objects` sub-inputs (~line 376): when `sub.vars` (text sub-fields only)
  Plain rendering is untouched when the flag or `varContext` is absent (PluginConfigPanel passes no varContext → no popover there, by design).

- [ ] **Step 3: Flag the fields in `ui/src/pluginConfig.ts`** — add `vars: true` to exactly these verified-interpolating fields (spec §4 list): logger `log_format` value sub-fields (find the shared log_format schema definition — it is reused across logger types), `redirect.uri`, `limit-count.key`, `limit-conn.key`, `proxy-cache.cache_key` (`item.vars`), `mocking.response_example` + `response_headers` value sub-field, `exit-transformer.body`, `forward-auth.extra_headers` value sub-field, `fault-injection` abort body + header value sub-field, `traffic-label` set_headers/set_labels value sub-fields, `body-transformer.template`, `lago.event_transaction_id` + `subscription_id`, `response-rewrite.add_headers`/`set_headers` value sub-fields. Do NOT flag `proxy-rewrite` headers (not interpolated by the plugin) or any `${ENV}` secret fields.

- [ ] **Step 4: Verify + commit**

Run: `cd ui && npm run build && npm run lint` → green. (SchemaForm callers don't pass `varContext` yet — optional prop, no build break.)

```bash
git add ui/src/components/VarInput.tsx ui/src/components/SchemaForm.tsx ui/src/pluginConfig.ts
git commit -m "feat(ui): VarInput autocomplete popover on var-accepting schema fields"
```

---

### Task 6: UI — `VarLegend` + wiring through NodeInspector/GraphCanvas/App

**Files:**
- Create: `ui/src/components/VarLegend.tsx`
- Modify: `ui/src/components/NodeInspector.tsx`, `ui/src/components/GraphCanvas.tsx`, `ui/src/App.tsx`

**Interfaces:**
- Consumes: `useContextSuggestions`, `Suggestion`, `Availability` (Task 4), `varContext` prop (Task 5).
- Produces: end-to-end feature: `App` passes `debugConfig` to `GraphCanvas`; `GraphCanvas` computes `predecessorId` and passes `policyName`, `predecessorId`, `debugConfig`, `kind` to `NodeInspector`; `NodeInspector` runs the hook, threads `varContext` into `SchemaForm`, renders the legend dialog and a "Context vars" header button.

- [ ] **Step 1: `VarLegend.tsx`.** A `Dialog`-based (reuse `components/Dialog.tsx`) reference panel: props `{ open: boolean; onClose: () => void; catalog: VarEntry[]; suggestions: Suggestion[]; availability: Availability }`. Content: intro line (syntax: `$name`, `${name}` required for names with dots — cite `$msg_` keys), grouped tables (Request / Response / Message & consumer / Families) listing name, description, example, and — when availability is `ok` — the live value column from `suggestions`; caveats box (redaction list summary: auth/cookie/token headers show `<redacted>`; cookies never previewable; bodies need `capture_bodies`; values come from the latest request). Static content derives from `catalog`, not hardcoded rows.

- [ ] **Step 2: GraphCanvas.** Add props `debugConfig: DebugConfig | null`. Compute for the selected node: `const incoming = edges.filter(e => e.target === selectedNodeId); const successEdge = incoming.find(e => e.sourceHandle !== 'error') ?? incoming[0]; const predecessorId = successEdge === undefined ? undefined : (nodes.find(n => n.id === successEdge.source)?.data as PluginNodeData | undefined)?.pluginType === 'listener' || (nodes.find(n => n.id === successEdge.source)?.data as PluginNodeData | undefined)?.pluginType === 'input' ? null : successEdge.source;` (undefined = no incoming edge; null = predecessor is the pipeline entry → use trace.initial). Pass `policyName={policy?.name ?? null}`, `predecessorId`, `debugConfig`, `kind` to `NodeInspector`.

- [ ] **Step 3: NodeInspector.** New props (`policyName`, `predecessorId`, `debugConfig`, `kind`). Call `useContextSuggestions({ policyName, nodeId: node?.id ?? null, predecessorId, kind, debugEnabled: debugConfig?.enabled ?? false, captureBodies: debugConfig?.capture_bodies ?? false })`. Local state `legendOpen`. Header gains a small "Context vars" icon button (e.g. lucide `Braces`, aria-label="Context vars reference") next to the close button. Pass `varContext={{ suggestions, availability, onOpenLegend: () => setLegendOpen(true) }}` to `SchemaForm`. Render `<VarLegend open={legendOpen} onClose={...} catalog={catalog} suggestions={suggestions} availability={availability} />`. The hook must not run network calls for `isFixed`/`isSupernode` nodes (guard: skip fetching when the selected node type is listener/client/supernode/boundary — pass `nodeId: null` in that case).

- [ ] **Step 4: App.** Pass `debugConfig={debugConfig}` to `<GraphCanvas ...>` (both props already in scope).

- [ ] **Step 5: Verify + commit**

Run: `cd ui && npm run build && npm run lint` → green. Manual smoke (optional here, mandatory in Task 8): run the gateway with `FEATHERBIT_DEBUG=true FEATHERBIT_DEBUG_TRACE_ALL=true`, hit a route, type `$` in `mocking.response_example`.

```bash
git add ui/src/components/VarLegend.tsx ui/src/components/NodeInspector.tsx ui/src/components/GraphCanvas.tsx ui/src/App.tsx
git commit -m "feat(ui): var legend and per-node live preview wiring"
```

---

### Task 7: E2E scenario + testbook

**Files:**
- Create: `e2e/tests/var-suggestions.spec.ts`
- Modify: `e2e/E2E_TESTBOOK.md`

**Interfaces:**
- Consumes: e2e helpers (`adminApi`, `dataPlane`, `deleteRouteIfPresent` — read `e2e/helpers/admin.ts`), browser idioms from `e2e/tests/editor.spec.ts` (read it: how tests open the UI, select nodes, interact — reuse its `page` patterns and selectors), the debug trigger header usage from `e2e/tests/debug.spec.ts`.

- [ ] **Step 1: Write the spec.** Scenarios:

```
E2E-VS-01 (API): GET /api/vars returns the catalog — assert 200; vars array
contains {name:'uri',kind:'static'}, {name:'http_*',kind:'family',family_source:'request_headers'},
'sent_http_*', 'request_body'; no duplicates.

E2E-VS-02 (browser): live suggestions with values.
  Setup via API: policy 'vs-policy' = listener -> mock(mocking, response_example 'hello') -> client;
  route 'vs-route' path /vs/*. Send GET /vs/ping with header 'x-vs-probe: live-value-42'
  and the debug trigger header (copy its name from debug.spec.ts).
  In the browser: open the UI, select route vs-route, click the mock node,
  focus the response_example field (it has vars:true), type '$http_x_vs'.
  Assert the popover lists an option containing 'http_x_vs_probe' AND 'live-value-42'.
  Press Enter; assert the field value now contains '$http_x_vs_probe'.
  Open the legend via the header button; assert it shows the 'Families' group and
  the ${...} dotted-keys rule text; close it.

E2E-VS-03 (browser): names-only state. Create policy 'vs-cold' + route never traffic'd
  (or reuse vs-policy after DELETE /api/debug/traces to clear the buffer).
  Select the mock node, type '$'; assert the popover renders catalog names
  (e.g. '$uri') and the footer shows the no-trace message
  ('No trace yet — send a request through this route').
```

Write real Playwright code following editor.spec.ts's login/navigation idioms; add stable hooks if the DOM needs them (e.g. `data-testid="var-popover"` on the popover container and `data-testid="var-legend"` on the legend dialog — adding those test ids to VarInput/VarLegend is in scope for this task; keep them minimal).

- [ ] **Step 2: Run** — `cargo build --release` (UI embedded — required), `cd e2e && npx playwright test var-suggestions.spec.ts`; then FULL `npm test`. Debug failures by reading the Playwright trace/output; the backend endpoints are all reviewed working, so failures are most likely selector/timing — prefer `await expect(locator)...` auto-waits over sleeps.

- [ ] **Step 3: Testbook** — add "Var suggestions" section (E2E-VS-01..03) in the existing table format.

- [ ] **Step 4: Commit**

```bash
git add e2e/tests/var-suggestions.spec.ts e2e/E2E_TESTBOOK.md ui/src
git commit -m "test(e2e): context var autocomplete, live preview, and legend scenarios"
```

---

### Task 8: Docs + final verification

**Files:**
- Create: `website/docs/reference/context-vars.md`
- Modify: `website/sidebars.ts` (reference section), `website/docs/guides/debugging.md` (cross-link), `website/docs/reference/roadmap.md`, `CLAUDE.md`

- [ ] **Step 1: Reference page** — `context-vars.md`: the full var table (statics + families, mirroring the catalog including `sent_http_*`/`request_body`), syntax rules (`$name`, `${name}` for dotted keys, no escape, unknown vars → empty string), which plugins/fields interpolate (the spec §4 field list), the autocomplete/legend feature (when values appear: debug + incoming edge + latest request; redaction and `capture_bodies` caveats). Front-matter/tone like other reference pages.
- [ ] **Step 2: Cross-links + roadmap + CLAUDE.md** — `guides/debugging.md` gains a short "Live value preview while editing" subsection linking to the reference; roadmap marks the feature shipped; CLAUDE.md Core features bullet: `- **Context var autocomplete** — $var suggestions with live value preview from debug traces, plus GET /api/vars catalog`.
- [ ] **Step 3: Builds** — `cd website && npm run build` green.
- [ ] **Step 4: Final verification** — full `cargo test`, fmt, clippy; `cd ui && npm run build && npm run lint`; `cargo build --release && cd e2e && npm test` (full suite); manual sweep: gateway with `FEATHERBIT_DEBUG=true FEATHERBIT_DEBUG_TRACE_ALL=true FEATHERBIT_DEBUG_BODIES=true`, send a request with query params + custom headers, verify dropdown values incl. `$arg_*`, `$sent_http_*`, body vars, legend caveats.
- [ ] **Step 5: Commit docs; then superpowers:requesting-code-review + superpowers:finishing-a-development-branch (PR → develop).**

```bash
git add website CLAUDE.md
git commit -m "docs: context vars reference, debugging guide link, roadmap"
```
