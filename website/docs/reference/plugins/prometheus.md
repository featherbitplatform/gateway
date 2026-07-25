---
title: prometheus
description: Thin parity node that adds a per-consumer request counter on top of featherbit's built-in Prometheus metrics.
---

<span className="plugin-chip" style={{'--chip-color': '#e6522c'}}>prometheus</span>

:::note featherbit already exposes Prometheus metrics
You do **not** need this node to get Prometheus metrics. featherbit records per-route request counters, request-latency histograms, and per-node execution metrics **out of the box** — the graph engine and the data-plane listener feed them on every request with no plugin involved, and they are rendered at the Admin API's **`/metrics`** endpoint. This node only *adds a dimension* on top of the always-on core metrics.
:::

A **thin metrics node** that records one dimension the built-in metrics do not: a **per-consumer request counter**. It never mutates the context and never fails, so only its **success** port is ever taken.

## What it adds

Each execution bumps `gateway_consumer_requests_total`, an `IntCounterVec` labelled:

- `consumer` — `consumer.name` from `context.message` (attached by an auth node such as `key-auth` / `basic-auth`), or `anonymous` when none is attached.
- `route` — the request `Host` (kept deliberately low-cardinality; see deviations).

Because the counter keys on the attached consumer, this node **must be placed after the auth node** that attaches it. Placed before (or with no auth), every request counts as `anonymous`.

## Built-in core metrics (always on)

These are recorded without any plugin and served at `/metrics` on the Admin API port:

| Metric | Labels | Meaning |
|---|---|---|
| `gateway_requests_total` | `route`, `method`, `status` | Total requests. |
| `gateway_request_duration_seconds` | `route` | End-to-end request-latency histogram. |
| `gateway_request_errors_total` | `route`, `error_code` | Failed requests. |
| `gateway_node_executions_total` | `policy`, `node_id`, `node_type` | Graph-node executions. |
| `gateway_node_duration_seconds` | `policy`, `node_id` | Per-node execution-latency histogram. |
| `gateway_node_errors_total` | `policy`, `node_id`, `error_code` | Node failures. |
| `gateway_consumer_requests_total` | `consumer`, `route` | **Added by this node** — per-consumer request count. |

## Configuration

| Key | Type | Default | Description |
|---|---|---|---|
| `prefer_name` | bool | `false` | Accepted for config compatibility; **inert** (the `route` label is always the request host — see behavior notes). |

All keys are optional; this node's configuration never fails to parse.

```yaml
- id: key-auth
  type: key-auth
  config:
    use_consumers: true
# place prometheus AFTER the auth node so the consumer is attached
- id: prometheus
  type: prometheus
  config:
    prefer_name: true
```

## Behavior notes

- The core metrics are **built-in and always on**, so this node is only a thin add-on that records the per-consumer counter.
- featherbit has no route object on the context here, so the `route` label uses the request `Host` (low-cardinality). `prefer_name` is accepted for config compatibility but is otherwise inert.
