---
title: brotli
description: Compress response bodies with brotli when the client accepts it and the response matches the configured types and size.
---

<span className="plugin-chip" style={{'--chip-color': '#f43f5e'}}>brotli</span>

Compresses `context.response.body` with brotli. It is a response-phase node — place it after `upstream` (and after any body-mutating node such as `response-rewrite`), before `client`. Shares its gating logic (`Accept-Encoding` parsing, content-type matching, already-encoded detection) with the [`gzip`](gzip.md) node.

## Configuration

All keys are optional.

| Key | Type | Default | Description |
|---|---|---|---|
| `types` | array of strings, or `"*"` | `["text/html"]` | Response content types to compress. `"*"` matches any. The response `content-type` is compared with parameters (`;charset=...`) stripped. A response without a `content-type` header is never compressed. |
| `min_length` | integer ≥ 1 | `20` | Bodies shorter than this many bytes are not compressed. |
| `comp_level` | integer 0-11 | `6` | Brotli quality level (matches ngx_brotli's `brotli_comp_level` default). |
| `vary` | bool | `false` | When true, appends `Vary: Accept-Encoding` to compressed responses. |

```yaml
type: brotli
config:
  types: ["text/html", "application/json"]
  min_length: 20
  comp_level: 6
  vary: true
```

Out-of-range levels, `min_length: 0`, and an empty `types` array are rejected at config load.

## Behavior

This plugin never fails at execution time — it always exits through the `success` port. The response passes through **unchanged** when any of these hold:

- the request's `Accept-Encoding` does not allow `br` — the token (or `*`) must be listed with a non-zero `q` value; a missing header means no compression;
- the response already carries a `content-encoding` (never compress twice, and never compress opaque pre-encoded bytes);
- the response `content-type` is absent or does not match `types`;
- the body is shorter than `min_length`.

Otherwise the body is brotli-compressed at `comp_level`, `content-encoding: br` is set, the stale `content-length` is removed (the server layer recomputes it), and `Vary: Accept-Encoding` is appended when `vary` is set. A codec failure logs a warning and leaves the response untouched.

**ETag handling**: after compressing, a strong quoted `ETag` (`"abc"`) is downgraded to a weak one (`W/"abc"`) — the compressed bytes are no longer byte-identical to the original representation. A weak etag is kept as-is; a non-standard (unquoted) etag is dropped, since it cannot be weakened.

Limitations: `mode`, `lgwin`, `lgblock`, and `http_version` are not supported — featherbit's codec uses generic mode with its own window size, and responses are fully buffered. `min_length` is compared against the actual buffered body length rather than the upstream `Content-Length` header.

The plugin does not read or write `context.message` or `context.errors`.
