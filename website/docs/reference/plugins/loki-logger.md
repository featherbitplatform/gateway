---
title: loki-logger
description: Ships access logs to a Grafana Loki push API in batches.
---

<span className="plugin-chip" style={{'--chip-color': '#f97316'}}>loki-logger</span>

Builds a JSON access-log entry for each request/response and ships accumulated batches to a Grafana Loki push endpoint. Each batch is grouped into a single Loki stream carrying the configured labels, with one `[nanosecond_timestamp, json_line]` value per entry, and POSTed to `<endpoint>/loki/api/v1/push`. Place this node in the response pipeline, **after the upstream node**.

## Configuration

| Key | Type | Default | Description |
|---|---|---|---|
| `endpoint_addrs` | array | — (**required**) | Loki base addresses (e.g. `http://loki:3100`); one is chosen per flush. A single-string `endpoint` is also accepted. |
| `endpoint_uri` | string | `/loki/api/v1/push` | Push path appended to each base address. |
| `tenant_id` | string | `fake` | Sent as the `X-Scope-OrgID` header. |
| `log_labels` | object | `{job: featherbit}` | Loki stream labels. `labels` is accepted as an alias. |
| `headers` | object | `{}` | Extra request headers. |
| `ssl_verify` | bool | `false` | Verify TLS certificates. |
| `timeout` | integer (ms) | `3000` | Whole-call deadline per flush. |
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
  type: loki-logger
  config:
    endpoint_addrs: [http://loki:3100]
    tenant_id: my-org
    log_labels:
      job: featherbit
      env: prod
    batch_max_size: 1000
```

## Behavior

The node is a pure passthrough: it never modifies the context and never fails, so only its **success** port is ever taken. `push` is fire-and-forget and never blocks the request path — when the queue is full, entries are dropped with a `tracing::warn!`. Delivery, batching, timing, and retries all run on a background task. Note: label values are static (no per-request `$var` resolution), so a batch forms a single stream.
