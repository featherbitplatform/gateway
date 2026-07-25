---
title: splunk-hec-logging
description: Ships access logs to a Splunk HTTP Event Collector in batches.
---

<span className="plugin-chip" style={{'--chip-color': '#65a30d'}}>splunk-hec-logging</span>

Builds a JSON access-log entry for each request/response, wraps it in a Splunk HEC event envelope (`{time, source, sourcetype, event}`), and ships accumulated batches to a Splunk HTTP Event Collector. Batches are POSTed as concatenated JSON events with an `Authorization: Splunk <token>` header. Place this node in the response pipeline, **after the upstream node**.

## Configuration

| Key | Type | Default | Description |
|---|---|---|---|
| `endpoint.uri` | string | — (**required**) | HEC collector URL, e.g. `https://splunk:8088/services/collector`. A top-level `uri` is also accepted. |
| `endpoint.token` | string | — (**required**) | HEC token, sent as `Authorization: Splunk <token>`. |
| `endpoint.channel` | string | — | Sent as the `X-Splunk-Request-Channel` header. |
| `endpoint.timeout` | integer (s) | `10` | Whole-call deadline per flush. |
| `source` | string | `featherbit-splunk-hec-logging` | HEC event `source` field. |
| `ssl_verify` | bool | `true` | Verify TLS certificates. |
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
  type: splunk-hec-logging
  config:
    endpoint:
      uri: https://splunk:8088/services/collector
      token: 00000000-0000-0000-0000-000000000000
    ssl_verify: true
    batch_max_size: 1000
```

## Behavior

The node is a pure passthrough: it never modifies the context and never fails, so only its **success** port is ever taken. `push` is fire-and-forget and never blocks the request path — when the queue is full, entries are dropped with a `tracing::warn!`. Each entry becomes a HEC event `{time, source, sourcetype: "_json", event}`; a batch is sent as the events concatenated with no separator (the format HEC expects). Delivery, batching, timing, and retries all run on a background task.
