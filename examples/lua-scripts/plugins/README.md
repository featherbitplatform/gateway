# Lua scripts

Mounted at `/etc/gateway/plugins` by [`../compose.yaml`](../compose.yaml); the
`script` nodes in [`../config/gateway.yaml`](../config/gateway.yaml) reference
them by that path.

| File | What it does |
|---|---|
| `add-request-id.lua` | Injects a unique `X-Request-Id` header into every request |
| `block-user-agents.lua` | Flags known bot/scraper User-Agents in `ctx.message.blocked_ua`; the policy's `condition` node branches on it and a `response-rewrite` answers `403` |
| `response-timer.lua` | Measures request duration — wired twice, before and after `upstream`, it adds an `X-Response-Time` header via `ctx.message` |
| `helpers.lua` | A shared module returning a table, for `require` |
| `with-require-example.lua` | Imports `helpers.lua` — how to share code between scripts |

## Referencing a script

```yaml
- id: request-id
  type: script
  config:
    runtime: lua
    source: /etc/gateway/plugins/add-request-id.lua
    timeout_ms: 5000        # optional; a runaway script is interrupted, not left pinning a worker
```

## Writing your own

Every script defines an `execute` function that receives the context table,
mutates it, and returns it:

```lua
function execute(ctx)
    ctx.request.headers["x-hello"] = { "world" }
    return ctx
end
```

- `ctx.request` — `method`, `path`, `host`, `scheme`, `headers` (map of name → array of strings), `query_params`, `body`, `remote_addr`
- `ctx.response` — `status_code`, `headers`, `body`
- `ctx.message` — free-form key/value map shared by every node in the chain (readable elsewhere as `$msg_<key>`)

A script cannot reject a request by writing `ctx.response` before the upstream --
the upstream replaces the response. Set a key in `ctx.message` and branch on it
with a `condition` node, as `block-user-agents.lua` does. The
[Lua scripting guide](https://featherbitplatform.github.io/gateway/docs/guides/lua-scripting)
covers the `ctx` shape in full, `require`, timeouts and the sandbox.

Lua (Luau) is the only scripting runtime; a Python runtime is on the roadmap but
not implemented.
