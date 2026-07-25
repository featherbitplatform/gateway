# Example Plugins

## Lua Plugins

| File | Description |
|---|---|
| `add-request-id.lua` | Injects a unique `X-Request-Id` header into every request |
| `block-user-agents.lua` | Blocks requests from known bot/scraper User-Agents (returns 403) |
| `response-timer.lua` | Measures request duration — use two instances (before and after upstream) to add an `X-Response-Time` header |

## Python Plugins

> Python runtime (`pyo3`) is not yet implemented. These examples show the target API.

| File | Description |
|---|---|
| `add_request_id.py` | Injects a UUID `X-Request-Id` using Python's `uuid` module |
| `jwt_claims_enricher.py` | Maps JWT claims (from the `jwt-auth` native plugin) into `X-User-Id`, `X-User-Email`, `X-User-Roles` headers |
| `response_transformer.py` | Strips internal headers, injects security headers, wraps JSON responses in a standard `{"success", "status", "data"}` envelope |

## Usage

Reference a script in your routing policy:

```yaml
- id: my-plugin
  type: script
  config:
    runtime: lua          # or "python" when available
    source: examples/plugins/add-request-id.lua
```

See `examples/gateway-with-scripts.yaml` for a complete routing policy that chains multiple Lua plugins together.

## Writing Your Own

Every script must define an `execute` function:

**Lua:**
```lua
function execute(ctx)
    -- read/modify ctx.request, ctx.response, ctx.message
    return ctx
end
```

**Python:**
```python
def execute(ctx):
    # read/modify ctx["request"], ctx["response"], ctx["message"]
    return ctx
```

The context structure:
- `ctx.request` — `method`, `path`, `host`, `scheme`, `headers` (map of string→array), `query_params`, `body`, `remote_addr`
- `ctx.response` — `status_code`, `headers`, `body`
- `ctx.message` — free-form key/value map shared between all plugins in the chain
