---
title: Listener and Client Nodes
description: The two structural nodes that anchor every policy graph — the listener as entry point, the client as terminal exit.
---

Every routing policy contains two fixed nodes that anchor the graph's entry and exit. They are structural rather than functional: neither transforms the [Context](context-object.md), but both are **required by validation** — a policy without a `listener` node or without a `client` node is rejected before it can be compiled.

```yaml
nodes:
  - id: listener
    type: listener
  # ... plugin nodes ...
  - id: client
    type: client
```

## Listener node — the entry point

The listener represents the matched request from the ingress listener. When a route matches, the gateway builds the initial Context — `request` populated from the incoming request, `response` empty, `message` and `errors` empty — and execution begins at the node the listener's edge points to.

- The listener's output port is `out` (compiled identically to `success`).
- The **entry node** of the compiled graph is the target of the listener's `out`/`success` edge — the first node that actually executes per request:

```yaml
edges:
  - from: listener.out
    to: rewrite-request.in   # rewrite-request is the entry node
```

- Compilation fails outright if the policy has no listener node.

## Client node — the terminal exit

The client node represents the requesting client. It is recorded as a **terminal** at compile time: when the Context reaches a client node, graph execution stops and the gateway sends `context.response` back to the caller (a `status_code` of `0` becomes `200`).

- The client node is a **passthrough** — it does not modify the Context.
- Unlike regular inputs, a client node's `in` port may have **multiple incoming edges**. Validation enforces at most one edge per input port for ordinary nodes, but explicitly exempts `client` (and `error-handler`) nodes — so the happy path and any error-handler paths can all deliver the final response through the same client node:

```yaml
edges:
  - from: rewrite-response.success
    to: client.in            # happy path
  - from: error-handler.success
    to: client.in            # error path, same client node
```

## In the web UI

On the canvas the listener is pre-placed on the left as the starting point and the client on the right as the endpoint. Both are fixed and non-removable; plugin nodes are wired between them.

## Summary

| | Listener | Client |
|---|---|---|
| Role | Graph entry point | Graph exit point (terminal) |
| Behavior | Emits the initial Context | Passthrough; execution stops here, `context.response` is sent |
| Ports | `out` output | `in` input |
| Incoming edges | — | Multiple allowed |
| Required | Yes (validation and compilation) | Yes (validation) |
