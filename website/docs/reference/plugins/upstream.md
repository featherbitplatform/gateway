---
title: upstream
description: Forward the request to a backend target over HTTP with round-robin, least-connections, or IP-hash load balancing.
---

<span className="plugin-chip" style={{'--chip-color': '#f59e0b'}}>upstream</span>

Proxies the request to one of the configured backend targets over HTTP and writes the backend's status, headers, and body into `context.response`. It is the workhorse node of most pipelines, usually placed after any auth/traffic-control nodes and before the `client` node.

## Configuration

| Key | Type | Default | Description |
|---|---|---|---|
| `targets` | array of `{host, port}` | **required** | The backend pool. Entries missing `host` or `port` are skipped; if no valid target remains, config load fails. |
| `load_balancing` | string | `round_robin` | One of `round_robin`, `least_connections`, `ip_hash`. Hyphenated and short spellings (`round-robin`, `least-conn`) are accepted, as is the legacy key name `load_balancer` (saved by earlier UI builds). |
| `timeout_ms` | integer | `60000` | Whole-call deadline (connect + request + response body) per proxied request; exceeding it emits `UPSTREAM_TIMEOUT` through the error port. When the node is permitted to stream its response (see below), this bounds connect + request + response headers only — the body is then bounded by `stream_idle_timeout_ms` instead. |
| `stream_idle_timeout_ms` | integer | `60000` | Only consulted when the node is permitted to stream its response body straight through to the client. If no frame arrives on the body for this long, the stream is reaped; the timer resets on every frame, so a steady stream survives indefinitely. |
| `tls` | bool | `false` | Connect to the upstream over TLS — `https` for the buffered path, `wss` for a WebSocket upgrade. |
| `ssl_verify` | bool | `true` | Verify the upstream's TLS certificate against the system's native root store. Only meaningful when `tls` is set; set `false` for self-signed backends. |

```yaml
type: upstream
config:
  targets:
    - host: backend-1
      port: 8443
    - host: backend-2
      port: 8443
  load_balancing: least_connections
  tls: true          # https / wss to the upstream
  ssl_verify: true
```

Config load fails if `targets` yields an empty pool, if `load_balancing` is not a string, or if it names an unknown strategy — values like `random` are rejected with `Unknown load_balancing 'random' — supported: round_robin, least_connections, ip_hash`.

### Mutual TLS to the upstream

When `tls: true`, the node can present a client certificate and/or trust a
private CA:

| Key | Type | Description |
| --- | --- | --- |
| `client_cert_path` | string | PEM client certificate (chain) presented to the upstream. Requires `client_key_path`. |
| `client_key_path` | string | PEM private key for `client_cert_path`. Requires `client_cert_path`. |
| `ca_cert_path` | string | PEM CA bundle used to verify the upstream. **Replaces** the system trust store for this upstream. Incompatible with `ssl_verify: false`. |

Files are loaded and validated when the policy compiles: unreadable files,
cert/key mismatches, or contradictory combinations reject the policy. Rotating
certificates means touching `gateway.yaml` (hot-reload recompiles the policy)
or restarting the gateway. `ssl_verify: false` together with a client
certificate is allowed: the certificate is presented, the upstream's own
certificate is not verified.

```yaml
type: upstream
config:
  tls: true
  targets:
    - host: payments.internal
      port: 8443
  client_cert_path: /etc/featherbit/certs/gateway-client.crt
  client_key_path: /etc/featherbit/certs/gateway-client.key
  ca_cert_path: /etc/featherbit/certs/private-ca.crt
```

Both HTTPS proxying and `wss` WebSocket relays present the certificate.

## Load balancing

- **round_robin** (default) — cycles through targets in order via a monotonic counter.
- **least_connections** — picks the target with the fewest in-flight requests. Each target's in-flight count is incremented when a request is dispatched and decremented when it completes, including on error paths.
- **ip_hash** — hashes the client IP from `context.request.remote_addr` (the ephemeral port is stripped first), so all connections from one client stick to the same target.

## Behavior

The plugin builds an HTTP request to `http://<host>:<port><path>?<query>`, forwarding the request method, all request headers (the `Host` header is overridden with the upstream target's `host:port`), and the buffered request body. The query string is rebuilt from `context.request.query_params`, whose values are stored exactly as received (the query is split on `&`/`=` without percent-decoding), so it survives the hop unchanged; parameter **order is normalized**, because the original ordering is not retained at ingress. The same request-target — path plus query — is what a WebSocket upgrade relays to a `ws://`/`wss://` upstream. On success it populates `context.response` with the upstream's status code, headers, and body, and exits through the `success` port. The upstream's status is passed through as-is — a backend 500 is still a `success`-port outcome.

Failures return the Context along with an error so the graph engine routes through the `error` port; the error is appended to `context.errors` — see [Errors](#errors).

The plugin does not read or write `context.message`, other than consulting the reserved `__may_stream` key the graph compiler sets — see [Streaming](#streaming).

Each proxied call runs under the `timeout_ms` deadline; exceeding it fails the node with error code `UPSTREAM_TIMEOUT` through the error port. On a streaming response this deadline's meaning changes — see below.

## Streaming

Whether a response streams straight through to the client instead of being fully buffered first is **inferred at compile time — there is no config key to turn it on**. When a policy is compiled, the graph walks every node on an `upstream` node's success path; if all of them declare (via `Plugin::reads_response_body`) that they never read `context.response.body`, that upstream is marked stream-capable and the compiled graph tells this node so at request time through the reserved `context.message.__may_stream` key. Nothing else reads or sets that key.

When permitted to stream:

- The node returns to the graph engine as soon as the upstream's status and headers have arrived; the body is relayed to the client frame-by-frame as it is read from the upstream, not accumulated into memory first — this is what lets, for example, an SSE event reach a client while the upstream connection is still open.
- **`timeout_ms` changes meaning**: instead of bounding connect + request + the whole response body, it bounds only connect + request + response **headers**. Once headers are in, the only bound left on the body is `stream_idle_timeout_ms` — so a slow-arriving first byte still fails fast, but a long-lived, actively-streaming body is never killed by `timeout_ms`.
- Framing follows whatever the upstream declared, since response headers (including `content-length`, if present) are copied through unchanged: an upstream response of **unknown length** (no `content-length`) relays chunked on HTTP/1.1, with no `content-length` on the gateway's response either; an upstream response that **declares a `content-length`** is passed through with that header intact, and the gateway's response stays length-delimited (not chunked) instead. Either way the body is still relayed frame-by-frame as it arrives, not buffered first — `content-length` here only describes the framing, not whether the response streams.

**Streaming is an allow-list, not a block-list — read it as "everything blocks unless it opts out."** `Plugin::reads_response_body` defaults to `true`; a node forces the whole upstream to buffer unless it opts out. The opt-out is answered by the **configured instance**, not the type, so the same node type can block on one route and stream on another:

| Opts out | When |
|---|---|
| `client`, `prometheus`, `opentelemetry`, `zipkin`, `skywalking` | always |
| `response-rewrite` | no `filters`, no `body`, and no `vars` gate reading the response body |
| `proxy-rewrite` | not the response phase, or no `add_headers` value reading the response body |
| `request-id` | `header_name` does not read the response body |
| `traffic-label` | no rule's `matcher` reads the response body |
| the 16 `log_format` loggers | a `log_format` is set, none of its entries read the response body, and `include_resp_body` is off |

Two consequences worth calling out because they run the other way from what an operator might expect:

- **A logger with no `log_format` still blocks streaming.** It falls through to the default entry, which records `size: ctx.response.body.len()` — streaming would silently log `0`, so the gateway keeps buffering rather than report a wrong byte count. Set a `log_format` that does not reference the body to let the route stream.
- **`limit-conn` can never stream.** Its `release` node must sit after `upstream` on every path out, and `limit-conn` does not opt out — so any policy using it buffers, full stop.

Adding a body-reading node after `upstream` opts that upstream back into full buffering — silently, from the policy author's point of view, unless they check. They don't have to: `POST /api/policies/validate` (and the policy compiler generally) reports it, naming both nodes, e.g. a `buffering` array entry `{"upstream": "up", "blocked_by": "gzip"}`. The policy still compiles and serves traffic exactly as it did before this feature — it is just buffered, not broken.

**Body references inside templates and conditions are detected.** The response body is reachable from any `Template` or `Expression` — via the `resp_body` var, a `response_body:$...` JSONPath, or a `{{response.body}}` reference — so a node that otherwise only touches headers becomes a body reader through its own config. Four node types evaluate one of these *after* `upstream` has run, and each is checked at compile time:

- `traffic-label` — a rule's `matcher` condition
- `response-rewrite` — the `vars` gate
- `proxy-rewrite` — a response-phase `add_headers` template
- `request-id` — the `header_name` template

A `traffic-label` rule matching on `["resp_body", "~~", "error"]` therefore forces buffering and keeps matching, instead of silently never matching against an empty body. The blocking node is named in the `buffering` report like any other.

The check errs toward buffering. The legacy `$resp_body` spelling is detected textually, because it is resolved at render time and never becomes a parsed reference — so a literal string containing `$resp_body` in a field that never interpolates `$var` also forces buffering. Buffering a response that could have streamed forgoes an optimization; streaming one whose body a node needed corrupts it.

**A mid-stream failure can never become an error response.** Once this node has prepared a streaming response, its status code and headers are already committed to the wire by the time any failure in the body — an idle timeout, the upstream dropping the connection, any other transport error partway through — could occur, so there is no way to rewrite it into a `4xx`/`5xx` with a body the way a pre-body failure can. The connection is instead terminated without the chunked terminator, so a client sees an unambiguous truncation rather than a response that silently and incorrectly claims to be complete.

## Errors

The node returns the Context with an error, so the graph engine routes through the `error` port and appends the error to `context.errors`. It prepares no response of its own: what the caller sees is decided by the policy's `error` wiring, an [`error-handler`](error-handler.md), or the gateway's default 500.

| Code | Status | When |
|---|---|---|
| `UPSTREAM_REQUEST_BUILD_ERROR` | — | The outbound request could not be constructed (e.g. invalid header values). |
| `UPSTREAM_CONNECTION_ERROR` | — | Connecting to or exchanging with the target failed. |
| `UPSTREAM_BODY_READ_ERROR` | — | Reading the upstream response body failed. |
| `UPSTREAM_TIMEOUT` | — | The call exceeded the `timeout_ms` deadline (connect + request + response body). |
