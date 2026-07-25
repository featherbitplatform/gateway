---
title: opentelemetry
description: Distributed tracing via OpenTelemetry — a start/end node pair that propagates W3C traceparent and exports OTLP/HTTP spans.
---

<span className="plugin-chip" style={{'--chip-color': '#425cc7'}}>opentelemetry</span>

Distributed tracing via OpenTelemetry, modelled as a **start/end node pair** wired around the `upstream` node. A per-request span lives in `context.message`: the start node creates and stores it; the end node loads and exports it.

## Placement

- **start node** (`phase: start`): place it right after `listener`, **before** `upstream`.
- **end node** (`phase: end`): place it **after** `upstream`, right before `client`.

Both nodes always continue through their **success** port — tracing is observability and never short-circuits the request through an error port. If the end node finds no stored span (start node absent or misordered) it passes the context through untouched.

## Propagation

The start node reads the incoming W3C `traceparent` header:

- **Header present** → this hop **continues** the trace: the incoming `trace_id` is reused, the caller's span becomes this hop's parent, and the incoming sampled flag is honored.
- **Header absent** → a **new** trace is started (fresh random `trace_id`, no parent, sampled per the `sampler` config).

Either way this hop gets a fresh `span_id` and start time, and the start node **injects** a `traceparent` header carrying this hop's span so the upstream service continues the trace.

## Configuration

| Key | Type | Default | Description |
|---|---|---|---|
| `phase` | string | — (required) | `start` (before upstream) or `end` (after upstream). |
| `endpoint` / `collector` | string | `http://localhost:4318` | OTLP/HTTP collector base URL; traces POST to `<collector>/v1/traces`. |
| `service_name` | string | `featherbit` | `service.name` resource attribute reported on each span. |
| `sampler.name` | string | `always_on` | `always_on`, `always_off`, or `trace_id_ratio`. |
| `sampler.options.fraction` | number | `1.0` | Fraction of new traces to sample for `trace_id_ratio`, in `0.0..=1.0`. |
| `ssl_verify` | bool | `true` | Verify the collector's TLS certificate. |
| `timeout` | int (seconds) | `3` | Export request timeout (end node). |

Construction fails fast on an unknown `phase`, an unknown sampler name, or a `fraction` outside `0.0..=1.0`.

```yaml
# start node — right after listener, before upstream
- id: otel-start
  type: opentelemetry
  config:
    phase: start
    service_name: my-gateway
    sampler: { name: trace_id_ratio, options: { fraction: 0.1 } }
# end node — after upstream, right before client
- id: otel-end
  type: opentelemetry
  config:
    phase: end
    endpoint: http://localhost:4318
```

## Export

When the end node runs and the span is sampled, it builds an OTLP/HTTP JSON payload (`resourceSpans` → `scopeSpans` → one `SERVER` span with `http.method`, `http.status_code`, `http.target`, and `http.host` attributes; `status.code` is `2` for a 5xx response, else `0`) and **fire-and-forgets** a POST to `<collector>/v1/traces` on a detached background task — the same best-effort pattern as `proxy-mirror`. The export result is ignored and never blocks or fails the request. If the span is not sampled, the export is skipped.

Trace/span id fields (`traceId`, `spanId`, `parentSpanId`) are emitted as lowercase hex strings, the canonical OTLP/JSON encoding accepted by modern collectors.

## Sampling

For a **continued** trace, the incoming sampled flag is honored. For a **new** trace, the decision follows the `sampler`:

- `always_on` / `always_off` — unconditional.
- `trace_id_ratio` — a **pseudo-random**, per-trace-consistent draw derived by hashing the trace id (no `rand` crate) is compared against `fraction`. A given trace id always samples the same way within a process.

## Limitations

- Spans are exported one-per-request (fire-and-forget) rather than through a batch span processor; `batch_span_processor` config is not supported.
- Sampler strategies are `always_on`, `always_off`, and `trace_id_ratio`; `parent_base` is not supported.
