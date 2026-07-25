---
title: skywalking-logger
description: Ships access-log entries to a SkyWalking OAP endpoint in batches.
---

<span className="plugin-chip" style={{'--chip-color': '#FF6D00'}}>skywalking-logger</span>

Ships a structured access-log entry for each request to an [Apache SkyWalking](https://skywalking.apache.org) OAP (Observability Analysis Platform) HTTP endpoint. Entries are buffered by a batch processor and POSTed in the background as a JSON array to `<endpoint_addr>/v3/logs`. The node passes the context through unchanged, so place it in the response pipeline after the upstream node.

## Configuration

| Key | Type | Default | Description |
|---|---|---|---|
| `endpoint_addr` | string | — (required) | SkyWalking OAP HTTP base, e.g. `http://127.0.0.1:12800`. |
| `service_name` | string | `featherbit` | SkyWalking service name reported on each log item. |
| `service_instance_name` | string | `featherbit Instance Name` | SkyWalking service instance name. |
| `ssl_verify` | bool | `true` | Verify the OAP TLS certificate. |
| `timeout` | int (seconds) | `3` | Per-flush HTTP timeout. |
| `log_format` | object | — | Custom `name -> "$var template"` map. When set, it replaces the default entry. |
| `include_req_body` / `include_resp_body` | bool | `false` | Include the (lossy UTF-8) request/response body in the default entry. |

`endpoint_addr` is required or config load fails. Batch-processor keys (`batch_max_size`, `inactive_timeout`, `buffer_duration`, `max_retry_count`, `retry_delay`, `max_pending_entries`) are also accepted.

```yaml
- id: skywalking-log
  type: skywalking-logger
  config:
    endpoint_addr: http://127.0.0.1:12800
    service_name: my-gateway
    service_instance_name: gw-1
```

## Behavior

For each request the node builds a log entry (the shared default entry, or a `log_format` custom entry) and wraps it in a SkyWalking `LogData` item:

```json
{
  "service": "<service_name>",
  "serviceInstance": "<service_instance_name>",
  "endpoint": "<request path>",
  "timestamp": 1700000000000,
  "body": { "json": { "json": "<the entry, JSON-encoded as a string>" } }
}
```

Items are buffered and the batch is POSTed as a JSON array to `<endpoint_addr>/v3/logs`. The node is a pure passthrough: it never modifies the context and only its **success** port is taken. Delivery is best-effort — a full queue drops items with a warning and failed flushes are retried per the batch config.

## Behavior notes

- featherbit does not thread trace propagation into the log entry, so no `traceContext` is attached to items.
- A millisecond `timestamp` (SkyWalking's `LogData.timestamp`) is stamped at build time. The entry is embedded under `body.json.json`.
