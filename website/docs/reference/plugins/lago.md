---
title: lago
description: Meters API traffic into Lago by emitting one usage-billing event per request.
---

<span className="plugin-chip" style={{'--chip-color': '#6C5CE7'}}>lago</span>

Meters API traffic into [Lago](https://getlago.com), an open-source usage-based billing platform. Unlike the other loggers this node is a **metering** integration, not an access log: it emits **one billing event per request** so Lago can price API consumption against a customer subscription. Events are buffered by a batch processor and POSTed in the background to `<endpoint>/api/v1/events/batch` with a `Bearer <token>` header. The node passes the context through unchanged, so place it in the response pipeline after the upstream node.

## Configuration

| Key | Type | Default | Description |
|---|---|---|---|
| `endpoint` | string | — (required) | Lago API base, e.g. `http://127.0.0.1:3000`. |
| `token` | string | — (required) | Lago API key, sent as `Authorization: Bearer <token>`. |
| `event_transaction_id` | string | — (required) | `$var` template for the event's idempotency/transaction id, e.g. `req_$request_uri`. |
| `subscription_id` | string | — (required) | `$var` template identifying the customer subscription, e.g. `cus_$consumer_name`. |
| `event_code` | string | — (required) | Lago billable-metric code the event bills against. |
| `endpoint_uri` | string | `/api/v1/events/batch` | Batch-send path appended to `endpoint`. |
| `ssl_verify` | bool | `true` | Verify the Lago TLS certificate. |
| `timeout` | int (ms) | `3000` | Per-flush HTTP timeout. |
| `log_format` | object | — | Custom `name -> "$var template"` map used for the event `properties`. |
| `include_req_body` / `include_resp_body` | bool | `false` | Include bodies in the default `properties` entry. |

All five core keys are required or config load fails. Batch keys default `batch_max_size` to **100** (Lago's batch limit); other batch-processor keys (`inactive_timeout`, `buffer_duration`, `max_retry_count`, `retry_delay`, `max_pending_entries`) follow the defaults.

```yaml
- id: lago-meter
  type: lago
  config:
    endpoint: http://127.0.0.1:3000
    token: ${LAGO_API_KEY}
    event_transaction_id: req_$request_uri
    subscription_id: cus_$consumer_name
    event_code: api_calls
```

## Behavior

For each request the node interpolates `event_transaction_id` and `subscription_id` against the request context, builds a `properties` log entry (default entry, or a `log_format` custom entry), and pushes one usage event:

```json
{
  "transaction_id": "req_/v1/items",
  "external_subscription_id": "cus_alice",
  "code": "api_calls",
  "timestamp": 1700000000,
  "properties": { "request": { "...": "..." }, "response": { "...": "..." } }
}
```

Events are buffered and the batch is POSTed as `{ "events": [...] }` to `<endpoint>/api/v1/events/batch`. The node is a pure passthrough: it never modifies the context and only its **success** port is taken. Delivery is best-effort — a full queue drops events with a warning and failed flushes are retried per the batch config.

## Behavior notes

- This node **meters** requests (one billing event per request) rather than logging them.
- A single `endpoint` is accepted, not an array of addresses.
- The event `properties` come from the shared log-entry builder, so they are the standard request/response entry (or a `log_format` custom entry) — configure `log_format` to shape them. An `event_properties` map is not supported.
