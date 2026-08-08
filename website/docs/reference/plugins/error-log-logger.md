---
title: error-log-logger
description: An error-only access logger that ships entries to a remote TCP sink when a request accumulated errors.
---

<span className="plugin-chip" style={{'--chip-color': '#ef4444'}}>error-log-logger</span>

An **error-only access logger**: it builds the shared access-log entry and ships it to a remote **TCP** sink **only when the request accumulated errors** (`context.errors` is non-empty). Requests that did not fail produce nothing.

:::caution Denials and throttles are not errors
`context.errors` holds one record per node that **could not do its job** — upstream unreachable, an IdP callout that failed, a counter store that is down, input the node cannot parse. Deliberate responses are not in it. A request that exits on an [outcome port](../../concepts/policies-and-graphs.md#outcome-ports-and-the-mandatory-wiring-rule) — an auth denial (`denied`), a throttle (`limited`), an open circuit breaker (`broken`), an injected abort (`abort`), a redirect (`redirect`) — carries **no** error record, so **this node emits nothing for it**. That is by design: those are the gateway working as configured.

To ship a record of denials or throttles, place a regular access logger (`logging`, `http-logger`, ...) on the path those outcome ports take.
:::

Delivery is **fire-and-forget**. Place this node in the response pipeline **after the `upstream` node** (and after any node whose errors you want captured).

## Configuration

| Key | Type | Default | Description |
|---|---|---|---|
| `host` | string | — (**required**) | TCP sink hostname or IP. |
| `port` | integer | — (**required**) | TCP sink port (0–65535). |
| `timeout` | integer (s) | `3` | Connect/send timeout. |
| `level` | string | `"WARN"` | Accepted for config compatibility. featherbit logs on the *presence* of `context.errors`, not on a syslog severity threshold. Must be one of `STDERR`, `EMERG`, `ALERT`, `CRIT`, `ERR`, `ERROR`, `WARN`, `NOTICE`, `INFO`, `DEBUG`. |
| `tls` | bool | `false` | **Not yet supported** — `true` is rejected at config load. |
| `log_format` | object | — | Custom `name -> "$var"` entry; replaces the default structured entry. |
| `batch_max_size` | integer | `1000` | Entries per batch; `1` flushes every entry immediately. |
| `inactive_timeout` | integer (s) | `5` | Flush when idle this long. |
| `buffer_duration` | integer (s) | `60` | Flush when the oldest buffered entry is this old. |
| `max_retry_count` | integer | `0` | Retries after a failed flush. |
| `retry_delay` | integer (s) | `1` | Delay between retries. |
| `max_pending_entries` | integer | `10000` | Queue capacity; entries are dropped (with a warning) when full. |

```yaml
- id: error-log
  type: error-log-logger
  config:
    host: 127.0.0.1
    port: 5044
```

## Behavior

When `context.errors` is non-empty, builds the shared access-log entry (which includes the `errors` array) and pushes it to the batch sink as one newline-delimited JSON object. When there are no errors, nothing is emitted — including for the deliberate outcome exits listed above. The node is a pure passthrough: only its **success** port is ever taken.

## Behavior notes

- **Source of logs**: featherbit logs **request-level errors** (`context.errors`) rather than the gateway's own internal error-log lines — featherbit has no internal error-log file to tail. Note that this is narrower than APISIX's error log, which also records rejections written by plugins: here a deliberate rejection is an outcome, not an error, and is not captured.
- Only the **tcp** sink (`host`/`port`) is implemented; the `skywalking`, `clickhouse`, and `kafka` sinks are out of scope for this socket-focused node.
- `level` is accepted but not enforced as a severity threshold.
- `tls` is **not yet supported**; `tls: true` is rejected at config load.
