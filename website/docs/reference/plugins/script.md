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
| `timeout_ms` | integer | `5000` | Script execution timeout. Currently stored but **not enforced** — see the warning below. |
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

:::warning
`timeout_ms` is parsed and stored but not yet enforced by the Lua VM. A script that loops forever will block the request indefinitely.
:::

Note: the UI node editor's runtime select also lists `python`; choosing it fails at policy-compile time, since only `lua` is implemented.

## Behavior

Scripts are loaded and validated once at policy-compile time, not per request: syntax errors, a failing top level, or a missing global `execute` function reject the policy immediately. At request time each execution runs in a fresh Lua VM, so scripts cannot leak state between requests.

`require` is sandboxed to `modules_path`: module names containing `..`, `/`, or `\` are rejected, and modules resolve as `<modules_path>/<name>.lua`. Modules are re-evaluated on every `require` (no caching). With no `modules_path` (e.g. `inline` without an explicit setting), `require` is unavailable.

On success the context rebuilt from the script's return value flows through the **success** port. `context.errors` and the wire protocol are not exposed to scripts and are carried over unchanged. Any failure routes the *original* context through the **error** port with one of these codes appended to `context.errors`:

| Code | Meaning |
|---|---|
| `LUA_LOAD_ERROR` | The script failed to load into the VM. |
| `LUA_MARSHAL_ERROR` | The Context could not be converted to a Lua table. |
| `LUA_MISSING_EXECUTE` | No global `execute` function was found. |
| `LUA_EXECUTION_ERROR` | The script raised a runtime error. |
| `LUA_UNMARSHAL_ERROR` | The returned table could not be converted back to a Context. |
