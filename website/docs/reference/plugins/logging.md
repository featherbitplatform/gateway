---
title: logging
description: Emits a structured JSON access-log line for the current request/response.
---

<span className="plugin-chip" style={{'--chip-color': '#6b7280'}}>logging</span>

Emits a structured JSON access-log record for the current request/response via `tracing` (target `access_log`), then passes the context through unchanged. Typically placed in the response pipeline, after the upstream node, so the final status and body size are available.

## Configuration

| Key | Type | Default | Description |
|---|---|---|---|
| `include_headers` | bool | `false` | Also log request and response headers. |
| `include_body` | bool | `false` | Reserved flag: parsed and stored but currently not acted on — only the response body *size* is logged, never its content. |

All keys are optional; this node's configuration never fails to parse.

```yaml
- id: access-log
  type: logging
  config:
    include_headers: true
```

Note: the UI node editor form also offers `level` and `format` fields — the plugin ignores both. Records are always logged at `info` level as JSON. The `include_headers` switch is honored.

## Behavior

Logs one JSON object at `info` level under the `access_log` target with these fields:

- `method`, `path`, `host`, `remote_addr` — from `context.request`
- `status`, `response_body_bytes` — from `context.response`
- `request_headers`, `response_headers` — only when `include_headers` is `true`
- `errors` — the accumulated `context.errors` array, only when non-empty

The node is a pure passthrough: it never modifies the context and never fails, so only its **success** port is ever taken. It reads `context.request`, `context.response`, and `context.errors`; it writes nothing.
