---
title: error-handler
description: Turn accumulated gateway errors into a templated JSON error response.
---

<span className="plugin-chip" style={{'--chip-color': '#ef4444'}}>error-handler</span>

Overwrites `context.response` with a configured status code and a body rendered from a template. It is typically wired to the `error` ports of other nodes (`upstream`, auth plugins, `rate-limit`, ...) so failed requests produce a controlled response instead of the raw error.

## Configuration

All keys are optional; the constructor never fails.

| Key | Type | Default | Description |
|---|---|---|---|
| `status_code` | integer | `500` | HTTP status of the error response. |
| `body_template` | string | `{"error": "internal_error", "message": "An unexpected error occurred"}` | Response body. May reference the most recent entry in `context.errors` through placeholders. |

Supported placeholders, substituted at execution time from the **last** error in `context.errors`:

- `{{error.code}}` — the error code (e.g. `UPSTREAM_CONNECTION_ERROR`, `RATE_LIMITED`)
- `{{error.message}}` — the human-readable error message
- `{{error.node_id}}` — the id of the node that raised the error

```yaml
type: error-handler
config:
  status_code: 502
  body_template: '{"error": "{{error.code}}", "message": "{{error.message}}"}'
```

## Behavior

On execution the plugin:

1. Renders `body_template`, replacing the placeholders with fields from `context.errors.last()`. If the errors list is empty, the template is emitted verbatim (placeholders included).
2. Sets `context.response.status_code` to the configured value, replaces the response body with the rendered template, and forces the `content-type` response header to `application/json`.

It always succeeds and exits through the `success` port — the `error` port is never taken, and no error codes are emitted. It reads `context.errors` but never appends to it, and does not touch `context.request` or `context.message`.

**UI editor note:** the node inspector form offers a `content_type` field, but the plugin does not read that key — the response content type is always `application/json`.
