# `script` node: a `respond` outcome port

**Status:** approved in discussion, 2026-09-20
**Depends on:** PR #64 (`examples/` reorganisation) — it moves the files this change edits.
**Target:** 0.11.0

## 1. Problem

A `script` node has two ports, `success` and `error`. A script that wants to *answer* a request — reject a bot, serve a canned response, redirect — has no way to say so: whatever it writes into `ctx.response` before the upstream is replaced when the upstream runs. The Lua guide claimed otherwise for a year ("rejects known bot user agents by writing a response directly"); making `examples/lua-scripts` runnable in #64 showed every blocked agent getting a `200`. #64 documents the workaround (flag `ctx.message`, branch with `condition`, answer with `response-rewrite`), which is three nodes for one decision the script already made.

Every other node in the graph that deliberately short-circuits does it on a declared **outcome port** — `denied`, `redirect`, `abort`, `hit`, `preflight` — with the meaning "a response is prepared; wire to `client` (or a custom handler)". `script` should have the same shape.

## 2. Design

### 2.1 Port

`script` gets a static `PortSpec` of its own (`src/plugins/ports.rs`):

| Port | Kind | Meaning |
|---|---|---|
| `success` | success | The script returned the context; the request continues. |
| `respond` | outcome | The script prepared `ctx.response` and asked to answer with it; wire to `client` (or a custom handler). |
| `error` | error | Load, marshal, execution, timeout or unmarshal failure — the *original* context, unchanged. |

`port_spec("script")` maps to it (`src/plugins/mod.rs`). Nothing else consults a list: the compiler, the debug traces, `/api/plugins`, the MCP `get_node_type`, the UI's port rows and palette all read the spec.

### 2.2 How a script takes the port

Explicitly, by a second return value naming the port:

```lua
function execute(ctx)
    if blocked(ctx) then
        ctx.response.status_code = 403
        ctx.response.body = '{"error": "forbidden"}'
        return ctx, "respond"
    end
    return ctx
end
```

| Script returns | Node leaves on |
|---|---|
| `ctx` (one value) | `success` — every existing script is unchanged |
| `ctx, "respond"` | `respond` |
| `ctx, "success"` | `success` — allowed, so a script can be explicit |
| `ctx, <anything else>` (other string, non-string, `nil` counts as absent) | `error`, code `LUA_BAD_PORT`, message naming what was returned and the two accepted values |

Nothing is inferred. A script that sets `ctx.response.status_code` and returns one value continues on `success`, exactly as today: the gateway does not guess that "status was set" means "stop here". The port is a decision the script states.

`respond` carries no constraint on `ctx.response`: a script may return `ctx, "respond"` with an empty response and the client gets whatever that is (a 200 with no body), the same as any other node that leaves on an outcome port with an unprepared response. That is the script author's call, not the runtime's.

The port may be taken anywhere in the graph, before or after the upstream. Before the upstream it is a short-circuit; after, it is a plain alternate exit. The engine does not care.

### 2.3 Runtime change

`LuaRuntime::execute` returns `Result<(Context, Option<&'static str>), PluginExecutionError>`. The call becomes `execute_fn.call::<LuaMultiValue>(ctx_table)`; the first value must be a table (else the existing `LUA_EXECUTION_ERROR`/`LUA_UNMARSHAL_ERROR` paths apply, unchanged), the second is read as described above. `ScriptPlugin::execute` maps `Some("respond")` to `PluginOutput::on_port(ctx, "respond")` and `None` to `PluginOutput::success(ctx)`.

`LUA_BAD_PORT` is a failure like the others in that method: the **original** context goes down the `error` port with the error appended; the mutated table is discarded. A script that names an unknown port did not finish making a decision, and the gateway will not pick one for it.

### 2.4 Streaming

`script` is not in the set of nodes that opt out of `reads_response_body()` — it marshals the body into Lua, so an upstream with a script downstream buffers, as today. The engine invariant "opt-out nodes never `Err` from `execute`" is untouched. Nothing here changes what streams.

## 3. The breaking change

`PortSpec` is per node **type**, and every `success`/outcome port must be wired. So after this change **every existing `script` node must wire `respond`**, or the policy fails to compile with the existing "unwired port" error naming the node and the port. There is no opt-in: a static spec is what lets the compiler, the UI and the agents agree on a node's shape without executing it, and a per-instance exception would be the first of its kind.

Precedent: `dingtalk-auth`/`feishu-auth` gained a mandatory `redirect` port in the same way. Mitigation:

- Release note under **Breaking** in 0.11.0, with the one-line fix (`- { from: <script>.respond, to: client.in }`).
- No shipped policy is affected: the repo's own configs, tests and e2e fixtures have no `script` node outside `examples/lua-scripts` and the docs, both of which this change updates.
- The compile error is the same one every unwired outcome port produces; the UI shows the unwired port row on the node.

## 4. Examples and docs

- `examples/lua-scripts/plugins/block-user-agents.lua` writes the 403 again and ends `return ctx, "respond"`; the `is-bot` `condition` and `reject` `response-rewrite` nodes added in #64 are removed; the edge `block-bots.respond → client.in` is added. `plugins/README.md`, `examples/README.md` and the compose header comment follow.
- `website/docs/reference/plugins/script.md`: **Ports** section with the table above; the return protocol under **Behavior**; `LUA_BAD_PORT` in **Errors**.
- `website/docs/guides/lua-scripting.md`: the worked example and wiring excerpt revert to the direct form with the `respond` edge; the sentence "a script cannot reject a request…" becomes "to answer from a script, `return ctx, "respond"` and wire the port".
- `src/mcp/server.rs` instructions (agents write scripts from them): add *"To answer the request from the script, prepare `ctx.response` and `return ctx, "respond"`; the node's `respond` port must be wired."*
- `website/docs/reference/roadmap.md`: the Lua row mentions the port; release notes carry the breaking entry.

## 5. Tests

| Level | Pins |
|---|---|
| `lua_runtime` unit | `return ctx` → `(ctx, None)`; `return ctx, "respond"` → `Some("respond")`; `return ctx, "success"` → `None`; `return ctx, "client"` → `Err` with `LUA_BAD_PORT`, **original** context (a header the script added must be absent); `return ctx, 42` → `LUA_BAD_PORT`; `return ctx, nil` → `None`. |
| `ports` | `test_every_known_type_has_a_valid_spec` (existing) covers the new spec's shape. |
| `graph::engine` | a policy with a `script` node and no `respond` edge fails to compile naming `respond` — the breaking change is deliberate and stays visible; the same policy with the edge compiles and, executed with a blocking script, the request leaves the node on `respond`. |
| `script` plugin | `execute` on a script returning `ctx, "respond"` yields `PluginOutput.port == Some("respond")`. |
| e2e `E2E-SCRIPT-01` | a data-plane route with a `script` node: `User-Agent: scrapy/2.0` → `403` from the script's own body; a browser UA → `200` proxied. New `e2e/tests/script.spec.ts`; the script source is written to a temp dir the test owns and referenced by absolute path. |

Mutation check: with `respond` mapped to `success` in `ScriptPlugin::execute`, the engine and e2e tests must fail (the upstream overwrites the 403).

## 6. Out of scope

- Naming arbitrary ports from a script (`return ctx, "denied"`): only `respond` and `success` are accepted. If a second outcome ever earns its place it gets its own spec entry and its own reasoning.
- Optional-wiring outcome ports. Rejected in discussion: it would be the first exception to the mandatory-outcome rule, and the rule is what lets a policy's short-circuits be read off the graph.
- Any change to what `ctx` exposes or to `require`, timeouts or the sandbox.
