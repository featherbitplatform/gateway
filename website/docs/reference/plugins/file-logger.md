---
title: file-logger
description: Appends per-request access-log entries as newline-delimited JSON to a local file.
---

<span className="plugin-chip" style={{'--chip-color': '#f59e0b'}}>file-logger</span>

Appends a JSON access-log entry for each request/response to a local **file**. Entries are handed to a batch sink; a background task opens the target file in append mode per flush and writes each entry as one newline-delimited JSON object.

Delivery is **fire-and-forget**: `execute` builds the entry, hands it to the sink, and returns the context unchanged. Place this node in the response pipeline **after the `upstream` node**.

## Configuration

| Key | Type | Default | Description |
|---|---|---|---|
| `path` | string | — (**required**) | Target file path. Opened in append mode; created if absent. Its parent directory must already exist. |
| `log_format` | object | — | Custom `name -> "$var"` entry; replaces the default structured entry. |
| `include_req_body` | bool | `false` | Add the request body to the default entry. |
| `include_resp_body` | bool | `false` | Add the response body to the default entry. |
| `batch_max_size` | integer | `1000` | Entries per batch; set `1` for immediate per-request writes. |
| `inactive_timeout` | integer (s) | `5` | Flush when idle this long. |
| `buffer_duration` | integer (s) | `60` | Flush when the oldest buffered entry is this old. |
| `max_retry_count` | integer | `0` | Retries after a failed write. |
| `retry_delay` | integer (s) | `1` | Delay between retries. |
| `max_pending_entries` | integer | `10000` | Queue capacity; entries are dropped (with a warning) when full. |

```yaml
- id: file-log
  type: file-logger
  config:
    path: /var/log/featherbit/access.log
    batch_max_size: 1
```

## Behavior

Builds the shared access-log entry, pushes it to the batch sink, and passes the context through unchanged. The node is a pure passthrough: only its **success** port is ever taken. If the file cannot be opened or written (for example, a missing parent directory), the flush fails and is retried/dropped per the batch keys.

## Behavior notes

- Writes are routed through the shared batch sink for consistency with the other loggers. Set `batch_max_size: 1` to write every entry as it arrives.
- The parent directory is **not** created: if it is missing the flush fails. The file itself is created if absent.
