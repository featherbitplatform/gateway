---
title: loggly
description: Ships access logs to SolarWinds Loggly in batches via the HTTP bulk endpoint.
---

<span className="plugin-chip" style={{'--chip-color': '#dc2626'}}>loggly</span>

Builds a JSON access-log entry for each request/response and ships accumulated batches to SolarWinds Loggly. Each batch is POSTed as newline-delimited JSON to `https://<host>/bulk/<customer_token>/tag/<tags>/`. Place this node in the response pipeline, **after the upstream node**.

:::note Limitations
Only the HTTP/S bulk path is implemented; RFC5424 syslog framing, `severity`/`severity_map`, and a syslog-over-UDP transport are not. `severity` is accepted for config compatibility but ignored.
:::

## Configuration

| Key | Type | Default | Description |
|---|---|---|---|
| `customer_token` | string | — (**required**) | Loggly customer token; forms part of the bulk URL. |
| `tags` | array | `[featherbit]` | Loggly tags, comma-joined into the URL and the `X-LOGGLY-TAG` header. |
| `host` | string | `logs-01.loggly.com` | Loggly host; a bare host gets an `https://` scheme. |
| `severity` | string | `INFO` | Accepted for compatibility; ignored in HTTP bulk mode. |
| `ssl_verify` | bool | `true` | Verify TLS certificates. |
| `timeout` | integer (ms) | `5000` | Whole-call deadline per flush. |
| `log_format` | object | — | Custom flat entry of `name -> "$var template"`. |
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
  type: loggly
  config:
    customer_token: 00000000-0000-0000-0000-000000000000
    tags: [featherbit, prod]
    ssl_verify: true
    batch_max_size: 1000
```

## Behavior

The node is a pure passthrough: it never modifies the context and never fails, so only its **success** port is ever taken. `push` is fire-and-forget and never blocks the request path — when the queue is full, entries are dropped with a `tracing::warn!`. A batch is serialized as newline-delimited JSON (the Loggly bulk format) and POSTed to the bulk endpoint. Delivery, batching, timing, and retries all run on a background task.
