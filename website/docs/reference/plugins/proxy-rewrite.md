---
title: proxy-rewrite
description: Rewrite the request path and add or remove headers on the request or response phase.
---

<span className="plugin-chip" style={{'--chip-color': '#3b82f6'}}>proxy-rewrite</span>

Mutates either `context.request` or `context.response`, selected by `phase`: strips or adds a path prefix and adds or removes headers. It is typically placed between the `listener` and `upstream` nodes (request phase) or between `upstream` and `client` (response phase).

## Configuration

All keys are optional.

:::caution Not the same header keys as `response-rewrite`
`proxy-rewrite` uses top-level `add_headers` / `remove_headers`. The sibling [`response-rewrite`](./response-rewrite.md) instead takes a single `headers` object with `add` / `set` / `remove`. Passing `headers` here (or `add_headers` there) is now **rejected at load** with a message pointing to the right shape, rather than being silently ignored.
:::

| Key | Type | Default | Description |
|---|---|---|---|
| `phase` | string | `request` | `request` or `response`. Any value other than `response` falls back to `request`. **`add_headers`/`remove_headers` act on whichever side `phase` selects** — to change a *response* header you must set `phase: response`, otherwise the request is modified and the response is left untouched. |
| `strip_path_prefix` | string | — | Prefix removed from the request path when the path starts with it. The result is normalized to start with `/`. Request phase only. |
| `add_path_prefix` | string | — | Prefix prepended to the request path (after stripping). Request phase only. |
| `add_headers` | map or array | `{}` | Headers to append. Accepts the map form `{name: value}` **and** the array form `[{name, value}]` (the shape the UI editor saves). Scalar values (numbers, bools) are stringified. |
| `remove_headers` | array of strings | `[]` | Header names to delete. |

```yaml
type: proxy-rewrite
config:
  phase: request
  strip_path_prefix: /api/v1
  add_headers:
    x-forwarded-tier: edge
  remove_headers: [x-internal]
```

Malformed `add_headers` shapes are rejected at config load: a plain string, non-object array entries, or non-scalar values (e.g. nested objects) all fail with a descriptive error. Array entries with a blank `name` (a blank row left in the UI editor) are silently skipped.

## Behavior

This plugin never fails at execution time — it always exits through the `success` port, and the `error` port is never taken.

In the **request** phase, in order:

1. If the path starts with `strip_path_prefix`, the prefix is removed (stripping `/api/v1` from `/api/v1` yields `/`).
2. `add_path_prefix` is prepended.
3. `add_headers` are appended to `context.request.headers` — existing values for the same header are kept, the new value is added alongside them.
4. `remove_headers` are deleted.

In the **response** phase, only steps 3 and 4 run, against `context.response.headers`. Path rewriting never applies to responses.

Header names are lowercased before being applied, for both adding and removing. The plugin does not read or write `context.message` or `context.errors`.

**UI editor note:** the node inspector form covers `phase`, `strip_path_prefix`, `add_headers`, and `remove_headers`, but omits `add_path_prefix` — set that key in YAML directly.
