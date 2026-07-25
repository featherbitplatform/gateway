---
title: Deployment
description: Docker Compose development setup, the scratch container image, and stateless multi-instance deployment.
---

## Docker Compose (development)

The repository ships a `docker-compose.yaml` for local development and E2E testing:

```yaml
services:
  gateway:
    build: .
    ports:
      - "8080:8080"      # data plane
      - "9090:9090"      # admin API + UI
    volumes:
      - ./config:/etc/gateway
      - ./examples/plugins:/etc/gateway/plugins
    environment:
      - GATEWAY_PORT=8080
      - ADMIN_PORT=9090
      - ADMIN_USER=admin
      - ADMIN_PASSWORD=admin
      - ECHO_BACKEND_HOST=echo-backend
      - ECHO_BACKEND_PORT=3000
      - LOG_LEVEL=info
    depends_on:
      - echo-backend

  echo-backend:
    build:
      context: ./dev/echo-backend
    ports:
      - "3000:3000"
```

The **echo-backend** is a minimal HTTP server that echoes back the request headers it receives from the gateway as a JSON response — send a request through the gateway, inspect the echo, and verify exactly which headers the upstream received after the routing policy was applied.

```bash
docker compose up

curl http://localhost:8080/api/users     # through the gateway
open http://localhost:9090               # node-graph editor
```

Config and script directories are bind-mounted, so editing `config/gateway.yaml` or a Lua script on the host triggers the gateway's hot-reload inside the container (see [Configuration](./configuration.md)).

## Container image: static binary, `FROM scratch`

The gateway is built as a fully static binary (musl) and shipped in a `FROM scratch` image — no OS, no shell, no extra attack surface. The final image contains only the binary, CA certificates, and the default config:

```dockerfile
FROM rust:alpine AS builder
RUN apk add --no-cache musl-dev g++ make
WORKDIR /app
COPY Cargo.toml Cargo.lock* ./
COPY src/ src/
COPY ui/dist/ ui/dist/
RUN cargo build --release

FROM scratch
COPY --from=builder /etc/ssl/certs/ca-certificates.crt /etc/ssl/certs/
COPY --from=builder /app/target/release/featherbit /gateway
COPY config/ /etc/gateway/
EXPOSE 8080 9090
ENTRYPOINT ["/gateway"]
CMD ["--system-config", "/etc/gateway/system.yaml", "--gateway-config", "/etc/gateway/gateway.yaml"]
```

Note that the UI must be built (`ui/dist/`) before the image, since `cargo build` embeds the UI assets into the binary.

## Graceful shutdown

On `SIGTERM` (the signal container orchestrators send) or Ctrl+C, the gateway shuts down gracefully:

1. **Stops accepting** new connections on every listener — the data plane, the Admin API, and all L4 (TCP/UDP) stream listeners.
2. **Drains in-flight HTTP and Admin requests** — established connections finish their current request/response, bounded by `timeouts.shutdown_timeout_seconds` (default 30s); anything still running when the deadline hits is dropped so the process can exit.
3. **Exits cleanly** once draining completes.

```yaml
# system.yaml
timeouts:
  shutdown_timeout_seconds: 30   # drain deadline (default)
```

This makes rolling deploys behind a load balancer safe: point the LB away, send `SIGTERM`, and in-flight requests complete instead of being cut. Long-lived **WebSocket** and **L4 tunnels** are not force-drained per-message — they stop being accepted and are closed when the process exits at the end of the drain window. Config-watcher tasks simply stop on exit.

## Stateless multi-instance deployment

featherbit's implemented clustering model is **stateless**: all gateway instances read the same configuration files (`system.yaml`, `gateway.yaml`, script files) from shared storage — a Kubernetes ConfigMap mount, NFS, or similar.

- When a config file changes on disk, each instance detects the change via its own filesystem watcher and hot-reloads **independently**.
- There is no coordination between instances — each is self-contained.
- To change configuration across all instances, edit the shared files. Admin API mutations apply to the in-memory configuration of the instance that received them, so with multiple instances behind a load balancer, file-based changes are the reliable propagation path.

This fits simple deployments, Docker Compose, single-node setups, and Kubernetes with ConfigMap-mounted config.

**Load-balancing caveat**: the `upstream` plugin's `least_connections` strategy tracks in-flight request counts **per gateway instance**. With multiple gateway replicas, each instance picks the target that is least loaded from its own local view, not globally across the fleet.

## HA clustering with etcd

For a coordinated multi-instance cluster, set `config.source: etcd` in `system.yaml`. Config (routes, policies, consumers) then lives in **etcd** as the source of truth: every instance loads from the same key prefix and converges on changes, and Admin API writes go to etcd so a change made on one node propagates to all.

```yaml
# system.yaml
config:
  source: etcd
  etcd:
    endpoints: ["http://etcd:2379"]
    prefix: /featherbit        # default
    # user / password optional
    timeout_ms: 3000
```

- **Storage layout** — one key per resource: `<prefix>/routes/<name>`, `<prefix>/policies/<name>`, `<prefix>/consumers/<name>`, each value the resource's JSON. You can inspect or edit them with `etcdctl` directly.
- **Bootstrap (seed-if-empty)** — on first start, if the prefix is empty the gateway seeds it from the local `gateway.yaml`, so a single-node file setup grows into a cluster without manual migration. If etcd already holds config, the local file is ignored.
- **Convergence** — each instance polls etcd (default every 2 s) and applies changes; an Admin API write is applied immediately on the writing node and picked up by the others within the poll interval. Invalid config is still rejected synchronously (the write validates before it reaches etcd).
- **Resilience** — an etcd outage does not take the data plane down: instances keep serving the last-good config and resume converging when etcd returns.
- **Transport** — featherbit talks to etcd's v3 **HTTP/JSON gateway** (no gRPC/protoc dependency). v1 is plaintext / no TLS to etcd (use a private network or loopback); TLS-to-etcd and a streaming watch are planned follow-ups.

Route precedence in etcd mode follows **key (name) order**, not file declaration order — name routes accordingly when match precedence matters.

Try it locally with the overlay compose (one etcd + two gateway replicas):

```bash
docker compose -f docker-compose.yaml -f docker-compose.etcd.yaml up
# then: a PUT to replica A's admin API converges to replica B within ~2s
```

File mode remains the default; nothing requires etcd unless you opt in.
