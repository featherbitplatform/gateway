---
title: http-logger
description: Ships access logs to an HTTP endpoint in batches.
---

<span className="plugin-chip" style={{'--chip-color': '#0ea5e9'}}>http-logger</span>

Builds a JSON access-log entry for each request/response and ships accumulated batches to an arbitrary HTTP endpoint. Entries are queued with a non-blocking, fire-and-forget push; a background task POSTs them once a batch fills, goes idle, or ages out. Place this node in the response pipeline, **after the upstream node**, so the final status and body size are captured.

## Configuration

| Key | Type | Default | Description |
|---|---|---|---|
| `uri` | string | — (**required**) | HTTP endpoint batches are POSTed to. |
| `method` | string | `POST` | HTTP method for the callout. |
| `headers` | object | `{}` | Extra request headers applied to every batch. |
| `auth_header` | string | — | Convenience for a single `Authorization` header value. |
| `concat_method` | string | `json` | `json` sends a JSON array (`application/json`); `new_line` sends `\n`-separated JSON objects (`text/plain`). |
| `ssl_verify` | bool | `false` | Verify TLS certificates for `https` endpoints. |
| `timeout` | integer (s) | `3` | Whole-call deadline per flush. |
| `log_format` | object | — | Custom flat entry of `name -> "$var template"`. When absent, the default structured entry is used. |
| `include_req_body` | bool | `false` | Add the request body to the default entry. |
| `include_resp_body` | bool | `false` | Add the response body to the default entry. |
| `batch_max_size` | integer | `1000` | Flush when the buffer reaches this many entries. |
| `inactive_timeout` | integer (s) | `5` | Flush after this idle period. |
| `buffer_duration` | integer (s) | `60` | Flush when the oldest buffered entry is this old. |
| `max_retry_count` | integer | `0` | Retries after a failed flush before dropping the batch. |
| `retry_delay` | integer (s) | `1` | Delay between retries. |
| `max_pending_entries` | integer | `10000` | Queue capacity; entries are dropped with a warning when full. |

```yaml
- id: access-log
  type: http-logger
  config:
    uri: http://log-collector:3000/logs
    method: POST
    headers:
      X-Token: secret
    concat_method: json
    batch_max_size: 1000
    inactive_timeout: 5
```

## Behavior

The node is a pure passthrough: it never modifies the context and never fails, so only its **success** port is ever taken. `push` is fire-and-forget and never blocks the request path — when the queue is full (a slow or retrying endpoint) entries are dropped with a `tracing::warn!`, which is the operator's signal to raise `max_pending_entries` or fix the downstream. Delivery, batching, timing, and retries all happen on a background task. On config reload the old sink is drained and flushed before a new one is spawned.
