---
title: set-vars
description: Derive variables from the context — a path segment, a header, a query parameter, a JSON body field — via templates, JSONPath and regex captures, and store them for downstream nodes.
---

<span className="plugin-chip" style={{'--chip-color': '#0ea5e9'}}>set-vars</span>

Pulls values out of the request context and stores them in `context.message`, where every downstream node can read them as `$msg_<name>` (legacy `$var` interpolation) or `{{message.<name>}}` (universal templates). Each variable is derived by up to three steps: a **source** template, an optional **JSONPath** applied to the source parsed as JSON, and an optional **regex capture**. Featherbit-native (no APISIX equivalent); it replaces the small Lua `script` you would otherwise write to extract a path segment or a body field.

## Configuration

Every regex and JSONPath is compiled at config load: a malformed one fails policy compilation, not requests. Unknown keys are rejected.

| Key | Type | Default | Description |
|---|---|---|---|
| `vars` | array | **required**, non-empty | Evaluated in order; each entry stores one variable. |
| `vars[].name` | string | **required** | Message key. Letters, digits, `_`, `-`, `.`; unique within the node. Read it as `$msg_<name>` / `{{message.<name>}}`. |
| `vars[].from` | string | `$request_body` when `json_path` is set, else **required** | Source text: any `$var` (`$uri`, `$http_<header>`, `$arg_<param>`, `$cookie_<name>`, `$msg_<key>`, …) or `{{namespace.path}}` template, literal text allowed. |
| `vars[].json_path` | string | — | RFC 9535 JSONPath applied to `from` parsed as JSON. One scalar node → its text; one object/array → its JSON text; several nodes → a JSON array text; none → `default`. |
| `vars[].regex` | string | — | Regex applied to the text after `json_path`. |
| `vars[].group` | integer or string | `1` | Which capture becomes the value: an index (`0` = whole match) or a named group. Needs `regex`. |
| `vars[].default` | string | — | Used when the source is empty, is not JSON, the path matches nothing, or the regex does not match. Without it the value is `""`. |

```yaml
type: set-vars
config:
  vars:
    - name: user                     # /hello/frenk → "frenk"
      from: $uri
      regex: '^/hello/([^/]+)'
      default: stranger
    - name: tenant                   # plain copy of a header
      from: $http_x_tenant
    - name: plan                     # query parameter with a fallback
      from: $arg_plan
      default: free
    - name: order_id                 # JSON body field (source defaults to the request body)
      json_path: $.order.id
    - name: minor                    # named capture group
      from: $http_x_api_version
      regex: '^(?P<major>\d+)\.(?P<minor>\d+)'
      group: minor
    - name: greeting                 # later entries see earlier ones
      from: 'hello $msg_user'
```

## Behavior

For each entry, in order: render `from` against the current context (so an entry can use `$msg_<name>` set by a previous entry), then apply `json_path`, then `regex`/`group`. The result is stored as a string under `context.message[name]`; an empty result falls back to `default` (or `""`). The node is always pass-through: it returns `Ok` in every case, the Context flows out the **`success` port**, and the `error` port is never taken.

Typical follow-ups: a `mocking` node answering `hello $msg_user`, a `proxy-rewrite` adding `x-tenant: $msg_tenant` to the upstream request, a `condition` branching on `["msg_plan", "==", "free"]`, or any logger's `log_format` including `$msg_order_id`. Values are visible in a debug trace as `message.<name>` in the step's `changes`.

## Behavior notes

- JSONPath sources must be valid JSON; a non-JSON source resolves to `default`. Use `from: $http_<header>` with `json_path` to read a JSON-valued header.
- A regex without capture groups selects the whole match by default.
- Absent variables render as the empty string, so `from: $arg_plan` with no `plan` parameter yields `default`.
- Values are strings; a JSON number `42` is stored as `"42"`.
