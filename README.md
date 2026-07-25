# featherbit

A lightweight, high-performance API gateway delivered as a single binary. Routes are configured as visual node graphs — each plugin is a node, wired together through success and error ports.

![CI](https://github.com/francescodedomenico/featherbit/actions/workflows/ci.yml/badge.svg) ![Security](https://github.com/francescodedomenico/featherbit/actions/workflows/security.yml/badge.svg) ![Rust](https://img.shields.io/badge/rust-1.82+-orange) ![License](https://img.shields.io/badge/license-Apache--2.0-blue)

## Documentation

Full documentation lives at [francescodedomenico.github.io/featherbit](https://francescodedomenico.github.io/featherbit/) — source in [`website/`](website/) (Docusaurus). Run it locally with `cd website && npm install && npm run start`; it deploys to GitHub Pages via `.github/workflows/docs.yml`. API references: `cargo doc --no-deps --document-private-items` (Rust) and `cd ui && npm run docs` (TypeDoc) — both get bundled into the site at `/api/`.

## Features

- **Node-graph routing policies** — design request/response pipelines visually or in YAML. Each node has context in, success output, and error output ports
- **80+ built-in plugins** — proxying, transforms, security, auth & authz (key/basic/JWT/HMAC/LDAP/OIDC incl. interactive SSO), traffic control, 17 loggers, tracing, metrics, serverless
- **Lua scripting** — write custom plugins in Lua (Luau runtime), loaded at startup with hot-reload
- **Protocols** — HTTP/1.1 and HTTP/2 (ALPN + h2c), WebSocket proxying (incl. RFC 8441 over HTTP/2), and L4 TCP/UDP stream proxying with SNI routing
- **TLS** — termination with hot-reloading certs, per-hostname SNI certificates, and mTLS client identity exposed to the graph
- **Embedded web UI** — node-graph editor served from the binary itself, with dark/light mode and a per-request debug tracer
- **Admin REST API** — full CRUD for routes, policies, consumers, and scripts; config reload, health/readiness probes, Prometheus metrics
- **Hot-reload** — file watcher detects config changes and reloads without restart
- **etcd clustering** — optional stateful mode: config delivered over etcd with cluster-wide convergence
- **Environment variable interpolation** — `${VAR:-default}` syntax in all YAML config, built for Kubernetes and Docker Compose

## Quick Start

### Run locally

```bash
cargo build
cargo run -- --system-config config/system.yaml --gateway-config config/gateway.yaml
```

The gateway listens on `:8080` (data plane) and `:9090` (admin API + UI).

### Run with Docker Compose

```bash
docker compose up
```

This starts the gateway and an echo-backend that returns received request headers as JSON.

```bash
# Send a request through the gateway
curl http://localhost:8080/api/users

# Open the node-graph editor
open http://localhost:9090
```

## Configuration

Two YAML files drive the gateway. All values support `${ENV_VAR:-default}` interpolation.

### `system.yaml` — global settings

```yaml
listener:
  bind: "0.0.0.0"
  port: ${GATEWAY_PORT:-8080}

logging:
  level: ${LOG_LEVEL:-info}
  format: text     # "json" or "text"

admin:
  bind: "0.0.0.0"
  port: ${ADMIN_PORT:-9090}
  username: ${ADMIN_USER:-admin}
  password: ${ADMIN_PASSWORD:-admin}
```

### `gateway.yaml` — routes and routing policies

Routes match incoming requests and dispatch them to a routing policy (a node graph):

```yaml
routes:
  - name: echo-api
    match:
      path: /api/*
      methods: [GET, POST, PUT, DELETE]
    policy: echo-policy
```

Policies define nodes and edges. Each node is a plugin instance, each edge connects an output port to an input port:

```yaml
policies:
  - name: echo-policy
    error_handler: error-handler
    nodes:
      - id: listener
        type: listener
      - id: rewrite
        type: proxy-rewrite
        config:
          phase: request
          strip_path_prefix: /api
      - id: backend
        type: upstream
        config:
          targets:
            - host: ${ECHO_BACKEND_HOST:-localhost}
              port: ${ECHO_BACKEND_PORT:-3000}
      - id: error-handler
        type: error-handler
        config:
          status_code: 502
          body_template: '{"error": "{{error.code}}"}'
    edges:
      - from: listener.out
        to: rewrite.in
      - from: rewrite.success
        to: backend.in
      - from: backend.success
        to: listener.in
      - from: backend.error
        to: error-handler.in
      - from: error-handler.success
        to: listener.in
```

## How Routing Works

```
Client Request
  │
  ▼
┌──────────┐  success  ┌───────────────┐  success  ┌──────────┐  success  ┌───────────────┐
│ Listener │──────────▶│ Proxy Rewrite │──────────▶│ Upstream │──────────▶│ Proxy Rewrite │──┐
│          │           │ (strip path)  │           │ (backend)│           │ (strip hdrs)  │  │
└──────────┘           └───────────────┘           └────┬─────┘           └───────────────┘  │
     ▲                                                  │ error                               │
     │                                                  ▼                                     │
     │                                            ┌──────────────┐                            │
     │                                            │Error Handler │                            │
     │                                            │ (502 + body) │────────────────────────────▶│
     │                                            └──────────────┘                            │
     └────────────────────────────────────────────────────────────────────────────────────────┘
                                                                                    Client Response
```

Every node receives a **Context** object and outputs it through either its **success** or **error** port. The context carries:

- `context.request` — method, path, headers, body, query params, remote addr
- `context.response` — status code, headers, body (populated by upstream or error handler)
- `context.message` — free-form key/value map for inter-plugin data passing
- `context.errors` — accumulated errors from failed nodes

## Plugins

Core structural nodes:

| Plugin | Description |
|---|---|
| `listener` | Graph entry point — emits the initial Context (fixed, one per policy) |
| `client` | Graph exit point — receives the final Context and sends the response (fixed) |
| `proxy-rewrite` | Rewrite path, add/remove headers (works on request or response phase) |
| `upstream` | Forward to backend service with round-robin, least-connections, or IP-hash load balancing |
| `error-handler` | Render custom error responses with template variables |
| `script` | Lua scripted plugin (see below) |

Beyond these, 80+ node types cover transformation, security, auth/authz, traffic control, logging, tracing, metrics, and serverless. The [plugin reference](https://francescodedomenico.github.io/featherbit/docs/reference/plugins) documents every one.

## Lua Scripting

Write custom plugins in Lua. Scripts implement an `execute(ctx)` function that receives and returns the context:

```lua
function execute(ctx)
    -- Read request data
    local path = ctx.request.path
    local auth = ctx.request.headers["authorization"]

    -- Modify the request
    ctx.request.headers["x-custom"] = {"injected-value"}

    -- Pass data to downstream plugins
    ctx.message.processed_by = "lua-plugin"

    return ctx
end
```

Configure a script node in your policy:

```yaml
- id: custom-logic
  type: script
  config:
    runtime: lua
    source: /etc/gateway/plugins/custom.lua
    # or inline:
    # inline: |
    #   function execute(ctx) ... end
```

Scripts are validated at startup and support hot-reload when the source file changes.

## Admin API

The admin API runs on a separate port (default `:9090`) with HTTP Basic authentication.

| Method | Path | Description |
|---|---|---|
| `GET` | `/api/routes` | List all routes |
| `POST` | `/api/routes` | Create a route |
| `GET` | `/api/routes/:name` | Get a route |
| `PUT` | `/api/routes/:name` | Update a route |
| `DELETE` | `/api/routes/:name` | Delete a route |
| `GET` | `/api/policies` | List all policies |
| `GET` | `/api/policies/:name` | Get a policy (full node graph) |
| `PUT` | `/api/policies/:name` | Create/update a policy |
| `DELETE` | `/api/policies/:name` | Delete a policy |
| `GET` | `/api/plugins` | List available plugin types |
| `GET` | `/api/scripts` | List loaded Lua scripts |
| `GET`/`POST` | `/api/consumers` | List / create consumers (per-consumer credentials) |
| `GET`/`PUT`/`DELETE` | `/api/consumers/:name` | Get / update / delete a consumer |
| `GET` | `/api/status` | Gateway version, route/policy counts |
| `GET` | `/api/config/export` | Export the running config as YAML |
| `POST` | `/api/config/reload` | Hot-reload config from disk |
| `GET` | `/api/debug/traces` | Per-request policy-execution traces (requires `debug.enabled`) |
| `POST` | `/api/debug/sandbox` | Run a policy against a synthetic request (requires `debug.enabled`) |
| `GET` | `/healthz` | Liveness probe |
| `GET` | `/readyz` | Readiness probe |
| `GET` | `/metrics` | Prometheus metrics |

Changes via the API take effect immediately (hot-reload).

## Web UI

Open `http://localhost:9090` to access the node-graph editor. The UI is embedded in the binary — no separate web server needed.

- Select a route from the sidebar to open its routing policy
- Drag plugins from the drawer onto the canvas
- Connect nodes by dragging from output ports (green = success, red = error) to input ports
- Click a node to edit its configuration in the inspector panel
- Click **Save Policy** to deploy changes (hot-reload, no restart)
- Toggle dark/light mode with the theme button

## Development

### Prerequisites

- Rust 1.82+
- Node.js 22+ (for building the UI)

### Build

```bash
# Build the UI first
cd ui && npm install && npm run build && cd ..

# Build the gateway (embeds UI assets)
cargo build
```

### Test

```bash
cargo test                             # 700+ unit + integration tests (inline in src/)
cd e2e && npm install && npm test      # Playwright end-to-end suite (see e2e/E2E_TESTBOOK.md)
```

### Security scans

The same SAST pipeline that runs in CI (`.github/workflows/security.yml`) runs locally via Docker — semgrep, cargo-deny, grype, hadolint, gitleaks, npm audit, and container image scans:

```powershell
./dev/sast.ps1            # Windows (dev/sast.sh on Linux/macOS); `image` target scans the built image
```

### Project Structure

```
src/
├── main.rs              # Entry point, CLI, startup orchestration, graceful shutdown
├── config/              # YAML loading, env interpolation, config structs
├── config_store/        # etcd config store (stateful clustering mode)
├── context/             # Context object (request, response, message, errors)
├── graph/               # Node-graph compiler and executor
├── routing/             # Path/method/header/host route matching
├── plugins/
│   ├── native/          # Built-in plugins (80+ node types)
│   ├── script/          # Lua scripting runtime
│   └── util/            # Shared plugin infrastructure (codecs, sessions, trace propagation)
├── server/              # HTTP listener, TLS, WebSocket proxying, request dispatch
├── stream/              # L4 TCP/UDP stream proxying, SNI routing
├── admin/               # Admin API (axum), auth, embedded UI serving
├── debug/               # Per-request policy-execution traces + plugin sandbox
├── consumers/           # Consumer store (per-consumer credentials)
├── vars/                # Var resolver + expression engine
├── outbound/            # Shared outbound HTTP client
├── ratelimit/ batch/ traffic/  # Rate limiting, log batching, traffic control
├── balancer.rs          # Shared load balancer (HTTP upstreams + L4 pools)
├── metrics/             # Prometheus metrics registry
├── hot_reload/          # File watcher, config reload
└── state.rs             # Shared mutable state (RwLock-protected)
ui/                      # React + TypeScript admin UI (Vite, React Flow, Tailwind)
website/                 # Docusaurus docs site (GitHub Pages)
e2e/                     # Playwright end-to-end suite (admin API + data plane + UI)
config/                  # Example system.yaml and gateway.yaml
dev/                     # Local dev helpers: echo backend, SAST pipeline scripts
tests/                   # Integration fixtures (TLS certs, Keycloak realm, compose stacks)
```

## License

Apache License 2.0 — see [LICENSE](LICENSE). Portions are derived from third-party software; see [NOTICE](NOTICE) for attributions.
