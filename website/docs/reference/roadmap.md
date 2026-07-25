---
title: Roadmap
description: Features that are specified but not yet implemented, and their current state.
---

# Roadmap

The following features appear in the requirements or configuration surface but are not yet implemented. No delivery dates are promised; this page tracks the honest current state.

| Feature | Current state |
|---|---|
| Python scripting runtime (`pyo3`) | Not implemented. `examples/plugins/README.md` documents the target `execute(ctx)` API, but no `.py` files ship and `runtime: python` is rejected at policy-compile time. |
| TLS termination | **Implemented** — set `tls` in `system.yaml` to serve HTTPS on the data-plane listener (and, via `admin.tls`, the Admin API). rustls/ring, PEM cert+key, `min_version` `1.2`/`1.3`, **certificate hot-reload** (served on new connections without a restart), **mTLS** (client-cert verification via `client_ca_path`, required/optional, with the client fingerprint, subject CN, and SAN DNS names exposed to the pipeline for identity-based authz), and **SNI multi-certificate** termination (`sni_certs` presents a per-hostname cert on one listener; see [TLS guide](../guides/tls.md#multiple-certificates-by-sni)). Follow-ups: CRL/OCSP revocation. |
| HTTP/2 | **Implemented** — enabled by default (`http2.enabled`); the listener negotiates HTTP/2 alongside HTTP/1.1 (ALPN over TLS, h2c prior-knowledge over plaintext), and the outbound client advertises h2 to TLS upstreams (see [TLS & HTTP/2](../guides/tls.md)). |
| WebSocket proxying | **Implemented** — a client WebSocket upgrade runs the normal policy graph (access-phase plugins apply), the `upstream` node resolves the target, and the listener relays the upgraded connection to a `ws://` or `wss://` upstream (via the `upstream` node's `tls` flag). Both the HTTP/1.1 upgrade and **HTTP/2 extended CONNECT (RFC 8441)** are accepted from clients (see [TLS, HTTP/2 & WebSocket](../guides/tls.md#websocket-proxying)). Follow-up: RFC 8441 to the upstream (upstream leg is HTTP/1.1). |
| TCP/UDP proxying | **Implemented** — L4 stream listeners under `stream:` in `system.yaml` proxy raw TCP (accept → relay) and UDP (per-client datagram sessions) to a load-balanced upstream pool, independent of the HTTP engine, including **SNI-based TLS passthrough routing** on TCP (see [L4 stream proxying](../guides/stream.md)). Follow-ups: dynamic/hot-reloadable stream routes. |
| `proxy-cache` plugin | **Implemented** — an in-memory cache expressed as a lookup/store node pair sharing one namespace by `id` (see [proxy-cache](./plugins/proxy-cache.md)). Follow-up: a shared/distributed cache backend (Redis) for multi-instance deployments. |
| `unpack` node | Specified in the requirements; not implemented. |
| etcd clustering (stateful mode) | **Implemented** — `config.source: etcd` delivers config over etcd's v3 HTTP/JSON gateway with cluster-wide convergence and seed-if-empty bootstrap (see [Deployment → HA clustering with etcd](../guides/deployment.md#ha-clustering-with-etcd)). Follow-ups: TLS-to-etcd, streaming watch, multi-endpoint failover. |
| Graceful shutdown | **Implemented** — on `SIGTERM`/Ctrl+C the gateway stops accepting on every listener and drains in-flight HTTP + Admin requests (bounded by `timeouts.shutdown_timeout_seconds`, default 30s) before exiting (see [Deployment → Graceful shutdown](../guides/deployment.md#graceful-shutdown)). Long-lived WebSocket/L4 tunnels get the drain window then close at exit. |
| Script execution timeouts | The [script](./plugins/script.md) node parses `timeout_ms` (default 5000) and stores it, but the Lua VM does not enforce it — runaway scripts block the request. |
| Terminal nodes / CORS preflight | **Known bug.** A plugin cannot stop graph execution. [`cors`](./plugins/cors.md) writes a `204` preflight response onto the context, but the engine then follows the `success` edge into `upstream`, which proxies the `OPTIONS` and overwrites it — so preflight requests never get their 204. Fixing it needs a conditional/second output port on the node, or an engine-level terminal signal. Covered by `E2E-DP-09` (expected-failure) in the e2e suite. |
