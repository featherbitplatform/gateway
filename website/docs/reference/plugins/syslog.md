---
title: syslog
description: Ships per-request access-log entries to a remote syslog server as RFC 5424 messages over TCP or UDP.
---

<span className="plugin-chip" style={{'--chip-color': '#a855f7'}}>syslog</span>

Ships a JSON access-log entry for each request/response to a remote **syslog** server, framed as RFC 5424 messages, over TCP or UDP. Each entry is JSON-encoded and wrapped in an RFC 5424 header:

```
<PRI>1 TIMESTAMP HOSTNAME APP-NAME PROCID - - {json entry}
```

The facility is `SYSLOG` (5) and severity is `INFO` (6), giving priority `5*8+6 = 46`. The hostname is the request `Host`, `APP-NAME` is `featherbit`, and `PROCID` is the gateway process id. Framed messages are buffered in a batch sink and flushed as one concatenated payload per flush.

Delivery is **fire-and-forget**. Place this node in the response pipeline **after the `upstream` node**.

## Configuration

| Key | Type | Default | Description |
|---|---|---|---|
| `host` | string | — (**required**) | Syslog server hostname or IP. |
| `port` | integer | `5140` | Syslog server port. |
| `sock_type` | `"tcp"` \| `"udp"` | `"tcp"` | Transport. |
| `timeout` | integer (ms) | `3000` | Connect/send timeout. |
| `tls` | bool | `false` | **Not yet supported** — `true` is rejected at config load. |
| `flush_limit`, `drop_limit`, `pool_size` | integer | — | Accepted for schema compatibility; **not honored** (batching is governed by the batch keys). |
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
- id: syslog
  type: syslog
  config:
    host: 127.0.0.1
    port: 5140
    sock_type: tcp
```

## Behavior

Builds the shared access-log entry, JSON-encodes it, wraps it in an RFC 5424 frame, and pushes the framed string to the batch sink. The node is a pure passthrough: only its **success** port is ever taken.

## Limitations

- `tls` is **not yet supported**; `tls: true` is rejected at config load.
- `flush_limit`, `drop_limit`, and `pool_size` are accepted but not honored; batching is governed by the shared batch keys instead.
