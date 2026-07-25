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
