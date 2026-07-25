---
title: L4 stream proxying
description: Proxy raw TCP and UDP to load-balanced upstreams, independent of the HTTP engine.
---

# L4 stream proxying (TCP / UDP)

featherbit can proxy raw **TCP** and **UDP** in addition to HTTP. Stream listeners are a separate data path — there are no routes, policies, or plugins; a listener binds a port and relays bytes straight to an upstream pool. Configure them under `stream:` in `system.yaml`:

```yaml
stream:
  # Raw TCP passthrough (e.g. front a Redis backend)
  - protocol: tcp            # tcp | udp
    bind: 0.0.0.0
    port: 6379
    upstream:
      load_balancing: round_robin   # round_robin (default) | least_connections | ip_hash
      targets:
        - { host: redis, port: 6379 }

  # UDP datagram proxy (e.g. front a DNS resolver)
  - protocol: udp
    bind: 0.0.0.0
    port: 53
    upstream:
      targets:
        - { host: dns, port: 53 }
```

- Listeners bind **at startup** and are **fail-fast**: a bind failure or invalid config aborts the process with a clear error (like the HTTP listener). Stream config is static — it is not hot-reloaded.
- **Load balancing** is shared with the HTTP `upstream` node: `round_robin`, `least_connections` (tracks live connections/sessions), and `ip_hash` (sticky per client IP).
- Values support `${ENV:-default}` interpolation, so ports and hosts can come from the environment.

## TCP

Each accepted connection selects an upstream target, connects to it (bounded by `timeouts.connection_seconds`), and relays bytes in both directions with `copy_bidirectional` until either side closes. `least_connections` reflects live connections for the connection's whole lifetime.

## UDP

UDP has no connections, so featherbit tracks one **session per client address**. The first datagram from a client picks an upstream target and opens a dedicated socket to it; replies are routed back to that client. A session is torn down after it has been idle in **both** directions for `timeouts.idle_seconds`.

## SNI routing (TLS passthrough)

A TCP listener can route by the TLS **ClientHello's SNI hostname** — fronting many TLS backends on one port (e.g. `:443`) **without terminating TLS**. The gateway peeks the SNI, picks a backend pool, and relays the bytes on so the client completes its handshake end-to-end with the chosen backend.

```yaml
stream:
  - protocol: tcp
    bind: 0.0.0.0
    port: 443
    # Fallback for unmatched / non-TLS / no-SNI connections:
    upstream:
      targets: [ { host: default-backend, port: 8443 } ]
    sni_routes:
      - server_name: api.example.com          # exact match
        upstream:
          targets: [ { host: api-backend, port: 8443 } ]
      - server_name: "*.tenant.example.com"    # single-label wildcard
        upstream:
          load_balancing: least_connections
          targets:
            - { host: tenant-1, port: 8443 }
            - { host: tenant-2, port: 8443 }
```

- **Matching** is case-insensitive. `server_name` is either exact (`api.example.com`) or a single-label wildcard (`*.example.com` matches `a.example.com` but not `example.com` or `a.b.example.com`). Routes are tried in order; the first match wins.
- **Fallback** — connections whose SNI matches no route, that carry no SNI, or that aren't TLS at all go to the listener's `upstream` pool.
- **Passthrough only** — TLS is never decrypted; the gateway does not need the backends' certificates. (SNI-based certificate *selection* for TLS termination is a separate, not-yet-implemented feature.)
- Each route's `upstream` has its own load-balancing across targets. `sni_routes` on a UDP listener is ignored (with a warning).

## Try it

```bash
featherbit --system-config examples/system-stream.yaml --gateway-config config/gateway.yaml

# TCP:  redis-cli -p 6379 ping         (through the proxy)
# UDP:  dig @127.0.0.1 -p 53 example.com
```

## Not covered (yet)

- Dynamic / hot-reloadable stream routes (stream config is startup-only).
- TLS / SNI-based stream routing (passthrough or termination for L4).
- Stream-level plugins / node-graph hooks (L4 bypasses the graph entirely).
- Prometheus stream metrics (connections, bytes, sessions).
- Graceful drain of in-flight streams on shutdown, and an idle timeout on established TCP relays (a TCP relay lives until one side closes).
- UDP sources that change address mid-session (connected-socket semantics; symmetric-NAT upstreams aren't handled).
