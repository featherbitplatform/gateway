---
title: listener & client
description: The two structural nodes that anchor every policy graph — the fixed entry point and the fixed exit point.
---

<span className="plugin-chip" style={{'--chip-color': '#8b5cf6'}}>listener</span> <span className="plugin-chip" style={{'--chip-color': '#10b981'}}>client</span>

Every routing policy contains two fixed structural nodes that anchor the graph: a `listener` node where execution starts and a `client` node where it ends. Neither takes any configuration — their `config` is always empty.

## listener

The graph's entry point. It represents the matched request from the ingress listener and emits the initial Context on its `out` port: `context.request` populated with all metadata from the incoming request, `context.response` empty, `context.errors` empty.

The node itself is a passthrough — the actual listener logic (accepting connections, matching routes, building the initial Context) lives in the server module. Execution of every policy graph starts at the `listener` node.

## client

The graph's terminal point, representing the requesting client. When the Context reaches a `client` node's `in` port, graph execution stops and the gateway sends whatever `context.response` holds at that moment back to the caller. The node is a passthrough and does not modify the Context.

Unlike regular inputs, a `client` node's input may have **multiple incoming edges** — the happy path and error-handler paths can all deliver the final response through the same node:

```yaml
edges:
  - from: listener.out
    to: backend.in
  - from: backend.success
    to: client.in
  - from: error-handler.success
    to: client.in
```

## Validation rules

Policy validation (run at config load and on Admin API writes) enforces both nodes structurally:

- Every policy **must** contain a `listener` node and a `client` node; a policy missing either is rejected.
- Each input port accepts only one incoming edge, **except** the inputs of `client` and `error-handler` nodes, which accept multiple.
- Like all nodes, they must not be orphans — each needs at least one connected edge.
