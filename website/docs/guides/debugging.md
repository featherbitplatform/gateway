---
title: Debugging & sandbox
description: Per-request policy-execution tracing and the plugin sandbox — see the Context at every node, and run plugins against a synthetic request.
---

Metrics tell you *that* a route is failing; a trace tells you *which node* did it. Debug mode records the [`Context`](../concepts/context-object.md) at every step of a policy graph — what each plugin changed, which edge the engine followed, how long it took — and the **sandbox** runs plugins against a request you make up, without sending real traffic.

Both are off by default and gated behind the Admin API's authentication.

## Enabling debug mode

Add a `debug:` section to `system.yaml`:

```yaml
debug:
  enabled: ${FEATHERBIT_DEBUG:-false}
  capture_bodies: ${FEATHERBIT_DEBUG_BODIES:-false}
```

:::warning Restart required
`system.yaml` is read once at startup and never hot-reloaded, so **toggling debug mode requires a restart**. That is deliberate: it means someone who obtains an Admin API credential cannot switch on request-context capture in a running gateway.
:::

**Every field accepts an environment variable.** Interpolation is a text pass over the file *before* it is parsed, so it works for numbers and booleans as well as strings:

```yaml
debug:
  enabled: ${FEATHERBIT_DEBUG:-false}
  trace_all: ${FEATHERBIT_DEBUG_TRACE_ALL:-false}
  capture_bodies: ${FEATHERBIT_DEBUG_BODIES:-false}
  max_traces: ${FEATHERBIT_DEBUG_MAX_TRACES:-50}
```

The variable names are yours to choose — only what you write in `system.yaml` matters, the gateway does not look for any fixed set.

:::caution Keep the `:-` default
`${FEATHERBIT_DEBUG}` with the variable unset expands to empty text, which YAML reads as null — serde then rejects that for a boolean or number. Always write a default: `${FEATHERBIT_DEBUG:-false}`, `${FEATHERBIT_DEBUG_MAX_TRACES:-50}`. And a value you *do* set must be valid for the field (`max_traces: abc` will fail to parse).
:::

| Key | Default | Description |
|---|---|---|
| `enabled` | `false` | Master switch. While off, every `/api/debug/*` route except `GET /api/debug/config` returns `404`. |
| `sandbox` | `true` | Allows `POST /api/debug/sandbox`. Set to `false` to trace requests without exposing plugin execution. |
| `trigger_header` | `x-featherbit-debug` | Request header whose **presence** opts a single request into tracing. |
| `trace_all` | `false` | Trace every request instead of waiting for the header. |
| `capture_bodies` | `false` | Include request/response bodies in snapshots. The costly flag — see [Bodies](#bodies). |
| `max_body_bytes` | `8192` | Per-body truncation limit when capture is on. |
| `max_traces` | `50` | Ring-buffer capacity; the oldest trace is evicted first. |
| `max_steps` | `200` | Maximum nodes recorded per trace. |
| `sandbox_timeout_seconds` | `30` | Deadline for one sandbox run. |
| `redact_headers` / `redact_query_params` / `redact_message_keys` | `[]` | Names to redact **in addition to** the built-in denylists. |

## Tracing a request

Send the trigger header. Presence is what counts — `x-featherbit-debug: 0` still traces.

```bash
curl -i -H 'x-featherbit-debug: 1' http://localhost:8080/api/hello
```

The response carries the id of the recorded trace:

```
HTTP/1.1 200 OK
x-featherbit-trace-id: 9f1c3b2e-...
```

:::note The trace-id header is harness-added and cannot be removed by a policy
`x-featherbit-trace-id` is stamped by the gateway **after** the whole policy graph has run, so it is not in `context.response.headers` while any node executes — a `response-rewrite` or `proxy-rewrite` node cannot remove it (and pre-seeding it would not survive, since `upstream` replaces the response header map wholesale). It is only returned for requests that **carried the trigger header**: a request swept up by `trace_all` is captured silently, with no header stamped on the response the client sees. So under `trace_all` there is no `x-featherbit-trace-id` to strip — find those traces in the panel or via `GET /api/debug/traces` instead.
:::

Then read it from the admin port:

```bash
curl -u admin:admin http://localhost:9090/api/debug/traces | jq
curl -u admin:admin http://localhost:9090/api/debug/traces/9f1c3b2e-... \
  | jq '.steps[] | {node_id, edge, changes}'
```

```json
{
  "node_id": "strip-prefix",
  "edge": "success",
  "changes": [
    { "path": "request.path", "kind": "modified", "before": "/api/hello", "after": "/hello" }
  ]
}
```

Each step records the node id and type, the outcome, the duration, the **edge the engine followed**, and the full context *after* that node. The `changes` list is derived by comparing consecutive snapshots, so it answers "what did this plugin actually do" directly.

:::note The trigger header is not stripped
It stays on the request and is therefore **forwarded to the upstream**, and visible in the trace. That is deliberate — removing it would mean the request you traced is not the request you are debugging — but it does mean a traced request carries one extra header that an ordinary one does not. If an upstream is sensitive to unknown headers, account for that, or use a [`proxy-rewrite`](../reference/plugins/proxy-rewrite.md) node to drop it before proxying.
:::

### Reading the `edge` field

`edge` is often the answer on its own, because most policy bugs are wiring bugs:

| Value | Meaning |
|---|---|
| `success` | Followed the node's success edge. |
| `error` | Followed the node's own `error` edge. |
| `catch_all` | Fell through to the policy-level `error_handler`. |
| `terminal` | Reached a `client` node; the response was sent. |
| `end_of_chain` | Succeeded but had no success edge to follow. |
| `unhandled` | Errored with **neither** an error edge nor a catch-all — the engine wrote a generic `500`. |
| `node_not_found` | An edge pointed at a node id that does not exist. |

An `unhandled` step is the classic cause of "my plugin returned 401 but the client saw 500". See [Error handling](../concepts/error-handling.md).

## The trace API

| Method | Path | Purpose |
|---|---|---|
| `GET` | `/api/debug/config` | Effective settings. **Always answers**, even when debug is off. |
| `GET` | `/api/debug/traces` | Summaries, newest first. Filter with `?route=&policy=&status=&source=&limit=`. |
| `GET` | `/api/debug/traces/{id}` | One trace, with per-step `changes`. `404` once evicted. |
| `DELETE` | `/api/debug/traces` | Empties the buffer. |
| `POST` | `/api/debug/sandbox` | Runs plugins or a policy against a synthetic context. |

While `debug.enabled` is false these return `404` rather than `403`: a `403` would confirm the surface exists, and these endpoints dump request contexts and execute plugins. Each rejection logs a warning naming the config key, and `GET /api/debug/config` still answers so the web UI can explain itself.

## Browsing recent traffic

Adding the header works when you control the client. When you do not — a browser app, a mobile client, a partner calling in — set `trace_all` and every request is captured into the same bounded buffer:

```yaml
debug:
  enabled: true
  trace_all: true      # capture every request, not just header-flagged ones
  max_traces: 50       # the "limited number" kept
```

Then browse the recent requests and select one to inspect, narrowing by the policy or route you are working on:

```bash
# recent requests on one policy, newest first
curl -u admin:admin 'http://localhost:9090/api/debug/traces?policy=echo-policy' | jq
# or by route, status, source, with a cap
curl -u admin:admin 'http://localhost:9090/api/debug/traces?route=echo-api&status=502&limit=10' | jq
```

The web UI's **Traces** tab shows the same list with a policy filter, and refreshes itself every couple of seconds while open, so requests that arrive after you open the panel appear on their own. Pick a request, step through its nodes.

Requests that match **no route** (a `404`) are captured too, under the policy label `(no route matched)` — so "I sent a request but nothing showed up" holds even for a path that never reaches a policy. Filter them out by picking a real policy if the 404 noise is in your way.

:::note One shared, bounded buffer
All routes share a single ring buffer of `max_traces`, and filtering happens on read. Under heavy mixed traffic a chatty route can therefore evict a rare request from a quiet one before you look at it — raise `max_traces`, or add the trigger header to the specific request you care about so you can fetch it by id immediately. `trace_all` also snapshots the context once per node for *all* traffic, so it is a development convenience, not a production sampling mechanism; the gateway logs a warning at startup when it is on.
:::

## The sandbox

Run plugins against a request you invent. Two modes; supply exactly one of `nodes` or `policy`.

**A named policy** — replay a synthetic request through a configured pipeline. The policy does not need a route attached, which is usually the one you are iterating on:

```bash
curl -u admin:admin -X POST http://localhost:9090/api/debug/sandbox \
  -H 'content-type: application/json' \
  -d '{"policy": "echo-policy", "context": {"path": "/api/hello"}}'
```

**Ad-hoc nodes** — test one plugin, or a handful, in isolation:

```bash
curl -u admin:admin -X POST http://localhost:9090/api/debug/sandbox \
  -H 'content-type: application/json' \
  -d '{
        "nodes": [{"id": "rw", "type": "proxy-rewrite",
                   "config": {"phase": "request", "strip_path_prefix": "/api"}}],
        "context": {"path": "/api/hello"}
      }'
```

featherbit synthesises a policy around your nodes — prepending a `listener`, appending a `client`, and chaining them through their success ports — then compiles and runs it through the ordinary engine. A sandbox run can never diverge from what the gateway really does, because it *is* the gateway.

The result carries the same trace shape as a live request, so everything in [Reading the `edge` field](#reading-the-edge-field) applies.

### The synthetic context

Every field is optional; `{}` yields a valid `GET /` run.

| Field | Default |
|---|---|
| `method` / `path` / `host` / `scheme` | `GET` / `/` / `sandbox.local` / `http` |
| `headers` / `query_params` | empty. A bare string is accepted: `{"apikey": "abc"}` |
| `body` / `body_base64` | empty. Supply at most one. |
| `remote_addr` / `protocol` | `127.0.0.1:0` / `http1` |
| `message` | empty |
| `response` | unset — seed `status_code`/`headers`/`body` to exercise response-phase plugins like [`response-rewrite`](../reference/plugins/response-rewrite.md) and the loggers |

Unknown fields are **rejected**, so a typo fails loudly instead of silently defaulting.

:::tip Replay a request you saw in a trace
`context` also accepts the **trace snapshot shape** — the nested `{request, response, message, errors}` object you get from `GET /api/debug/traces/{id}`. Paste a step's `after` (or the trace's `initial`) straight in and it is flattened automatically; display-only fields (`errors`, body `len`/`truncated`/`binary`) are ignored. The web UI's **Copy to sandbox** button on a trace does this in one click. Caveat: snapshots are **redacted** — a header shown as `<redacted>` replays as that literal string, and a truncated or binary body cannot be reconstructed, so replace those with real values before relying on the run.
:::

:::note Response-phase plugins need a seeded response
The synthetic response starts empty. A `proxy-rewrite` with `phase: response` and `remove_headers: [x-powered-by]` will therefore report **no change** in the sandbox — there was no `x-powered-by` header to remove. Seed one and the removal shows up:

```json
{ "nodes": [{ "id": "rw", "type": "proxy-rewrite",
              "config": { "phase": "response", "remove_headers": ["x-powered-by"] } }],
  "context": { "response": { "status_code": 200, "headers": { "x-powered-by": "php" } } } }
```

The web UI's Sandbox tab has an **add a response** shortcut for exactly this.
:::

### `on_error` (ad-hoc nodes only)

| Value | Behaviour |
|---|---|
| `stop` (default) | Error ports left unwired, so a failing node records `edge: "unhandled"` — showing you exactly what an unwired error port does to a request. |
| `client` | Wires every node's error port to `client`, preserving the plugin's own status code. |

:::danger Plugins execute for real
The sandbox runs against the gateway's live resources. Outbound calls are made, rate-limit counters decrement, circuit breakers trip, loggers fire, and FaaS invocations are billed. Recompiling a policy gives fresh plugin *instances*, but they resolve into the same shared registries, so counters are **not** isolated. Set `debug.sandbox: false` to allow tracing without plugin execution.
:::

## Redaction

Redaction happens **at capture time** — a secret never enters the trace buffer, which outlives the request. Values are replaced with `<redacted>`, one per original value so multi-valued headers keep their shape.

Always redacted, case-insensitively:

- **Headers** — `authorization`, `proxy-authorization`, `cookie`, `set-cookie`, `x-api-key`, `api-key`, `apikey`, `x-auth-token`, `x-access-token`, `x-csrf-token`, `x-amz-security-token`, `x-forwarded-client-cert`.
- **Query parameters** — `access_token`, `id_token`, `refresh_token`, `token`, `api_key`, `apikey`, `code`, `client_secret`, `state`. OAuth puts codes and tokens in query strings, so header redaction alone would leak on every [`openid-connect`](../reference/plugins/openid-connect.md) trace.
- **`message` keys** — any key *containing* `secret`, `password`, `passwd`, `token`, `credential`, `private_key`, `authorization`, or `jwt`. Note that bare `key` is deliberately **not** matched, so `consumer.key_id` stays visible.

The `redact_*` config lists extend these; they never replace them.

:::danger What is not redacted
**Captured bodies are not redacted at all** — there is no general way to find a secret in an arbitrary payload. That is why `capture_bodies` is a separate flag and defaults off. A plugin or Lua script that copies a token into an unmatched `context.message` key will also leak it. Debug mode is a development and staging tool; do not run it against production traffic.
:::

## Bodies

`body.len` is always recorded, so you can see a body change size without capturing its content. With `capture_bodies: true` the text is included, UTF-8-lossy and clipped to `max_body_bytes`.

Consecutive identical bodies are stored once and marked `unchanged` — most nodes never touch the body, which turns the dominant memory cost from *steps × body size* into *body changes × body size*.

**Binary bodies** (an image, a blob, anything not valid UTF-8) are never rendered as text — they would be replacement-character mojibake. The snapshot instead sets `body.binary: true` and reports only `body.len`. This is purely about the debug *display*: the gateway itself proxies binary bodies byte-for-byte (they live as raw `Bytes`, and the `Context` serializes them losslessly as base64), so an image request/response passes through untouched whether or not it is being traced. A UTF-8 text body clipped mid-character by `max_body_bytes` is still shown as text, not mistaken for binary. To feed a binary body into the sandbox, use `body_base64` rather than `body`.

### Sizing the buffer

Worst case is roughly `max_traces × max_steps × (snapshot + 2 × max_body_bytes)`. With the defaults and bodies off, that is about 20 MB worst case and closer to 2 MB in practice. **When enabling body capture, lower the other limits** — `max_traces: 20`, `max_body_bytes: 4096` is a reasonable pairing.

## The web UI

The **Debug** button in the editor sidebar opens a panel with two tabs: *Traces* (pick a trace, step through its nodes, see what each changed) and *Sandbox* (pick a policy or paste nodes, supply a context, run). The nodes field is pre-filled from the policy open on the canvas. When debug mode is off the button stays visible but disabled, with a tooltip naming the config key. See [Web UI](./web-ui.md).

## Limitations

- **Each process has its own buffer.** In an HA deployment a traced request lands in exactly one replica while the UI talks to one admin port, so a trace you expect may be on another instance. Trace ids are returned to the caller precisely so you can correlate.
- Traces are in-memory only and do not survive a restart. For durable distributed tracing use the [`opentelemetry`](../reference/plugins/opentelemetry.md) or [`zipkin`](../reference/plugins/zipkin.md) nodes.
- L4 stream listeners and the WebSocket relay past the `101` are not traced; HTTP policy graphs are.
- `trace_all` snapshots the context once per node for *all* traffic. It is a development convenience, not a production sampling mechanism; the gateway logs a warning at startup when it is on.
