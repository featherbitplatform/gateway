---
title: error-handler
description: Turn accumulated gateway errors into a templated JSON error response.
---

<span className="plugin-chip" style={{'--chip-color': '#ef4444'}}>error-handler</span>

Overwrites `context.response` with a configured status code and a body rendered from a template. It is typically wired to the `error` ports of other nodes (`upstream`, an auth plugin's IdP-callout failure, a `limit-count` counter-backend outage, ...) so genuinely *failed* requests produce a controlled response instead of the raw error. Deliberate rejections do not come here — they leave their node's own outcome port with the response already prepared.

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

1. **Checks `context.errors`.** If it is **empty**, the context is passed through **completely unchanged** — no template render, no status rewrite, no header change (see below).
2. Otherwise renders `body_template`, replacing the placeholders with fields from `context.errors.last()`.
3. Sets `context.response.status_code` to the configured value, replaces the response body with the rendered template, and forces the `content-type` response header to `application/json`.

### Outcome exits pass through untouched

A context reaching this node with **no** error record is left exactly as it arrived. That is the case for every **outcome** exit — `denied`, `limited`, `broken`, `abort`, `redirect`, `preflight`, `routed`, `hit`. Those are not failures: the emitting node did its job and already prepared the client-facing response, so there is no error to report and nothing for the handler to add. Were it not for this guard, such a response would be replaced by the raw, unsubstituted template (or by the default 500) — exactly what the [named-output-ports model](../../concepts/policies-and-graphs.md#outcome-ports-and-the-mandatory-wiring-rule) exists to prevent.

Wire outcome ports **straight to `client`**. Routing one through `error-handler` is harmless but pointless; to reshape a denial or throttle response, wire its port through a response-shaping node (`response-rewrite`, `error-page`, or `exit-transformer` with `always: true`) instead.

It always succeeds and exits through the `success` port — the `error` port is never taken, and no error codes are emitted. It reads `context.errors` but never appends to it, and does not touch `context.request` or `context.message`.

**UI editor note:** the node inspector form offers a `content_type` field, but the plugin does not read that key — the response content type is always `application/json`.
