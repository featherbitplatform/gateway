---
title: clickhouse-logger
description: Batches access-log entries and inserts them into ClickHouse over its HTTP interface.
---

<span className="plugin-chip" style={{'--chip-color': '#eab308'}}>clickhouse-logger</span>

Builds one access-log entry per request and hands it to a fire-and-forget batching sink; a background task POSTs batches to ClickHouse's HTTP interface as an `INSERT INTO <logtable> FORMAT JSONEachRow` statement. Place it in the response pipeline, after the `upstream` node. The node passes the context through unchanged and never fails.

## Configuration

| Key | Type | Default | Description |
|---|---|---|---|
| `endpoint_addr` | string | — | ClickHouse HTTP URL, e.g. `http://clickhouse:8123`. One of `endpoint_addr`/`endpoint_addrs` is **required**. |
| `endpoint_addrs` | array&lt;string&gt; | — | Multiple URLs; one is chosen per flush (round-robin). |
| `database` | string | — | Target database, sent as `X-ClickHouse-Database`. **Required**. |
| `logtable` | string | — | Target table, used in `INSERT INTO <logtable>`. **Required**. |
| `user` | string | `""` | ClickHouse user, sent as `X-ClickHouse-User`. |
| `password` | string | `""` | ClickHouse password, sent as `X-ClickHouse-Key`. |
| `ssl_verify` | bool | `true` | Verify TLS certificates. |
| `timeout` | integer (s) | `3` | Per-flush HTTP deadline. |
| `include_req_body` | bool | `false` | Include the request body in the default entry. |
| `include_resp_body` | bool | `false` | Include the response body in the default entry. |
| `log_format` | object | — | Custom flat entry of `name -> "$var template"`. |
| `batch_max_size` | integer | `1000` | Entries per batch before an immediate flush. |
| `inactive_timeout` | integer (s) | `5` | Flush after this much idle time. |
| `buffer_duration` | integer (s) | `60` | Flush when the oldest buffered entry is this old. |
| `max_retry_count` | integer | `0` | Retries after a failed flush. |
| `retry_delay` | integer (s) | `1` | Delay between retries. |
| `max_pending_entries` | integer | `10000` | Queue capacity; entries are dropped (with a warning) when full. |

```yaml
- id: ch-log
  type: clickhouse-logger
  config:
    endpoint_addr: http://clickhouse:8123
    database: default
    logtable: gateway_logs
    user: default
    password: ${CLICKHOUSE_PASSWORD}
    timeout: 3
```

## Behavior

Each flush POSTs the endpoint URL with `Content-Type: application/json` and the `X-ClickHouse-User`, `X-ClickHouse-Key`, and `X-ClickHouse-Database` headers. The body is `INSERT INTO <logtable> FORMAT JSONEachRow ` followed by the batch's JSON entries. A response status `>= 400` fails the batch, which is retried per the batch settings.

## Behavior notes

- Multiple encoded entries in a batch are joined with a newline, which ClickHouse's `JSONEachRow` format accepts as a row separator.
