---
title: skywalking
description: Distributed tracing with Apache SkyWalking, as a start/end node pair around the upstream.
---

<span className="plugin-chip" style={{'--chip-color': '#5b3fa8'}}>skywalking</span>

Distributed tracing with [Apache SkyWalking](https://skywalking.apache.org/). It is a **start/end node pair** wrapped around the `upstream` node. The wire propagation format is the SkyWalking `sw8` header.

- The **start** node (`phase: start`, placed **before** `upstream`) reads an incoming `sw8` header to continue an existing trace, or begins a new one and makes the sampling decision. It creates this hop's span, stores it in `context.message`, and injects a fresh downstream `sw8` header so the upstream service joins the trace.
- The **end** node (`phase: end`, placed **after** `upstream`) loads the span and — when sampled — fire-and-forget POSTs a SkyWalking trace *segment* to `<endpoint_addr>/v3/segments` on a detached task. The export never blocks the request path.

Both nodes always continue through the **success** port; neither ever routes to the error port.

## Wiring

```text
... → skywalking(start) → upstream → skywalking(end) → ...
```

The start node must run before `upstream` (so it can inject `sw8` into the proxied request); the end node must run after `upstream` (so the final status and duration are available).

## Configuration

| Key | Type | Default | Description |
|---|---|---|---|
| `phase` | string | — (**required**) | `start` (before `upstream`) or `end` (after `upstream`). Any other value fails at config load. |
| `endpoint_addr` | string | `http://127.0.0.1:12800` | SkyWalking OAP HTTP base (used by the end node's export). |
| `service_name` | string | `featherbit` | Service name reported on the segment and in `sw8`. |
| `service_instance_name` | string | `featherbit Instance Name` | Service instance name. |
| `sample_ratio` | number | `1.0` | Fraction of *new* traces to sample, in `0.0..=1.0`. Requests arriving with a sampled `sw8` are always continued regardless of this ratio. |
| `ssl_verify` | bool | `true` | Verify the OAP TLS certificate on export. |
| `timeout` | int (seconds) | `3` | Per-export HTTP timeout (end node). |

```yaml
# before upstream
- id: sw-start
  type: skywalking
  config:
    phase: start
    service_name: my-gateway
    sample_ratio: 1.0

# after upstream
- id: sw-end
  type: skywalking
  config:
    phase: end
    endpoint_addr: http://127.0.0.1:12800
    service_name: my-gateway
```

## Behavior

**Start node**
- Parses an inbound `sw8` header. If present, the trace id and sampling flag are continued and the header's parent span id is recorded as this span's parent. If absent, a new 128-bit trace id is generated and the sampling decision is made from `sample_ratio`.
- Stores the span (`trace_id`, `span_id`, `parent_span_id`, `sampled`, `start_ms`) in `context.message` for the end node.
- Injects a downstream `sw8` header (8 hyphen-separated fields: sample flag, base64 trace id, base64 segment id, plain-integer parent span id, base64 service, base64 instance, base64 endpoint, base64 peer).

**End node**
- Loads the span. If none was stored (start node absent or not run), it is a no-op and passes through.
- When the span is sampled, it builds a single-span Entry/Http segment and POSTs it as JSON to `<endpoint_addr>/v3/segments` on a detached task. Response and errors are ignored; the request is never blocked.

The exported segment carries `traceId`, `traceSegmentId`, `service`, `serviceInstance`, and one span with `spanId: 0`, `parentSpanId: -1`, `spanType: "Entry"`, `spanLayer: "Http"`, `componentId: 49`, the request path as `operationName`, start/end times in milliseconds, `isError` when the status is ≥ 400, and `http.method` / `http.path` / `http.status_code` tags.

## Limitations

Full SkyWalking correlation (segment references, multi-span segments, the entry/exit span pair, a background report timer) is reduced to a subset:

- Each request exports a **single-span** segment. The incoming `sw8`'s parent segment and service are honored for propagation (trace id + sampling flag continued) but are not emitted as a formal `refs` segment-reference on the exported span.
- Export is per-request and immediate (a detached task); there is no buffered report timer.
- The segment is POSTed as a single JSON object to `/v3/segments`; the real OAP endpoint also accepts a batch array — the single-object form is the documented subset here.
- `componentId` is reported as `49` (generic HTTP).
