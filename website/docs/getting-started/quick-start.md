---
title: Quick Start
description: Run featherbit locally or with Docker Compose and send your first request through the gateway.
---

featherbit needs two YAML files to start: `system.yaml` (listener, timeouts, logging, admin API) and `gateway.yaml` (routes and policies). The repository ships working examples in the `config/` directory, wired to a small echo backend used for development.

## Run locally

```bash
cargo build
cargo run -- --system-config config/system.yaml --gateway-config config/gateway.yaml
```

Both flags default to `config/system.yaml` and `config/gateway.yaml`, so from the repository root `cargo run` alone works too.

Once started, the gateway listens on two ports:

| Port | Purpose |
|---|---|
| `:8080` | Data plane — client traffic matched against your routes |
| `:9090` | Admin API and embedded web UI (HTTP Basic Auth, default `admin`/`admin`) |

The default `gateway.yaml` proxies `/api/*` to an echo backend on `localhost:3000`. Start it in another terminal so upstream calls succeed:

```bash
python dev/echo-backend/server.py
```

## Run with Docker Compose

```bash
docker compose up
```

This starts the **gateway** (ports `8080` and `9090`, with `config/` mounted into the container) and the **echo-backend** — a minimal HTTP server that echoes the method, path, and headers it receives back as JSON. Environment variables in the compose file (`ECHO_BACKEND_HOST`, `ADMIN_PASSWORD`, ...) are interpolated into the YAML config via the `${VAR:-default}` syntax.

## Send a request

The default route matches `path: /api/*` and applies a policy that strips the `/api` prefix before forwarding to the echo backend:

```bash
curl http://localhost:8080/api/users
```

The echo backend answers with a JSON view of what it actually received — note the stripped path:

```json
{
  "method": "GET",
  "path": "/users",
  "headers": {
    "host": "localhost:3000",
    "accept": "*/*"
  }
}
```

This makes it easy to verify exactly which path and headers your routing policy delivered to the upstream.

## Open the web UI

```bash
open http://localhost:9090
```

Log in with the admin credentials (default `admin`/`admin`) to see the node-graph editor. Select the `echo-api` route in the sidebar to view the policy you just exercised as a visual graph.

## Next steps

Continue with [Your first route](first-route.md) to add a route and policy of your own, or read [Architecture](../concepts/architecture.md) to understand what happened between `curl` and the echo response.
