---
title: zipkin
description: Distributed tracing via Zipkin — a start/end node pair that propagates B3 headers and exports Zipkin v2 spans.
---

<span className="plugin-chip" style={{'--chip-color': '#ff9a3c'}}>zipkin</span>

Distributed tracing via Zipkin, modeled as a **start/end node pair** wired around the `upstream` node. A per-request span lives in `context.message`: the start node creates and stores it; the end node loads and exports it.

## Placement

- **start node** (`phase: start`): place it right after `listener`, **before** `upstream`.
- **end node** (`phase: end`): place it **after** `upstream`, right before `client`.

Both nodes always continue through their **success** port — tracing is observability and never short-circuits the request through an error port. If the end node finds no stored span it passes the context through untouched.

## Propagation

The start node extracts B3 propagation from either the single `b3` header (`traceid-spanid-sampled-parentid`) or the `x-b3-*` multi-header form:

- **Header present** → this hop **continues** the trace: the incoming `trace_id` is reused, the caller's span becomes this hop's parent, and the incoming sampled flag is honored.
- **Header absent** → a **new** trace is started (fresh random `trace_id`, no parent, sampled per `sample_ratio`).

Either way this hop gets a fresh `span_id` and start time, and the start node **injects** the `x-b3-traceid`, `x-b3-spanid`, `x-b3-sampled` (and `x-b3-parentspanid`) headers carrying this hop's span so the upstream service continues the trace.

## Configuration

| Key | Type | Default | Description |
|---|---|---|---|
| `phase` | string | — (required) | `start` (before upstream) or `end` (after upstream). |
| `endpoint` | string | — (required) | Full Zipkin v2 collector URL, e.g. `http://localhost:9411/api/v2/spans`. |
| `service_name` | string | `featherbit` | `localEndpoint.serviceName` reported on each span. |
| `server_addr` | string | — | Optional `localEndpoint.ipv4` for the reporting host. |
| `sample_ratio` | number | `1.0` | Fraction of new traces to sample, in `0.0..=1.0`. |
| `ssl_verify` | bool | `true` | Verify the collector's TLS certificate. |
| `timeout` | int (seconds) | `3` | Export request timeout (end node). |

Construction fails fast on an unknown `phase`, a missing/empty `endpoint`, or a `sample_ratio` outside `0.0..=1.0`. The same config (including `endpoint`) is placed on both the start and end nodes.

```yaml
# start node — right after listener, before upstream
- id: zipkin-start
  type: zipkin
  config:
    phase: start
    endpoint: http://localhost:9411/api/v2/spans
    service_name: my-gateway
    sample_ratio: 0.1
# end node — after upstream, right before client
- id: zipkin-end
  type: zipkin
  config:
    phase: end
    endpoint: http://localhost:9411/api/v2/spans
```

## Export

When the end node runs and the span is sampled, it builds a Zipkin v2 JSON span array (one `SERVER` span with `traceId`, `id`, optional `parentId`, `timestamp`/`duration` in microseconds, `localEndpoint.serviceName`, and `http.method`/`http.status_code`/`http.path` tags) and **fire-and-forgets** a POST to the collector `endpoint` on a detached background task — the same best-effort pattern as `proxy-mirror`. The export result is ignored and never blocks or fails the request. If the span is not sampled, the export is skipped.

## Sampling

For a **continued** trace, the incoming `x-b3-sampled` flag is honored. For a **new** trace, a **pseudo-random**, per-trace-consistent draw derived by hashing the trace id (no `rand` crate) is compared against `sample_ratio`. A given trace id always samples the same way within a process.

## Limitations

- Spans are exported one-per-request (fire-and-forget) rather than through a batched reporter.
- Only the Zipkin v2 span format is emitted, and only a single `SERVER` span per hop — no child spans.
