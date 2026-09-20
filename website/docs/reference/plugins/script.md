---
title: script
description: Runs a user-provided Lua script as a graph node, with full read/write access to the Context.
---

<span className="plugin-chip" style={{'--chip-color': '#a855f7'}}>script</span>

Runs a user-provided script as a graph node behind the same plugin contract as native plugins. The script receives the full Context (`request`, `response`, `message`) and returns a possibly modified copy; anything it writes into `ctx.message` is visible to downstream nodes. It can sit anywhere in the request or response pipeline. Only the Lua (Luau) runtime is currently supported. The [Lua scripting guide](../../guides/lua-scripting.md) has the worked examples, `require` sandboxing and hot-reload rules (agents: `featherbit://docs/guides/lua-scripting`).

## The `ctx` table

Define a global `execute(ctx)`, mutate what you need, and **return the same table** — a fresh table of your own will not have the fields the gateway expects.

| Field | Type |
|---|---|
| `ctx.request.method` / `.path` / `.host` / `.scheme` / `.remote_addr` | string |
| `ctx.request.headers` / `.query_params` | table of `name → array of strings` (`{"alice"}`); assigning a bare string or number is accepted and wrapped |
| `ctx.request.body` / `ctx.response.body` | string (`nil` means empty); encode tables yourself, e.g. as JSON text |
| `ctx.response.status_code` | number, e.g. `200` |
| `ctx.response.headers` | same map-of-arrays shape as request headers |
| `ctx.message` | free-form map shared with every other node; values convert to/from JSON (arrays are 1-indexed tables) |

`protocol` and `errors` are not exposed and pass through untouched. Header and query names are lowercase as the gateway hands them over.

```lua
function execute(ctx)
  local name = string.match(ctx.request.path or "", "^/hello/([^/]+)")
  ctx.message.user = name or "stranger"          -- readable downstream as $msg_user
  ctx.request.headers["x-user"] = ctx.message.user  -- a bare string is fine
  return ctx
end
```

A returned table that does not fit this shape fails with `LUA_UNMARSHAL_ERROR`, and the message names the field (for example `ctx.response.body must be a string, got table`). For extracting a value without a script — a path segment, a header, a JSON body field — [`set-vars`](set-vars.md) does it declaratively.

## Configuration

| Key | Type | Default | Description |
|---|---|---|---|
| `runtime` | string | `lua` | Scripting runtime. Any value other than `lua` is a config error. |
| `source` | string | — | Path to a script file, read at policy-compile time. |
| `inline` | string | — | Script text embedded in the config. One of `source` or `inline` is required (`source` wins if both are set). |
| `timeout_ms` | integer | `5000` | Wall-clock budget for one execution, covering both loading the source and the `execute(ctx)` call. `0` disables enforcement. |
| `modules_path` | string | `source`'s parent directory (none for `inline`) | Directory the sandboxed `require` resolves modules from. |

```yaml
- id: enrich
  type: script
  config:
    runtime: lua
    source: scripts/enrich.lua
    timeout_ms: 2000
```

The script must define a global `execute(ctx)` function that returns the (possibly modified) context table:

```lua
function execute(ctx)
    ctx.request.headers["x-enriched"] = {"true"}
    ctx.message.user_tier = "gold"
    return ctx
end
```

:::note[What `timeout_ms` does and does not stop]
The budget is enforced by a Luau VM interrupt, which fires at VM instruction
boundaries. A runaway loop is stopped and the node fails on its `error` port
with code `LUA_TIMEOUT`, distinct from the `LUA_EXECUTION_ERROR` an ordinary
script fault produces.

Time spent *outside* the VM is not interrupted — inside a Rust callback, or
in the file IO a `require` performs. This is tight-loop protection, not a
universal watchdog.

The same budget bounds the validation run at policy-compile time, so a
top-level infinite loop is rejected by `PUT /api/policies` instead of hanging
the Admin API. A script whose top level legitimately takes longer than
`timeout_ms` to load will now fail to compile; raise the budget or move the
work into `execute`.
:::

Note: the UI node editor's runtime select also lists `python`; choosing it fails at policy-compile time, since only `lua` is implemented.

## Behavior

Scripts are loaded and validated once at policy-compile time, not per request: syntax errors, a failing top level, or a missing global `execute` function reject the policy immediately. At request time each execution runs in a fresh Lua VM, so scripts cannot leak state between requests.

`require` is sandboxed to `modules_path`: module names containing `..`, `/`, or `\` are rejected, and modules resolve as `<modules_path>/<name>.lua`. Modules are re-evaluated on every `require` (no caching). With no `modules_path` (e.g. `inline` without an explicit setting), `require` is unavailable.

The context rebuilt from the script's return value flows through the **success** port, or through **respond** when the script returned `ctx, "respond"` (see [Ports](#ports)). `context.errors` and the wire protocol are not exposed to scripts and are carried over unchanged. Any failure routes the *original* context through the **error** port with one of these codes appended to `context.errors` — see [Errors](#errors).

## Ports

| Port | Kind | Meaning |
|---|---|---|
| `success` | success | The script returned the context; the request continues. |
| `respond` | outcome | The script prepared `ctx.response` and asked to answer with it — `return ctx, "respond"`. Wire to `client` (or a custom handler). **Mandatory wiring**, like every outcome port. |
| `error` | error | Load, marshal, execution, timeout or unmarshal failure, or an unknown port name; the *original* context, unchanged. |

A script names the port it leaves on with an optional second return value:

```lua
function execute(ctx)
    if blocked(ctx) then
        ctx.response.status_code = 403
        ctx.response.body = '{"error": "forbidden"}'
        return ctx, "respond"     -- answer now; the upstream never runs
    end
    return ctx                    -- same as `return ctx, "success"`
end
```

Nothing is inferred: a script that sets `ctx.response.status_code` and returns one value continues on `success`, and the upstream replaces that response. `respond` places no constraint on `ctx.response` either — returning it with nothing prepared answers with whatever the response holds. Only `"respond"` and `"success"` are accepted names; anything else is `LUA_BAD_PORT` (below).

## Errors

The node returns the Context with an error, so the graph engine routes through the `error` port and appends the error to `context.errors`. It prepares no response of its own: what the caller sees is decided by the policy's `error` wiring, an [`error-handler`](error-handler.md), or the gateway's default 500.

| Code | Status | When |
|---|---|---|
| `LUA_LOAD_ERROR` | — | The script failed to load into the VM. |
| `LUA_MARSHAL_ERROR` | — | The Context could not be converted to a Lua table. |
| `LUA_MISSING_EXECUTE` | — | No global `execute` function was found. |
| `LUA_EXECUTION_ERROR` | — | The script raised a runtime error. |
| `LUA_UNMARSHAL_ERROR` | — | The returned table did not fit the `ctx` shape (the message names the field), or `execute` returned something that is not a table. |
| `LUA_BAD_PORT` | — | The second return value was not `"respond"` or `"success"`; the message shows what was returned. The mutated context is discarded. |

Syntax errors, a failing top level and a missing `execute` are caught at policy-compile time, not per request.
