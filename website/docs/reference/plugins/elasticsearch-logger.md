---
title: elasticsearch-logger
description: Batches access-log entries and ships them to Elasticsearch via the _bulk API.
---

<span className="plugin-chip" style={{'--chip-color': '#14b8a6'}}>elasticsearch-logger</span>

Builds one access-log entry per request and hands it to a fire-and-forget batching sink; a background task POSTs batches to Elasticsearch's [`_bulk`](https://www.elastic.co/guide/en/elasticsearch/reference/current/docs-bulk.html) API as newline-delimited JSON. Place it in the response pipeline, after the `upstream` node, so the final status and body size are captured. The node passes the context through unchanged and never fails.

## Configuration

| Key | Type | Default | Description |
|---|---|---|---|
| `endpoint_addr` | string | — | Elasticsearch base URL, e.g. `http://es:9200`. One of `endpoint_addr`/`endpoint_addrs` is **required**. |
| `endpoint_addrs` | array&lt;string&gt; | — | Multiple base URLs; one is chosen per flush (round-robin). |
| `field.index` | string | — | Destination index name. **Required**. |
| `field.type` | string | — | Accepted for compatibility; not emitted (see Behavior notes). |
| `auth.username` | string | — | HTTP Basic auth username (with `auth.password`). |
| `auth.password` | string | — | HTTP Basic auth password. |
| `ssl_verify` | bool | `true` | Verify TLS certificates for `https` endpoints. |
| `timeout` | integer (s) | `10` | Per-flush HTTP deadline. |
| `include_req_body` | bool | `false` | Include the request body in the default entry. |
| `include_resp_body` | bool | `false` | Include the response body in the default entry. |
| `log_format` | object | — | Custom flat entry of `name -> "$var template"` (replaces the default structured entry). |
| `batch_max_size` | integer | `1000` | Entries per batch before an immediate flush. |
| `inactive_timeout` | integer (s) | `5` | Flush after this much idle time. |
| `buffer_duration` | integer (s) | `60` | Flush when the oldest buffered entry is this old. |
| `max_retry_count` | integer | `0` | Retries after a failed flush. |
| `retry_delay` | integer (s) | `1` | Delay between retries. |
| `max_pending_entries` | integer | `10000` | Queue capacity; entries are dropped (with a warning) when full. |

```yaml
- id: es-log
  type: elasticsearch-logger
  config:
    endpoint_addr: http://es:9200
    field:
      index: services
    auth:
      username: elastic
      password: ${ES_PASSWORD}
    ssl_verify: true
    timeout: 10
    batch_max_size: 1000
```

## Behavior

Each flush POSTs `<endpoint>/_bulk` with `Content-Type: application/x-ndjson`. The body pairs an action line `{"index":{"_index":<name>}}` with each entry line, both newline-terminated. When `auth` is set, an `Authorization: Basic <base64>` header is added. A non-`200` response fails the batch, which is retried per the batch settings.

## Behavior notes

- **No Elasticsearch version probe.** featherbit targets ES 7+ and never emits `_type` in the action line, so it performs no version-probe callout; `field.type` is accepted but ignored.
- **Static index name.** Because entries are flushed in batches without a request context, `field.index` is used as a literal string — `{time}` strftime tokens and `$var` references are not resolved.
