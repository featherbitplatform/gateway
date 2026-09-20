---
title: serverless-post-function
description: Run one or more inline Lua functions against the Context after the upstream call, threading the Context through each in sequence.
---

<span className="plugin-chip" style={{'--chip-color': '#06b6d4'}}>serverless-post-function</span>

Identical to [`serverless-pre-function`](./serverless-pre-function.md) in every respect except its conventional placement: it sits **after** the `upstream` node, so its functions typically inspect or rewrite `ctx.response`.

## Configuration

Same shape as [`serverless-pre-function`](./serverless-pre-function.md#configuration).

| Key | Type | Default | Description |
|---|---|---|---|
| `functions` | array of strings | — (**required**, ≥1) | Each string is Lua source defining a global `execute(ctx)` function. Compiled at config load. |
| `phase` | string | — | Accepted for config compatibility, but **inert** — phase is expressed by the node's placement in the graph. |
| `timeout_ms` | integer | `5000` | Per-function execution timeout (stored, not yet enforced). |
| `modules_path` | string | — | Directory the sandboxed `require` resolves modules from. |

```yaml
- id: post
  type: serverless-post-function
  config:
    phase: body_filter     # accepted for compatibility; inert
    functions:
      - |
        function execute(ctx)
          ctx.response.headers["x-served-by"] = {"featherbit"}
          return ctx
        end
```

## Behavior

See [`serverless-pre-function` → Behavior](./serverless-pre-function.md#behavior). Functions compile at policy-compile time, run in order in fresh VMs threading the Context, succeed through the **success** port, and propagate the first error through the **error** port.

## Behavior notes

See [`serverless-pre-function` → Behavior notes](./serverless-pre-function.md#behavior-notes). Each function defines a global `execute(ctx)` and returns the Context, and phase is expressed by the node's position in the graph (this node after `upstream`).

## Errors

The functions run on the shared Lua runtime, so a failure in any of them propagates immediately — later functions do not run. The node returns the Context with the error, so the graph engine routes through the `error` port and appends the error to `context.errors`; it prepares no response of its own, so what the caller sees is decided by the policy's `error` wiring, an [`error-handler`](error-handler.md), or the gateway's default 500.

Returning a second value from `execute` is an error (`LUA_BAD_PORT`): these nodes have no `respond` port; a script that must answer the request belongs in a [`script`](script.md) node.

| Code | Status | When |
|---|---|---|
| `LUA_EXECUTION_ERROR` | — | A function raised a runtime error. |
| `LUA_LOAD_ERROR` | — | A function failed to load into the VM. |
| `LUA_MARSHAL_ERROR` | — | The Context could not be converted to a Lua table. |
| `LUA_MISSING_EXECUTE` | — | A function defined no global `execute`. |
| `LUA_UNMARSHAL_ERROR` | — | A returned table did not fit the `ctx` shape; the message names the field. |
| `LUA_BAD_PORT` | — | A function returned a second value (e.g. `return ctx, "respond"`); this node type has no such port. |

Load, missing-`execute` and syntax failures are normally caught at policy-compile time — they reach a live request only if the source changed underneath a compiled policy. The `ctx` table shape is documented on [`script`](script.md).
