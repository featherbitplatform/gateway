---
title: Observability
description: Prometheus metrics, liveness and readiness probes, and structured logging.
---

featherbit exposes Prometheus metrics and health probes on the admin port, and logs through the `tracing` subscriber configured in `system.yaml`.

## Prometheus metrics

`GET /metrics` on the admin port renders the shared gateway registry in the Prometheus text exposition format (`text/plain; charset=utf-8`). Unlike the health probes, `/metrics` requires Basic auth (see [Admin API](./admin-api.md)).

Six metric families are recorded — per-route metrics by the data plane, per-node metrics by the graph engine:

| Metric | Type | Labels | Description |
|---|---|---|---|
| `gateway_requests_total` | counter | `route`, `method`, `status` | Total number of requests |
| `gateway_request_duration_seconds` | histogram | `route` | End-to-end request latency (buckets 1 ms to 5 s) |
| `gateway_request_errors_total` | counter | `route`, `error_code` | Failed requests by error code |
| `gateway_node_executions_total` | counter | `policy`, `node_id`, `node_type` | Graph node executions |
| `gateway_node_duration_seconds` | histogram | `policy`, `node_id` | Per-node execution latency (buckets 0.1 ms to 500 ms) |
| `gateway_node_errors_total` | counter | `policy`, `node_id`, `error_code` | Node failures by error code |

The per-node families let you pinpoint which node inside a routing policy is slow or failing, not just which route.

```bash
curl -u admin:admin http://localhost:9090/metrics
```

## Health and readiness

Both probes are served on the admin port and are **exempt from authentication**, so orchestrators can hit them without credentials.

| Endpoint | Semantics |
|---|---|
| `GET /healthz` | Liveness: always `200 OK` with `{"status": "healthy"}` while the process is running |
| `GET /readyz` | Readiness: `200 OK` with the compiled route count once at least one route is loaded; `503 Service Unavailable` with `{"status": "not_ready", "reason": "no routes loaded"}` while the route table is empty |

Use `/healthz` for liveness probes (restart on failure) and `/readyz` for readiness probes (remove from load balancing until routes are compiled).

## Structured logging

Logging is configured in `system.yaml` and handled by the `tracing` subscriber:

```yaml
logging:
  level: ${LOG_LEVEL:-info}
  format: json        # "json" (default) or any other value for plain text
```

- `level` — log level filter (`trace` through `error`); defaults to `info`. The `RUST_LOG` environment variable, when set, overrides `level` at startup.
- `format` — `json` (the default, recommended for production) or plain text.

## Per-request tracing

Metrics and logs aggregate; they tell you *that* a route is slow or failing, not *which node* did it. For a step-by-step view of one request — the `Context` after every plugin, what each one changed, and which edge the engine followed — see [Debugging & sandbox](./debugging.md).
