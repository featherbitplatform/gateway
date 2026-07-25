---
title: serverless-pre-function
description: Run one or more inline Lua functions against the Context before the upstream call, threading the Context through each in sequence.
---

<span className="plugin-chip" style={{'--chip-color': '#06b6d4'}}>serverless-pre-function</span>

Runs a list of inline Lua functions as a single graph node, threading the Context through each in order. Place it **before** the `upstream` node (its `serverless-post-function` twin goes after). Each function has full read/write access to the Context, exactly like the [`script`](./script.md) node.

## Configuration

| Key | Type | Default | Description |
|---|---|---|---|
| `functions` | array of strings | — (**required**, ≥1) | Each string is Lua source defining a global `execute(ctx)` function. Compiled at config load. |
| `phase` | string | — | Accepted for config compatibility, but **inert** — phase is expressed by the node's placement in the graph. |
| `timeout_ms` | integer | `5000` | Per-function execution timeout passed to the Lua runtime (stored, not yet enforced by the VM). |
| `modules_path` | string | — | Directory the sandboxed `require` resolves modules from. |

```yaml
- id: pre
  type: serverless-pre-function
  config:
    phase: access          # accepted for compatibility; inert
    timeout_ms: 2000
    functions:
      - |
        function execute(ctx)
          ctx.request.headers["x-serverless"] = {"pre"}
          return ctx
        end
      - |
        function execute(ctx)
          ctx.message.checked = true
          return ctx
        end
```

Each function must define a global `execute(ctx)` and return the (possibly modified) Context table. See the [Lua scripting guide](../../guides/lua-scripting.md) for the Context table shape.

## Behavior

Every function string is compiled and validated once at policy-compile time (in `from_config`): a syntax error, a top level that fails to load, a missing `execute`, an empty `functions` array, or a non-string entry all reject the policy immediately — never a live request.

At request time the functions run in declaration order in fresh Lua VMs, threading the Context: the table one function returns is the input to the next. On success the final Context flows through the **success** port. If any function raises an error, that failure (`LUA_EXECUTION_ERROR`, etc.) is propagated immediately, routing the Context through the **error** port; later functions do not run.

## Behavior notes

- **Function contract.** featherbit reuses the `script` plugin's Lua runtime: each function defines a global `function execute(ctx) ... return ctx end` and receives/returns the marshalled Context table — the same contract as the [`script`](./script.md) node. There is no `conf` argument.
- **Phase by graph position.** featherbit expresses phase through *placement in the policy graph*: a `serverless-pre-function` node sits before the `upstream` node, a `serverless-post-function` node after it. The `phase` key is accepted for config compatibility but inert.
- `timeout_ms` is stored but not yet enforced by the VM (same caveat as the `script` node).
