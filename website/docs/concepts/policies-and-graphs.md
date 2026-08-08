---
title: Policies and Graphs
description: How routing policies are expressed as node graphs — nodes, edges, ports, YAML serialization, and compilation rules.
---

import UiShot from '@site/src/components/UiShot';

A **routing policy** is a directed node graph that defines how requests matched by a route are processed. Each node is a plugin instance; each edge routes the [Context](context-object.md) from one node's output port to another node's input port. Policies live in `gateway.yaml` and are referenced by name from routes.

<UiShot
  name="policy-graph"
  alt="A policy graph: listener, cors, key-auth, rate-limit, proxy-rewrite, upstream and logging chained by green success edges; key-auth's denied port routes straight to client, while red dashed error edges from rate-limit and upstream converge on a shared error-handler."
  caption={<>A policy as the editor draws it. Solid green edges are the <code>success</code> path: <code>listener</code> → CORS → auth → rate limit → rewrite → upstream → access log → <code>client</code>. A rejected API key is a deliberate outcome, not a failure: it leaves key-auth's own dedicated <code>denied</code> port straight to <code>client</code>, its 401 already prepared. The dashed red edges leave rate-limit's and upstream's <code>error</code> ports — a throttled client and a failing upstream land on the same <code>error-handler</code> rather than a raw 500.</>}
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

| Port | Kind | Direction | Meaning |
|---|---|---|---|
| `out` | — | output | Alias for `success` on edge endpoints; the only output `listener` declares |
| `success` | `success` | output | Emits the context when the node completes normally and the request continues |
| *(plugin-declared)* | `outcome` | output | The node did its job and chose a deliberate alternate route — a rejection, redirect, throttle, or short-circuit, usually with the response already fully prepared. Names are drawn from a shared vocabulary (below), not invented per plugin |
| `error` | `error` | output | Emits the context (with the node's error appended) when the node **could not** do its job — configuration, parse, or infrastructure failure |
| `in` | — | input | Receives the context |

The endpoint string is split on the **last** dot, so node IDs may themselves contain dots. An endpoint with no dot at all defaults its port to `out`.

### Outcome ports and the mandatory-wiring rule

Most plugins declare only the default `success`/`error` pair. 37 node types
additionally declare one or more **outcome** ports for a deliberate
alternate result that isn't a failure — `key-auth` exits on `denied` for an
invalid API key, `cors` exits on `preflight` for an answered `OPTIONS`
request, `rate-limit` exits on `limited` for a 429, and so on. Every plugin's
full port declaration (name, kind, and a description) is inspectable from the
plugin catalog at `GET /api/plugins` — the same catalog the web UI's
node-graph editor reads to render each node's handles, colored by port kind,
with a warning badge on anything required but unconnected.

Every port of kind `success` or `outcome` is **mandatory**: the policy
compiler rejects any policy that leaves one unwired. For example, a policy
that wires `key-auth`'s `success` edge but not its `denied` edge fails
compilation (at startup, on hot-reload, or on an Admin API write) with:

```
policy 'my-policy': output port 'denied' of node 'auth' (type 'key-auth') must be wired — add an edge from 'auth.denied'
```

The message names the policy, the port, the node, and the node's type, and
validation collects **all** such violations at once rather than stopping at
the first. Only `error` stays optional — its fallback chain (per-node error
edge → policy catch-all → a generic 500, see [Error
handling](error-handling.md)) is unchanged, because it's fine for "the node
might fail someday" to have no dedicated handler; it is never fine for "the
node just produced a fully-formed rejection" to have nowhere to go.

The standard outcome vocabulary — no plugin invents a synonym:

| Port | Meaning | Typical status |
|---|---|---|
| `denied` | Deliberate policy rejection | 401 / 403 / 405 / 413 |
| `redirect` | Deliberate 3xx response | 301 / 302 / 307 / 308 |
| `limited` | Traffic-control rejection | 429 |
| `broken` | Circuit breaker open | 502 / 503 |
| `preflight` | CORS preflight answered | 204 |
| `abort` | Injected fault response | configurable |
| `routed` | Steered to and served by an alternate weighted target | backend-defined |
| `hit` | Served from cache | cached status |

Outcome ports typically carry a response that's already fully prepared, so
they're almost always wired straight to `client` — routing one through
`upstream` would let it overwrite the prepared response, and routing it
through `error-handler` would replace the prepared body with the handler's
template. A handful of node types (`limit-conn`, `api-breaker`,
`proxy-cache`) are expressed as a pair of nodes sharing one type and
therefore one port declaration, so the role that never actually emits the
outcome (e.g. `limit-conn`'s release node) still has to have it wired.

For any specific plugin's exact ports, when each one fires, and a wiring
example, see that plugin's own page in the [plugin
reference](../reference/plugins/index.md) — every plugin with outcome ports
carries a **Ports** section.

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
- indexes edges by their **source port**, normalizing `out` to `success`; an edge naming a port the node's type doesn't declare is a compile error, and two edges leaving the same `node.port` (fan-out) is also a compile error;
- enforces the **mandatory-wiring rule**: every `success` or `outcome` port of every node must have an outgoing edge, or compilation fails naming the missing edge (`error` ports are exempt — see [Error handling](error-handling.md));
- determines the **entry node** as the target of the listener's `success`/`out` edge — this is the first node executed for each request;
- fails if the policy has no `listener` node or a plugin cannot be constructed from its config.

Each node has at most one edge per output port. At runtime the engine follows the port the executing plugin's result names: `success` after a normal result, the plugin's own outcome port (e.g. `denied`, `redirect`) after a deliberate alternate result, or the error edge (or the policy's catch-all handler) after a failure — ending at a terminal client node or at a node with no outgoing edge for the port it just took.

One compiled graph instance serves all requests for the routes that reference its policy; it is shared read-only across requests.

:::note Planned
The specification also describes an `unpack` node for extracting typed values out of the Context and wiring them into named input ports of other nodes. This node type is not implemented; the only input port in use today is `in`, carrying the Context.
:::
