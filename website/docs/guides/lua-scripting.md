---
title: Lua Scripting
description: Write custom plugins in Lua with the execute(ctx) contract, sandboxed require, and error-port routing.
---

The `script` plugin runs user-provided Lua (Luau runtime, via mlua) as a graph node behind the same plugin contract as native plugins. A script node sits anywhere in a policy graph; whatever it writes into the context is visible to downstream nodes.

## The `execute(ctx)` contract

Every script must define a global `execute` function that receives the context as a Lua table and returns it (possibly modified):

```lua
function execute(ctx)
    -- read request data
    local path = ctx.request.path
    local auth = ctx.request.headers["authorization"]

    -- modify the request
    ctx.request.headers["x-custom"] = {"injected-value"}

    -- pass data to downstream plugins
    ctx.message.processed_by = "lua-plugin"

    return ctx
end
```

### The `ctx` table

| Field | Contents |
|---|---|
| `ctx.request` | `method`, `path`, `host`, `scheme`, `remote_addr` (strings); `headers` and `query_params` (maps of name → 1-indexed array of strings); `body` (string) |
| `ctx.response` | `status_code` (integer), `headers` (same map-of-arrays shape), `body` (string) |
| `ctx.message` | Free-form key/value map shared between all plugins in the chain; values are converted to/from JSON (arrays become 1-indexed tables, objects become string-keyed tables) |

Two context fields are **not** exposed to scripts: the wire `protocol` and the `errors` accumulated by earlier nodes. Both are carried through a script node unchanged.

The returned table must keep `request` and `response` (including their `headers` and bodies) well-formed — malformed shapes fail unmarshalling; `query_params` and `message` are optional.

### Fresh VM per execution

A fresh Lua VM is created for every execution — only the source text is retained between calls. Scripts cannot leak or persist state between requests; use `ctx.message` to pass data along the chain within a single request.

## Configuring a script node

```yaml
- id: custom-logic
  type: script
  config:
    runtime: lua
    source: /etc/gateway/plugins/custom.lua
    # or inline:
    # inline: |
    #   function execute(ctx) ... end
```

| Key | Type | Default | Description |
|---|---|---|---|
| `runtime` | string | `lua` | Scripting runtime; any other value is a config error |
| `source` | string | — | Path to a script file, read at policy-compile time |
| `inline` | string | — | Script text embedded in the config. One of `source` or `inline` is required; `source` wins if both are set |
| `timeout_ms` | integer | `5000` | Script execution timeout — see warning below |
| `modules_path` | string | the `source` script's parent directory (none for `inline`) | Directory the sandboxed `require` resolves modules from |

:::warning timeout_ms is not enforced yet
`timeout_ms` is parsed and stored by the Lua runtime but **not currently enforced** by the VM. A long-running script is not interrupted. Treat the key as forward-looking configuration.
:::

### Validation at policy-compile time

Scripts are loaded and validated when the policy is compiled (at startup, on hot-reload, or when saved via the Admin API) — not per request. Compilation fails early if:

- the source file is unreadable, or neither `source` nor `inline` is set;
- the script has syntax errors or its top level errors on load;
- the script does not define a global `execute` function.

## Worked example

`examples/lua-scripts/plugins/block-user-agents.lua` flags known bot/scraper user agents in `ctx.message`. It does not write the rejection itself: a script that sets `ctx.response` before the upstream sees that response replaced when the upstream runs, so the policy branches on the flag with a `condition` node and answers from a `response-rewrite` node:

```lua
-- block-user-agents.lua
-- Flags requests from specific User-Agent patterns (scrapers, bots) by
-- setting ctx.message.blocked_ua. The policy branches on it with a
-- `condition` node and answers 403 from a `response-rewrite` node.
--
-- A script cannot reject a request by writing ctx.response before the
-- upstream: the upstream node replaces the response. Branch instead.

local blocked_patterns = {
    "python%-requests",
    "scrapy",
    "wget",
    "go%-http%-client",
}

function execute(ctx)
    local ua_list = ctx.request.headers["user-agent"]
    if not ua_list then
        return ctx
    end

    local ua = ua_list[1] or ""
    local ua_lower = string.lower(ua)

    for _, pattern in ipairs(blocked_patterns) do
        if string.find(ua_lower, pattern) then
            ctx.message.blocked_ua = ua
            return ctx
        end
    end

    return ctx
end
```

Wired into a policy (from `examples/lua-scripts/config/gateway.yaml`, abridged):

```yaml
policies:
  - name: scripted-policy
    error_handler: error-handler
    nodes:
      - id: listener
        type: listener
      - id: block-bots
        type: script
        config:
          runtime: lua
          source: /etc/gateway/plugins/block-user-agents.lua
      - id: is-bot
        type: condition
        config:
          conditions:
            - ["msg_blocked_ua", "!=", ""]
      - id: reject
        type: response-rewrite
        config:
          status_code: 403
          body: '{"error": "forbidden", "message": "Blocked user agent"}'
      - id: backend
        type: upstream
        config:
          targets:
            - host: ${UPSTREAM_HOST:-whoami}
              port: ${UPSTREAM_PORT:-80}
      - id: client
        type: client
    edges:
      - from: listener.out
        to: block-bots.in
      - from: block-bots.success
        to: is-bot.in
      - from: is-bot.true
        to: reject.in
      - from: reject.success
        to: client.in
      - from: is-bot.false
        to: backend.in
      - from: backend.success
        to: client.in
```

Run it with `docker compose -f examples/lua-scripts/compose.yaml up`, then `curl -A scrapy/2.0 -i http://localhost:8080/api/users` for the 403 and a plain `curl` for the proxied response.

The `examples/lua-scripts/plugins/` directory also ships `add-request-id.lua` (injects an `X-Request-Id` header) and `response-timer.lua` (two instances of the same script, before and after the upstream, add an `X-Response-Time` header via `ctx.message`).

## Sandboxed `require` and shared modules

Scripts can import shared modules with `require("name")`, resolved as `<modules_path>/<name>.lua`. The loader is sandboxed:

- Module names containing `..`, `/`, or `\` are rejected — no path traversal; only files directly inside `modules_path` can be loaded.
- When no `modules_path` applies (e.g. `inline` scripts without an explicit `modules_path`), `require` is not installed at all.
- Modules are re-evaluated on every `require`; results are **not cached**.

A shared module returns a table (`examples/lua-scripts/plugins/helpers.lua`):

```lua
-- helpers.lua
local M = {}

local counter = 0
function M.generate_id()
    counter = counter + 1
    return string.format("%s-%d", os.clock(), counter)
end

function M.contains_ci(str, pattern)
    return string.find(string.lower(str), string.lower(pattern)) ~= nil
end

return M
```

And a script imports it (`examples/lua-scripts/plugins/with-require-example.lua`, abridged):

```lua
local helpers = require("helpers")

function execute(ctx)
    local request_id = helpers.generate_id()
    ctx.request.headers["x-request-id"] = { request_id }
    ctx.message.request_id = request_id
    return ctx
end
```

By default `helpers.lua` just needs to sit in the same directory as the script; set `modules_path` explicitly to load modules from elsewhere.

## Error handling

Every script failure mode returns a plugin error carrying the original context, so the graph engine routes through the node's **error port** exactly like a native plugin failure (see [Error handling](../concepts/error-handling.md)):

| Error code | Cause |
|---|---|
| `LUA_LOAD_ERROR` | The script source failed to load into the VM |
| `LUA_MARSHAL_ERROR` | The context could not be marshalled into a Lua table |
| `LUA_MISSING_EXECUTE` | No global `execute` function was found |
| `LUA_EXECUTION_ERROR` | The script raised a runtime error (e.g. `error(...)`) |
| `LUA_UNMARSHAL_ERROR` | The returned table could not be rebuilt into a context |

## Hot-reload of scripts

Script sources referenced by `source` are read when the policy is compiled. Any configuration reload — file-watcher trigger, `POST /api/config/reload`, or a policy save from the Web UI — re-reads and re-validates the script files. Because the file watcher monitors the config file's parent directory recursively, editing a script file that lives under that directory also triggers a reload (see [Configuration](./configuration.md)).

:::note Planned
A Python scripting runtime (pyo3) is planned but not implemented; `runtime: lua` is the only supported value today.
:::

For the full config-key reference, see the [script plugin reference](../reference/plugins/script.md).
