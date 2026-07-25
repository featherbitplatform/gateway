---
title: echo
description: Replace or wrap the response body and set response headers — a demo and debugging utility.
---

<span className="plugin-chip" style={{'--chip-color': '#22c55e'}}>echo</span>

Rewrites the response: `body` replaces the upstream body, `before_body` / `after_body` are concatenated around it, and `headers` are set on the response. It is a **response-phase** node — place it after `upstream` (between `upstream` and `client`) so it sees the upstream response. It is intended as a demo/debugging utility.

## Configuration

At least one of `body` / `before_body` / `after_body` is required.

| Key | Type | Default | Description |
|---|---|---|---|
| `body` | string | — | Replaces the upstream response body. |
| `before_body` | string | — | Prepended to the body (after any `body` replacement). |
| `after_body` | string | — | Appended to the body. |
| `headers` | map or array | `{}` | Response headers to set. Accepts the map form `{name: value}` **and** the array form `[{name, value}]` (the shape the UI editor saves; blank-name rows are skipped). Values must be scalars (strings, numbers, bools — stringified); names are lowercased. Existing values for the same header are **replaced**, not appended (like `ngx.header`). |

```yaml
type: echo
config:
  before_body: "before the body modification "
  after_body: " after the body modification"
  headers:
    x-served-by: featherbit
```

Malformed shapes are rejected at config load: non-string body keys, a non-map `headers`, or non-scalar header values all fail with a descriptive error.

## Behavior

This plugin never fails at execution time — it always exits through the `success` port, and the `error` port is never taken.

On each response, in order:

1. With `body`, the upstream body is discarded and replaced. Without it, the upstream body is kept — and if it is compressed (`content-encoding: gzip`, `deflate`, or `br`), it is **decoded first** so `before_body`/`after_body` concatenate onto text rather than onto a compressed stream. An unsupported encoding or corrupt stream falls back to the raw bytes.
2. `before_body` and `after_body` are concatenated around the result.
3. Per the body-mutation convention, `content-length` is removed (the server recomputes it from the final body) and `content-encoding` is removed (the body is left decoded).
4. `headers` are set on `context.response.headers`, replacing any existing values.
