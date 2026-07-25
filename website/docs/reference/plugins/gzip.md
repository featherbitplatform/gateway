---
title: gzip
description: Compress response bodies with gzip when the client accepts it and the response matches the configured types and size.
---

<span className="plugin-chip" style={{'--chip-color': '#0ea5e9'}}>gzip</span>

Compresses `context.response.body` with gzip. It is a response-phase node — place it after `upstream` (and after any body-mutating node such as `response-rewrite`), before `client`. The node performs both the request's `Accept-Encoding` check and the compression itself, in process.

## Configuration

All keys are optional.

| Key | Type | Default | Description |
|---|---|---|---|
| `types` | array of strings, or `"*"` | `["text/html"]` | Response content types to compress. `"*"` matches any. The response `content-type` is compared with parameters (`;charset=...`) stripped. A response without a `content-type` header is never compressed. |
| `min_length` | integer ≥ 1 | `20` | Bodies shorter than this many bytes are not compressed. |
| `comp_level` | integer 1-9 | `1` | gzip compression level. |
| `vary` | bool | `false` | When true, appends `Vary: Accept-Encoding` to compressed responses. |

```yaml
type: gzip
config:
  types: ["text/html", "application/json"]
  min_length: 20
  comp_level: 6
  vary: true
```

Out-of-range levels, `min_length: 0`, an empty `types` array, or a `types` string other than `"*"` are rejected at config load.

## Behavior

This plugin never fails at execution time — it always exits through the `success` port. The response passes through **unchanged** when any of these hold:

- the request's `Accept-Encoding` does not allow gzip — the token (or `*`) must be listed with a non-zero `q` value; a missing header means no compression;
- the response already carries a `content-encoding` (never compress twice, and never compress opaque pre-encoded bytes — even for encodings featherbit does not know);
- the response `content-type` is absent or does not match `types`;
- the body is shorter than `min_length`.

Otherwise the body is gzip-compressed at `comp_level`, `content-encoding: gzip` is set, the stale `content-length` is removed (the server layer recomputes it from the compressed body), and `Vary: Accept-Encoding` is appended when `vary` is set. A codec failure logs a warning and leaves the response untouched.

Behavior notes: `http_version` and `buffers` are not supported (featherbit responses are fully buffered), and `min_length` is compared against the actual buffered body length rather than the upstream `Content-Length` header. Unlike the `brotli` node, the gzip node does not weaken `ETag` headers.

The plugin does not read or write `context.message` or `context.errors`.
