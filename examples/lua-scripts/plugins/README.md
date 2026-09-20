# Lua scripts

Mounted at `/etc/gateway/plugins` by [`../compose.yaml`](../compose.yaml); the
`script` nodes in [`../config/gateway.yaml`](../config/gateway.yaml) reference
them by that path.

| File | What it does |
|---|---|
| `add-request-id.lua` | Injects a unique `X-Request-Id` header into every request |
| `block-user-agents.lua` | Rejects known bot/scraper User-Agents with a `403` prepared in the script, returned with `"respond"` so the node leaves on its `respond` port |
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
    # wire <id>.respond → client.in as well as <id>.success
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

To answer a request from a script, prepare `ctx.response` and `return ctx, "respond"`;
the node's `respond` port must be wired (to `client`, usually). Setting `ctx.response`
alone does not stop the request — the upstream replaces it. The
[Lua scripting guide](https://featherbitplatform.github.io/gateway/docs/guides/lua-scripting)
covers the `ctx` shape in full, `require`, timeouts and the sandbox.

Lua (Luau) is the only scripting runtime; a Python runtime is on the roadmap but
not implemented.
