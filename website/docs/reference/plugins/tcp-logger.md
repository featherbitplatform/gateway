---
title: tcp-logger
description: Ships per-request access-log entries to a remote TCP endpoint in batches.
---

<span className="plugin-chip" style={{'--chip-color': '#0ea5e9'}}>tcp-logger</span>

Ships a JSON access-log entry for each request/response to a remote **TCP** endpoint (Logstash, Fluentd, a raw TCP collector, ...). Entries are buffered in a batch sink and delivered by a background task that opens a fresh TCP connection per flush and writes each entry as one newline-delimited JSON object.

Delivery is **fire-and-forget**: `execute` builds the entry, hands it to the sink, and returns the context unchanged — it never blocks on the network. Place this node in the response pipeline **after the `upstream` node**, so the final status, latency, and body size are available.

## Configuration

| Key | Type | Default | Description |
|---|---|---|---|
| `host` | string | — (**required**) | TCP server hostname or IP. |
| `port` | integer | — (**required**) | TCP server port (0–65535). |
| `timeout` | integer (ms) | `1000` | Connect/send timeout. |
| `tls` | bool | `false` | **Not yet supported** — `true` is rejected at config load. |
| `tls_options` | string | — | Accepted but ignored (TLS unsupported). |
| `log_format` | object | — | Custom `name -> "$var"` entry; replaces the default structured entry. |
| `include_req_body` | bool | `false` | Add the request body to the default entry. |
| `include_resp_body` | bool | `false` | Add the response body to the default entry. |
| `batch_max_size` | integer | `1000` | Entries per batch; `1` flushes every entry immediately. |
| `inactive_timeout` | integer (s) | `5` | Flush when idle this long. |
| `buffer_duration` | integer (s) | `60` | Flush when the oldest buffered entry is this old. |
| `max_retry_count` | integer | `0` | Retries after a failed flush. |
| `retry_delay` | integer (s) | `1` | Delay between retries. |
| `max_pending_entries` | integer | `10000` | Queue capacity; entries are dropped (with a warning) when full. |

```yaml
- id: tcp-log
  type: tcp-logger
  config:
    host: 127.0.0.1
    port: 5044
    timeout: 1000
    batch_max_size: 100
```

## Behavior

Builds the shared access-log entry (default structured object, or the flat `log_format` object when configured), pushes it to the batch sink, and passes the context through unchanged. The node is a pure passthrough: it never modifies the context and never fails, so only its **success** port is ever taken. A failed TCP flush is retried per the batch keys and otherwise logged and dropped.

## Behavior notes

- Entries are always sent as **newline-delimited JSON** (one object per line). The wire format is stable regardless of batching.
- `tls` / `tls_options` are **not yet supported**; `tls: true` is rejected. Plain TCP only.
