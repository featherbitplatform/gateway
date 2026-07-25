---
title: Policies and Graphs
description: How routing policies are expressed as node graphs — nodes, edges, ports, YAML serialization, and compilation rules.
---

import UiShot from '@site/src/components/UiShot';

A **routing policy** is a directed node graph that defines how requests matched by a route are processed. Each node is a plugin instance; each edge routes the [Context](context-object.md) from one node's output port to another node's input port. Policies live in `gateway.yaml` and are referenced by name from routes.

<UiShot
  name="policy-graph"
  alt="A policy graph: listener, cors, key-auth, rate-limit, proxy-rewrite, upstream and logging chained by green success edges, with red dashed error edges from key-auth, rate-limit and upstream converging on a shared error-handler."
  caption={<>A policy as the editor draws it. Solid green edges are the <code>success</code> path: <code>listener</code> → CORS → auth → rate limit → rewrite → upstream → access log → <code>client</code>. The dashed red edges leave each node's <code>error</code> port — a rejected API key, a throttled client, and a failing upstream all land on the same <code>error-handler</code> rather than a raw 500.</>}
/>

## Nodes

Each node entry has:

| Field | Meaning |
|---|---|
| `id` | Unique identifier within the policy; used by edges and in error records |
| `type` | Plugin type (`listener`, `client`, `upstream`, `proxy-rewrite`, `error-handler`, `jwt-auth`, `script`, ...) |
| `config` | Type-specific configuration map (optional for structural nodes) |
| `position` | Optional `{x, y}` canvas coordinates, used only by the web UI |

Two node types are structural rather than plugins: `listener` (entry) and `client` (exit) — see [Listener and client nodes](listener-and-client.md).

## Edges and ports

Edges use the `node_id.port` form on both ends:

```yaml
edges:
  - from: rewrite.success
    to: backend.in
```

| Port | Direction | Meaning |
|---|---|---|
| `out` | output | The listener's output port; treated identically to `success` |
| `success` | output | Emits the context when the node completes successfully |
| `error` | output | Emits the context (with the node's error appended) when the node fails |
| `in` | input | Receives the context |

The endpoint string is split on the **last** dot, so node IDs may themselves contain dots. An endpoint with no dot at all defaults its port to `out`.

## Full YAML example

The policy shipped in `config/gateway.yaml`:

```yaml
policies:
  - name: echo-policy
    error_handler: error-handler      # policy-level catch-all (optional)
    nodes:
      - id: listener
        type: listener

      - id: rewrite-request
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

      - id: rewrite-response
        type: proxy-rewrite
        config:
          phase: response
          remove_headers:
            - x-powered-by

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
        to: rewrite-response.in
      - from: rewrite-response.success
        to: client.in
      - from: backend.error
        to: error-handler.in
      - from: error-handler.success
        to: client.in
```

This same YAML is what the web UI reads and writes — designing the graph on the canvas and editing the file by hand are interchangeable.

## Compilation rules

Before serving traffic, each policy is validated (see the rules in [Error handling](error-handling.md#validation-rules)) and compiled into an executable graph. Compilation:

- instantiates each node's plugin from its `type` and `config`;
- records every `client` node as a **terminal** — execution stops when the context reaches one;
- indexes edges by their **source port**: `success` and `out` become the node's success edge, `error` becomes its error edge; any other source port is a compile error;
- determines the **entry node** as the target of the listener's `success`/`out` edge — this is the first node executed for each request;
- fails if the policy has no `listener` node or a plugin cannot be constructed from its config.

Each node has at most one success edge and one error edge. At runtime the engine walks the graph from the entry node: success edge after each successful node, error edge (or the policy's catch-all handler) after a failure, ending at a terminal client node or at a node with no success edge.

One compiled graph instance serves all requests for the routes that reference its policy; it is shared read-only across requests.

:::note Planned
The specification also describes an `unpack` node for extracting typed values out of the Context and wiring them into named input ports of other nodes. This node type is not implemented; the only input port in use today is `in`, carrying the Context.
:::
