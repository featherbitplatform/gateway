---
title: set-vars
description: Compute named values from the live context once and store them in context.message for downstream nodes to reuse.
---

<span className="plugin-chip" style={{'--chip-color': '#10b981'}}>set-vars</span>

Computes one or more named values from the live [`Context`](../../concepts/context-object.md) and stores them under `context.message`, so any node placed later in the graph — or a policy author reading the same context in Lua, or the debug trace viewer — can read them back as `{{message.<name>}}`. It exists to let a policy **compose a value once** (e.g. derive a tenant id from a header, or build a composite key from several fields) instead of repeating the same `{{...}}` expression in every node that needs it.

It is a native node, not ported from Apache APISIX — featherbit has no upstream equivalent to mirror here.

## Configuration

| Key | Type | Default | Description |
|---|---|---|---|
| `vars` | map or array | — (required) | The variables to compute. Accepts the YAML-authored map form (`{name: value}`) **and** the UI editor's array-of-records form (`[{name, value}]`, the "Variables" list with **Name**/**Value** sub-fields). At least one entry is required. |

```yaml
type: set-vars
config:
  vars:
    tenant: "{{request.headers.x-tenant-id}}"
    request_key: "{{request.method}}:{{request.path}}"
```

Each value is a **`{{...}}` template** ([Templates](../templates.md)), rendered at this node's position in the graph — a `request.*`/`response.*`/`client.*` reference sees the context as it stands when this node executes, and `env.*` resolves at config load like everywhere else. The Value field is `template: 'full'` in the UI, so its popover offers the complete `request`/`response`/`message`/`client`/`env` suggestion set with live preview, the same as `proxy-rewrite`'s `add_headers` value.

Rejected at config load:

- No `vars` key, or an empty map/array.
- A name outside the `{{message.<name>}}`-safe charset — alphanumeric, `_`, `.`, `-` only (anything else could never be referenced back via a template path or offered by autocomplete).
- A duplicate name within the array form.
- A value that isn't a string, number, or bool (numbers and bools are stringified; nested objects/arrays are rejected).

## Behavior

Runtime-infallible: `set-vars` always exits through the `success` port; the `error` port is never taken.

Two passes, in this order:

1. **Render** every configured template against the context **as it entered this node** — before any of this node's own writes happen.
2. **Apply** all rendered results to `context.message`, keyed by the raw name.

The two-pass shape means **entries are independent: one var can never reference another var set by the same node.** Given `vars: {a: "hello", b: "{{message.a}}"}`, `b` renders against the context from *before* `a` was written, so `{{message.a}}` sees no `message.a` key yet and renders empty — not `"hello"`. If you need one derived value to build on another, split them across two `set-vars` nodes wired in sequence (or reuse an upstream node's own output the normal way).

A var with the same name as an existing `context.message` key **overwrites** it — last-write-wins, same as every other node that writes `context.message`.

### Reading a var back downstream

Once set, a var is visible everywhere `{{message.<name>}}` resolves:

- Any later node's templated config field, e.g. `proxy-rewrite`'s `add_headers`:
  ```yaml
  type: proxy-rewrite
  config:
    add_headers:
      x-tenant: "{{message.tenant}}"
  ```
- A `script` node's Lua, via `ctx.message["<name>"]` (or the legacy `$msg_<name>` var, if the field you're writing to is one of the 15 that still supports it — see [Templates](../templates.md#legacy-var-interop)).
- The web UI's inspector popover and the expanded template editor modal, as a `message.<name>` suggestion row with a live preview — once a request carrying real data has flowed through the policy and been captured by [debug mode](../../guides/debugging.md).
- A [debug trace](../../guides/debugging.md)'s per-node context snapshot, under `message.<name>`, for any step at or after this node.

## UI editor note

The node inspector form covers exactly the `vars` key, as a "Variables" objects list with **Name** (plain text, the raw key) and **Value** (full `{{...}}` template support) sub-fields — the array-of-records shape `SetVarsPlugin::from_config` accepts alongside the YAML map form.
