---
title: Your First Route
description: Add a new route and routing policy to gateway.yaml, reload the gateway, and verify it with curl.
---

import UiShot from '@site/src/components/UiShot';

This tutorial adds a second route to the setup from the [Quick Start](quick-start.md): requests to `/orders/*` will be forwarded to the echo backend with the `/orders` prefix stripped, and upstream failures will get a custom JSON error response.

Make sure the gateway and the echo backend are running (locally or via `docker compose up`).

## 1. Add the route

A route pairs a **match rule** with the name of a **policy**. Append this to the `routes:` list in `config/gateway.yaml`:

```yaml
routes:
  # ... existing echo-api route ...
  - name: orders-api
    match:
      path: /orders/*
      methods: [GET, POST]
    policy: orders-policy
```

- `match.path` — glob-style path pattern the incoming request must match.
- `match.methods` — allowed HTTP methods.
- `policy` — the name of a policy defined in the `policies:` section. Routes are checked in declaration order; the first match wins.

## 2. Add the policy

A policy is a node graph: a list of **nodes** (plugin instances) and **edges** (connections between ports). Append this to the `policies:` list — it is the shipped `echo-policy` adapted for the new prefix:

```yaml
policies:
  # ... existing echo-policy ...
  - name: orders-policy
    error_handler: error-handler
    nodes:
      - id: listener
        type: listener

      - id: rewrite-request
        type: proxy-rewrite
        config:
          phase: request
          strip_path_prefix: /orders

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
          body_template: '{"error": "{{error.code}}", "message": "{{error.message}}"}'

      - id: client
        type: client

    edges:
      - from: listener.out
        to: rewrite-request.in
      - from: rewrite-request.success
        to: backend.in
      - from: backend.success
        to: client.in
      - from: backend.error
        to: error-handler.in
      - from: error-handler.success
        to: client.in
```

Reading it piece by piece:

- **Nodes.** Each node has a unique `id`, a `type` (one of the built-in plugin types), and a type-specific `config` map. Every policy must contain a `listener` node (the graph's entry point) and a `client` node (the terminal exit point) — see [Listener and client nodes](../concepts/listener-and-client.md).
- **Edges.** Each edge connects `from: node_id.port` to `to: node_id.port`. The listener emits the initial context on its `out` port; plugin nodes emit on `success` or `error`; targets receive on `in`. See [Policies and graphs](../concepts/policies-and-graphs.md) for the full port semantics.
- **Happy path.** `listener → rewrite-request → backend → client`: strip `/orders`, call the upstream, send its response to the caller.
- **Error path.** `backend.error → error-handler.in` routes upstream failures to a node that renders a 502 with a templated JSON body, which then flows to the same `client` node. The policy's `error_handler: error-handler` field additionally names this node as the catch-all for any node whose error port is not wired — see [Error handling](../concepts/error-handling.md).

## 3. Reload the configuration

You have two options:

- **Just save the file.** A file watcher detects changes to `gateway.yaml` and hot-reloads automatically.
- **Trigger it explicitly** via the admin API:

```bash
curl -X POST -u admin:admin http://localhost:9090/api/config/reload
```

Either way the gateway validates and compiles every policy before swapping the route table. If your edit is invalid (for example an edge pointing at a nonexistent node), the reload fails and traffic keeps flowing on the previous configuration — check the gateway logs for the validation messages.

## 4. Verify

```bash
curl http://localhost:8080/orders/123
```

The echo backend reports the path it received, confirming the prefix strip:

```json
{
  "method": "GET",
  "path": "/123",
  "headers": {
    "host": "localhost:3000",
    "accept": "*/*"
  }
}
```

To see the error path, stop the echo backend and repeat the request — the `error-handler` node now answers:

```json
{"error": "upstream_error", "message": "..."}
```

with status `502`.

## 5. Edit the same policy in the web UI

Open `http://localhost:9090` and select `orders-api` in the route list. The exact graph you wrote in YAML appears on the canvas: green wires for success edges, red for error edges. You can add plugins from the drawer, rewire ports, edit node config in the inspector, and click **Save Policy** to deploy — the UI and the YAML are two views of the same data, so changes made in either place stay in sync.

<UiShot
  name="editor"
  alt="The web UI with the orders-api route selected, showing its policy graph on the canvas."
  caption="The policy you just wrote in YAML, rendered on the canvas. Nothing was imported or converted — the editor is reading the same policy the gateway is running."
/>
