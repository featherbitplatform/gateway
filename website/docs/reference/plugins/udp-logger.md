---
title: udp-logger
description: Ships per-request access-log entries to a remote UDP endpoint as JSON datagrams.
---

<span className="plugin-chip" style={{'--chip-color': '#22c55e'}}>udp-logger</span>

Ships a JSON access-log entry for each request/response to a remote **UDP** endpoint. Entries are buffered in a batch sink and delivered by a background task that binds an ephemeral UDP socket and sends each entry as one datagram of JSON bytes to `host:port`.

Delivery is **fire-and-forget**: `execute` builds the entry, hands it to the sink, and returns the context unchanged. UDP itself is best-effort — a datagram the kernel accepts is considered delivered. Place this node in the response pipeline **after the `upstream` node**.

## Configuration

| Key | Type | Default | Description |
|---|---|---|---|
| `host` | string | — (**required**) | UDP server hostname or IP. |
| `port` | integer | — (**required**) | UDP server port (0–65535). |
| `timeout` | integer (s) | `3` | Per-datagram send timeout. |
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
- id: udp-log
  type: udp-logger
  config:
    host: 127.0.0.1
    port: 5140
```

## Behavior

Builds the shared access-log entry, pushes it to the batch sink, and passes the context through unchanged. The node is a pure passthrough: only its **success** port is ever taken. On a partial batch failure, entries already sent are not resent — only the undelivered tail is retried.

## Behavior notes

- Each entry is sent as its **own datagram** (one JSON object per packet), regardless of the batch size.
