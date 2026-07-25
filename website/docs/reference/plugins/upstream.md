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
| `timeout_ms` | integer | `60000` | Whole-call deadline (connect + request + response body) per proxied request; exceeding it emits `UPSTREAM_TIMEOUT` through the error port. |
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

## Load balancing

- **round_robin** (default) — cycles through targets in order via a monotonic counter.
- **least_connections** — picks the target with the fewest in-flight requests. Each target's in-flight count is incremented when a request is dispatched and decremented when it completes, including on error paths.
- **ip_hash** — hashes the client IP from `context.request.remote_addr` (the ephemeral port is stripped first), so all connections from one client stick to the same target.

## Behavior

The plugin builds an HTTP request to `http://<host>:<port><path>`, forwarding the request method, all request headers (the `Host` header is overridden with the upstream target's `host:port`), and the buffered request body. On success it populates `context.response` with the upstream's status code, headers, and body, and exits through the `success` port. The upstream's status is passed through as-is — a backend 500 is still a `success`-port outcome.

Failures return the Context along with an error so the graph engine routes through the `error` port; the error is appended to `context.errors`:

| Code | When |
|---|---|
| `UPSTREAM_REQUEST_BUILD_ERROR` | The outbound request could not be constructed (e.g. invalid header values). |
| `UPSTREAM_CONNECTION_ERROR` | Connecting to or exchanging with the target failed. |
| `UPSTREAM_BODY_READ_ERROR` | Reading the upstream response body failed. |

The plugin does not read or write `context.message`.

Each proxied call runs under the `timeout_ms` deadline; exceeding it fails the node with error code `UPSTREAM_TIMEOUT` through the error port.
