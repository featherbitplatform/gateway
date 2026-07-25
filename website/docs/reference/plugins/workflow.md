---
title: workflow
description: Ordered traffic rules — the first matching case rejects the request or applies a fixed-window rate limit.
---

<span className="plugin-chip" style={{'--chip-color': '#8b5cf6'}}>workflow</span>

Evaluates ordered rules against each request; the first rule whose `case` matches applies its action. Supported actions: `return` (reject with a status code) and `limit-count` (fixed-window rate limiting via the shared counter store). Requests matching no rule pass through.

## Configuration

All expressions and action parameters are validated at config load; unsupported action names are rejected.

| Key | Type | Default | Description |
|---|---|---|---|
| `rules` | array | **required**, non-empty | Evaluated in order; first match wins. |
| `rules[].case` | array | match all | Triple-array condition, rules AND-ed (e.g. `[["uri", "~~", "^/admin"], ["arg_debug", "==", "1"]]`). Omit to match every request. |
| `rules[].actions` | array | **required**, non-empty | `[[name, params]]`. Only the **first** action is applied. |

### `return` action params

| Key | Type | Default | Description |
|---|---|---|---|
| `code` | integer 100–599 | **required** | Rejection status. Body is always `{"error_msg":"rejected by workflow"}`. |

### `limit-count` action params

| Key | Type | Default | Description |
|---|---|---|---|
| `count` | integer > 0 | **required** | Allowed requests per window. |
| `time_window` | number > 0 (seconds) | **required** | Fixed window length. |
| `key` | string | `"$remote_addr"` | `$var` template resolved per request into the counter key (e.g. `"$http_x_api_key"`, `"$remote_addr:$uri"`). |
| `rejected_code` | integer 200–599 | `503` | Status when the limit is exceeded. |
| `rejected_msg` | string | — | When set, the rejection body is `{"error_msg": "<msg>"}`; empty body otherwise. |
| `policy` | string | `"local"` | Counter backend name (only `local`, in-memory per instance, is available today). |

```yaml
type: workflow
config:
  rules:
    - case:
        - ["uri", "~~", "^/admin"]
      actions:
        - ["return", { "code": 403 }]
    - case:
        - ["arg_tier", "==", "free"]
      actions:
        - ["limit-count", { "count": 100, "time_window": 60, "key": "$remote_addr", "rejected_code": 429 }]
```

## Behavior

Rules are checked top to bottom; a rule without a `case` always matches. The **first matching rule wins** — its action applies and no further rules are evaluated.

- **`return` matches** — the plugin writes the rejection onto `context.response` (configured status, JSON body `{"error_msg":"rejected by workflow"}`, `content-type: application/json`) and fails with error code `WORKFLOW_REJECTED`, routing through the **`error` port**.
- **`limit-count`, within limit** — the plugin sets `x-ratelimit-limit` / `x-ratelimit-remaining` / `x-ratelimit-reset` response headers and continues through the **`success` port**.
- **`limit-count`, exceeded** — rejection written onto `context.response` (status `rejected_code`, quota headers, body from `rejected_msg` or empty) and fails with error code `RATE_LIMITED` through the **`error` port**.
- **No rule matches** — passthrough on `success`, Context untouched.

### Wiring the early exits

Rejections exit via `error` with the response **already prepared** — wire the `error` port to a pass-through path so the prepared rejection reaches the client:

```yaml
edges:
  - { from: workflow.success, to: upstream.in }  # allowed traffic continues
  - { from: workflow.error,   to: client.in }    # rejected: prepared response goes out as-is
```

Routing `error` through an `error-handler` will replace the prepared body with the handler's template.

Counters are isolated per workflow node instance and per rule, live in process memory, and reset on restart/config reload.

## Limitations

- Only the `return` and `limit-count` actions are supported.
- `limit-count`'s `key` is a `$var` template (default `"$remote_addr"`); there is no separate `key_type`, and `policy` supports only `local`.
- Quota headers are always sent (not configurable).
- Rejections route through the `error` port with codes `WORKFLOW_REJECTED` / `RATE_LIMITED` (graph-wiring mechanics).
