---
title: Architecture
description: How a request flows through featherbit, how shared state is locked, and how policies are compiled and swapped.
---

featherbit runs three concerns inside one process: the **data plane** (the HTTP listener that serves client traffic), the **admin API** (a separate axum server with the embedded UI), and the **hot-reload watcher**. All three share one state object.

## Request flow

```
HTTP request
  → server::listener matches a route (first match wins, in config order)
  → builds a Context from the request
  → CompiledGraph::execute() walks the policy's nodes
      following success/error edges
  → the final Context.response is sent to the client
```

In detail:

1. The data-plane server buffers the request body and scans the route table for the first route whose match rule (path, method, headers, host) accepts the request. Unmatched requests get a JSON 404.
2. A fresh [Context](context-object.md) is built: `request` populated from the incoming request, `response` empty (`status_code` 0), `message` and `errors` empty.
3. The route's compiled graph executes: starting at the entry node, each plugin runs and the walk follows its success edge on `Ok` or its error edge (or the catch-all handler) on failure, until a terminal `client` node is reached. See [Policies and graphs](policies-and-graphs.md) and [Error handling](error-handling.md).
4. The resulting `Context.response` is converted to an HTTP response. A `status_code` of `0` (never set by any node) is treated as `200`.

## Shared state and locking

`SharedState` is wrapped in an `Arc` and cloned into every server task. It holds:

| Field | Contents |
|---|---|
| `system` | Immutable `system.yaml` config, fixed for the process lifetime |
| `gateway` | Current `gateway.yaml` config behind a `RwLock`, mutated by admin API CRUD |
| `routes` | `RwLock`-protected route table: each route paired with an `Arc<CompiledGraph>` of its policy, in declaration order |
| `config_path` | Path to `gateway.yaml`, needed for reload-from-disk |
| `metrics` | Process-wide Prometheus registry |

The locking model keeps the request path cheap:

- The **data plane only takes short read locks** on `routes`, and only while matching the request. The lock is released **before** graph execution starts, so a slow upstream call never blocks a config reload.
- **Write locks** are taken only by the admin API and the hot-reload paths, when swapping in a freshly compiled route table.
- Route recompilation never happens on the request path.

## Policy compilation: validate → compile → swap

Every configuration load follows the same three steps:

1. **Validate** — each policy's graph structure is checked (`validate_policy`): listener and client nodes present, edges reference existing nodes, no duplicate inputs, no orphans. All violations are collected and reported together.
2. **Compile** — each valid policy is turned into a `CompiledGraph` with instantiated plugin objects and edges indexed by source port. Policies shared by multiple routes are compiled once and shared via `Arc`.
3. **Swap** — the new route table replaces the old one under a write lock.

This runs at startup (the process exits on invalid config — fail fast), after admin API mutations (`reload`), and when the file watcher or `POST /api/config/reload` re-reads `gateway.yaml` from disk (`reload_from_disk`). **A failed reload has no side effects**: if parsing, validation, or compilation fails, the existing route table stays in place and traffic keeps flowing on the last good configuration.

## Module map

| Module | Responsibility |
|---|---|
| `src/main.rs` | Entry point, CLI flags, startup orchestration, logging init |
| `src/config/` | YAML loading, `${ENV_VAR:-default}` interpolation, config structs |
| `src/context/` | The Context object (`request`, `response`, `message`, `errors`) |
| `src/graph/engine.rs` | Compiles `PolicyConfig` into `CompiledGraph`; executes the node walk |
| `src/graph/validation.rs` | Structural validation of policy graphs before compilation |
| `src/routing/` | Path/method/header/host route matching |
| `src/plugins/native/` | Built-in plugins |
| `src/plugins/script/` | Lua scripting runtime, Context↔Lua marshalling |
| `src/server/` | Data-plane HTTP listener, request dispatch |
| `src/admin/` | Admin API (axum), Basic Auth middleware, embedded UI serving |
| `src/metrics/` | Prometheus metrics registry |
| `src/hot_reload/` | File watcher triggering reload on `gateway.yaml` changes |
| `src/state.rs` | `SharedState`: lock-protected config and compiled route table |

## Configuration files

- `system.yaml` — listener bind/port, timeouts, logging, admin API settings. Loaded once at startup.
- `gateway.yaml` — routes and policies. Hot-reloaded on change.

All YAML values in both files support `${ENV_VAR:-default}` interpolation, resolved at load time.

:::note Planned
TLS termination, HTTP/2 tuning, WebSocket/TCP/UDP proxying, etcd-backed clustering, and graceful shutdown appear in the project specification but are not implemented.
:::
