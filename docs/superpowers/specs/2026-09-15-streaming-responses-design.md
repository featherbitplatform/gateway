# Streaming responses design

**Date:** 2026-09-15
**Status:** approved design, not yet implemented
**Target version:** 0.9.0 (new capability, not a patch)

## 1. Problem

featherbit cannot relay a streaming response. Two places make it impossible:

- `src/server/listener.rs:180` — the request handler returns `Response<Full<Bytes>>`. `Full` is a
  single complete buffer by definition; it has no streaming capability.
- `src/outbound/mod.rs:160` — the upstream response body is read with `.collect().to_bytes()`
  before the context continues through the graph.

So a Server-Sent Events endpoint, a chunked feed, or any long-lived response is held by the gateway
until the upstream closes it. The client receives nothing, then the connection dies at the
upstream's `timeout_ms`.

This was found in production: a notifications channel (`/api/v2/builder/status-stream`, consumed by
a frontend worker) delivered nothing through the gateway while working directly against the backend.

Two secondary effects are worth recording, because both cost debugging time:

- **The failure is invisible in debug traces.** A trace is recorded after `graph.execute_traced`
  returns (`listener.rs:329-350`); for a stream the graph never returns, so the hung request never
  appears. An empty trace list for a streaming route is *evidence of* the bug, not evidence against.
- **`X-Accel-Buffering: no` does nothing.** It is an nginx directive. Configs ported from
  nginx-family gateways carry it and it silently has no effect here.

WebSocket is unaffected: it has a dedicated relay path (`websocket::proxy_upgrade`) that never
touches the buffered body.

## 2. Constraint that shapes everything

54 of ~80 plugins reference `context.response.body`. They split into two roles:

- **Body producers** — the majority. They *write* a body the gateway itself generates: a 401
  denial, a 429, a mock, an error page. Always small, always terminal, never streamed.
- **Body transformers** — a minority that *read* an upstream body: `response-rewrite` (only when
  `filters` is configured), `gzip`, `brotli`, `body-transformer`, `data-mask`, `proxy-cache`,
  `degraphql`, `exit-transformer`, and loggers that log the body.

Only the second group conflicts with streaming. A design that avoids touching the first group keeps
the blast radius small.

`Context` is `Serialize`/`Deserialize` — it marshals into Lua and into debug snapshots — so a
stream handle cannot be an ordinary field. The codebase already solves this shape of problem: the
WebSocket upgrade handle is carried out-of-band while the `Context` stays serde-clean.

## 3. Decisions

| Question | Decision |
|---|---|
| Stream or buffer? | **Inferred at compile time** from whether anything downstream reads the body. No config knob. |
| `timeout_ms` on a stream | **Deadline to first byte**, then a separate idle timeout. Buffered responses keep today's whole-call semantics. |
| Body reader wired after an upstream | **Buffer, and surface which node forced it** — compile log, `validate_policy`, Admin API. |
| Mechanism | **Out-of-band stream handle** (`#[serde(skip)]`) plus compile-time inference. |
| Concurrency guards | **Move with the stream**, not released at headers. |
| Request-body streaming (uploads) | **Out of scope.** Different direction, different problems. |
| Debug traces of streams | Headers and status recorded, body noted as streamed, never captured. |

## 4. Data model

```rust
pub struct GatewayResponse {
    pub status_code: u16,
    pub headers: HashMap<String, Vec<String>>,
    #[serde(with = "bytes_serde")]
    pub body: Bytes,

    /// Streaming body, when the upstream response is relayed unbuffered.
    /// Skipped by serde: `Context` must stay serializable for Lua marshalling
    /// and debug snapshots, the same reason the WebSocket upgrade handle is
    /// carried out-of-band.
    #[serde(skip)]
    pub stream: Option<ResponseStream>,
}
```

**Invariant:** `stream.is_some()` implies `body` is empty. Exactly one of the two ever carries
content. A `Context` that crosses into Lua or into a trace snapshot loses the stream — acceptable,
because inference guarantees no such node sits on a streaming path.

`ResponseStream` wraps the boxed upstream body (`Send`, `'static`) plus the guards described in §7.

## 5. Compile-time inference

A new defaulted method on the `Plugin` trait, answered by the **configured instance**, not the type:

```rust
/// Whether this configured instance reads `context.response.body`.
///
/// Defaults to `true`: a plugin that does not opt out forces buffering, so
/// adding a plugin can never silently break a stream. Opting out is a
/// deliberate statement about a specific configuration.
fn reads_response_body(&self) -> bool {
    true
}
```

Opt-outs to implement, chosen for impact rather than completeness:

| Node | Opts out when |
|---|---|
| `response-rewrite` | neither `filters` nor `body`/`body_base64` is set (headers/status only) |
| `proxy-rewrite` | always — it never touches the body |
| `client` | always — terminal |
| `request-id`, `traffic-label` | always |
| `prometheus`, `opentelemetry`, `zipkin`, `skywalking` | always |
| loggers (`logging`, `http-logger`, `file-logger`, …) | when `log_format` does not reference the body |

At compile time, for each `upstream` node the compiler walks every path from that node's **`success`
port** to `client`. If every node on every such path opts out, the upstream is marked
stream-capable. The set of stream-capable node ids is stored on the `CompiledGraph`.

Only the `success` path is walked, deliberately. The `error` path is taken exactly when the upstream
produced no response body, so whatever sits there handles a body the gateway generates itself — an
`error-handler` downstream must not force the success path to buffer. A policy with several
`upstream` nodes (a `traffic-split` fan-out, say) evaluates each one independently.

Inference is conservative about runtime gates: a `response-rewrite` that configures `filters` behind
a `vars` condition counts as a reader even though the condition may be false at runtime. Config
presence, not runtime behavior, decides.

At execution the engine signals the decision through a reserved `context.message` key, the same
mechanism `__policy`, `__route`, `__request_start_ms` and `__ws_upstream_*` already use.

When a node forces buffering, the compiler records the node id and the reason. That record is
surfaced in three places: the compile log, the `validate_policy` response, and the Admin API so the
UI can mark the node. **Silence here would reproduce exactly the failure mode that motivated this
work.**

## 6. Upstream node

When marked stream-capable, the node skips `.collect()`, boxes the hyper body into
`response.stream`, sets status and headers, and exits `success`.

Timeout semantics change **only on a streaming response**:

| Phase | Bound |
|---|---|
| connect + request + response headers | `timeout_ms` (existing key, existing default) |
| between body chunks | `stream_idle_timeout_ms` (new, default 60000) |

A buffered response keeps today's whole-call `timeout_ms` exactly, so no existing deployment
changes behavior.

A mid-stream upstream failure **cannot** become an error page: status and headers are already on the
wire. The response terminates and the failure is logged. This asymmetry is a real behavioral
difference from buffered responses and must be documented on the plugin page, not just in code.

## 7. Concurrency guards

`upstream`'s balancer in-flight guard and `limit-conn`'s guard currently release when `execute()`
returns. On a streaming response that happens when the *headers* are sent, so a long-lived stream
would stop counting against concurrency limits almost immediately — worst on exactly the routes
where limits matter most.

Both guards move into `ResponseStream` and release when the stream ends or is dropped.

## 8. Transport

`listener.rs` returns `Response<BoxBody<Bytes, …>>` instead of `Response<Full<Bytes>>`, emitting
the stream when present and `Full` otherwise.

- No `content-length` on a streamed response; HTTP/1.1 uses chunked encoding, HTTP/2 uses data frames.
- Debug traces record status and headers plus a note that the body streamed. Capturing it would mean
  consuming it.
- The WebSocket branch is untouched.

## 9. Known limitations to document

1. **A streaming route cannot use body transformers.** `gzip`, `brotli`, a `response-rewrite` with
   `filters`, `proxy-cache`, `body-transformer` and body-logging loggers all force buffering. This
   is correct — compressing or caching an endless stream is not meaningful — but it must be stated,
   and the compiler must say which node did it.
2. **Mid-stream errors cannot be rewritten** into an error response (§6).
3. **Request bodies are still fully buffered.** Streaming uploads are a separate piece of work.
4. **`X-Accel-Buffering` remains inert.** It is honored by nginx, not by featherbit. Configs
   migrated from APISIX/nginx carry it harmlessly.

## 10. Testing

| Level | Test |
|---|---|
| Unit | Inference marks an upstream stream-capable when only header-only nodes follow |
| Unit | Inference does **not** mark it when a `response-rewrite` with `filters`, a `gzip`, or a body-logging logger follows — one test per reader kind |
| Unit | The forced-buffering record names the offending node |
| Unit | `response-rewrite` opts out with headers-only config and opts in with `filters` |
| Integration | A real SSE upstream: the first event reaches the client **before the server closes**. This is the property that is impossible today and is the single most important test in the suite. |
| Integration | Idle timeout fires on a silent stream; a stream with periodic events survives well past `timeout_ms` |
| Integration | A buffered response is byte-identical to today, including `content-length` |
| Integration | Concurrency guard is still held mid-stream and released at stream end |
| e2e | `E2E-STREAM-*` scenarios in `e2e/E2E_TESTBOOK.md` |

## 11. Out of scope

Request-body streaming; `X-Accel-Buffering` support; streaming through `proxy-cache`; compression of
streams; the `opentelemetry` `http.target` query-string omission noted during the audit that led
here.
