# `script` `respond` Port Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let a Lua script answer a request itself — `return ctx, "respond"` leaves the `script` node on a new `respond` outcome port, the same shape every other deliberate short-circuit in the graph already has.

**Architecture:** `script` gets its own static `PortSpec` (`success`, `respond`, `error`). `LuaRuntime::execute` reads an optional second return value naming the port and hands `(Context, Option<&'static str>)` back; `ScriptPlugin::execute` turns that into `PluginOutput::on_port` or `success`. Nothing is inferred from `ctx.response`; an unknown second value is a `LUA_BAD_PORT` failure on `error` with the original context. Everything downstream — compiler, traces, `/api/plugins`, MCP `get_node_type`, UI port rows — reads the spec.

**Tech Stack:** Rust, `mlua` (Luau) `LuaMultiValue`, axum admin API (unchanged), Playwright e2e.

**Spec:** `docs/superpowers/specs/2026-09-20-script-respond-port-design.md`

**Ordering constraint:** Tasks 1–2 touch only `src/` and `website/docs`. **Task 3 edits `examples/lua-scripts/…`, which PR #64 creates** — before dispatching Task 3, confirm #64 is merged into `develop` and `git rebase develop` this branch (or merge develop in). If #64 is not merged yet, stop after Task 2 and wait.

## Global Constraints

- **Explicit, never inferred.** `return ctx` → `success`; `return ctx, "respond"` → `respond`; `return ctx, "success"` → `success`; `return ctx, nil` → `success`; any other second value → `error` with code `LUA_BAD_PORT`. A script that sets `ctx.response.status_code` and returns one value continues on `success` exactly as today.
- **`LUA_BAD_PORT` routes the ORIGINAL context** down `error` (the mutated table is discarded), like every other failure in `LuaRuntime::execute`.
- **`respond` places no constraint on `ctx.response`.** The runtime does not check that a status was set.
- **`respond` is mandatory-wired on every `script` node** (static per-type `PortSpec`). This is a deliberate breaking change; a test pins that a script node without a `respond` edge fails to compile.
- **Only `respond` and `success` are accepted names.** No general "return any port".
- **`script` keeps reading the response body** (`reads_response_body` unchanged); nothing about streaming changes.
- No `src/lib.rs`: code lands with its first caller; test-only items take `#[cfg(test)]`; never `#[allow(dead_code)]`. Lint with `cargo clippy --all-targets --locked -- -D warnings` AND `--no-default-features`.
- Run the **full** `cargo test` (the `admin::policies` docs/catalog drift tests and `ports::test_every_known_type_has_a_valid_spec` are what catch a half-registered port). Also `cargo test --release`.
- Commands run in the foreground; mutation checks restore from a file copy, never `git checkout --`.
- Commit style: Conventional Commits, no `Co-Authored-By`, no AI attribution. Branch `feature/script-respond-port`, off `develop`.

## File Structure

| File | Responsibility |
|---|---|
| `src/plugins/ports.rs` (modify) | `SCRIPT_SPEC` — the port contract |
| `src/plugins/mod.rs` (modify) | `port_spec("script")` mapping |
| `src/plugins/script/lua_runtime.rs` (modify) | read the second return value; `LUA_BAD_PORT`; return `(Context, Option<&'static str>)` |
| `src/plugins/script/mod.rs` (modify) | map the port to `PluginOutput` |
| `src/graph/engine.rs` (tests only) | pin the breaking wiring rule and the end-to-end short-circuit |
| `src/mcp/server.rs` (modify) | one sentence in the agent instructions |
| `website/docs/reference/plugins/script.md`, `website/docs/guides/lua-scripting.md` | port table, return protocol, worked example |
| `examples/lua-scripts/…`, `e2e/tests/script.spec.ts`, `e2e/E2E_TESTBOOK.md`, `website/docs/reference/roadmap.md` | the runnable example, `E2E-SCRIPT-01`, release-note row |

---

### Task 1: The port — spec, runtime, plugin

**Files:**
- Modify: `src/plugins/ports.rs` (add `SCRIPT_SPEC` beside `FAULT_INJECTION_SPEC`, ~line 105)
- Modify: `src/plugins/mod.rs` (`port_spec` match, ~line 513: add `"script" => Some(&ports::SCRIPT_SPEC),`)
- Modify: `src/plugins/script/lua_runtime.rs` (`execute`, lines ~89–165; tests from line 581)
- Modify: `src/plugins/script/mod.rs` (`impl Plugin for ScriptPlugin`, ~line 135)

**Interfaces:**
- Produces: `pub const SCRIPT_SPEC: PortSpec` with outputs `[SUCCESS, respond (Outcome), ERROR]`; `LuaRuntime::execute(&self, ctx: Context) -> Result<(Context, Option<&'static str>), PluginExecutionError>` where the `Option` is `None` or `Some("respond")`; error code string `"LUA_BAD_PORT"`; `pub(crate) const RESPOND_PORT: &str = "respond"` in `lua_runtime.rs`.
- Consumes: `PluginOutput::on_port(ctx, &'static str)` (`src/plugins/mod.rs:40`), `failure_code`, `lua_to_context` (existing).

- [ ] **Step 1: Write the failing runtime tests**

In `src/plugins/script/lua_runtime.rs`'s `mod tests`, after `test_lua_preserves_errors_and_protocol`:

```rust
    /// A bare `return ctx` is the whole existing contract: no port named,
    /// the node continues on success. Every script written so far relies on it.
    #[test]
    fn test_lua_single_return_takes_success() {
        let rt = LuaRuntime::new("function execute(ctx) return ctx end", 5000, None).unwrap();
        let (_, port) = rt.execute(test_context()).unwrap();
        assert_eq!(port, None);
    }

    /// The feature: a script names the port it wants to leave on.
    #[test]
    fn test_lua_second_return_respond_takes_the_respond_port() {
        let rt = LuaRuntime::new(
            r#"
            function execute(ctx)
                ctx.response.status_code = 403
                ctx.response.body = "blocked"
                return ctx, "respond"
            end
            "#,
            5000,
            None,
        )
        .unwrap();
        let (ctx, port) = rt.execute(test_context()).unwrap();
        assert_eq!(port, Some(RESPOND_PORT));
        assert_eq!(ctx.response.status_code, 403, "the prepared response travels with the port");
    }

    /// Naming success explicitly is allowed, so a script can be spelled out.
    #[test]
    fn test_lua_second_return_success_is_plain_success() {
        let rt = LuaRuntime::new("function execute(ctx) return ctx, \"success\" end", 5000, None).unwrap();
        let (_, port) = rt.execute(test_context()).unwrap();
        assert_eq!(port, None);
    }

    /// `nil` is "no second value", not a bad port: `return ctx, maybe_port`
    /// with an unset local must keep working.
    #[test]
    fn test_lua_second_return_nil_is_absent() {
        let rt = LuaRuntime::new("function execute(ctx) return ctx, nil end", 5000, None).unwrap();
        let (_, port) = rt.execute(test_context()).unwrap();
        assert_eq!(port, None);
    }

    /// An unknown port name is a failure, and the ORIGINAL context goes down
    /// error: the script did not finish making a decision, so nothing it
    /// wrote is kept. The header it added must be absent from the error path.
    #[test]
    fn test_lua_unknown_port_is_lua_bad_port_with_the_original_context() {
        let rt = LuaRuntime::new(
            r#"
            function execute(ctx)
                ctx.request.headers["x-mutated"] = { "yes" }
                return ctx, "client"
            end
            "#,
            5000,
            None,
        )
        .unwrap();
        let err = rt.execute(test_context()).unwrap_err();
        assert_eq!(err.error.code, "LUA_BAD_PORT");
        assert!(err.error.message.contains("client"), "{}", err.error.message);
        assert!(err.error.message.contains("respond"), "the message names the accepted values: {}", err.error.message);
        assert!(
            !err.context.request.headers.contains_key("x-mutated"),
            "the mutated table must be discarded on a bad port"
        );
    }

    /// A non-string second value is the same failure, not a coercion.
    #[test]
    fn test_lua_non_string_port_is_lua_bad_port() {
        let rt = LuaRuntime::new("function execute(ctx) return ctx, 42 end", 5000, None).unwrap();
        let err = rt.execute(test_context()).unwrap_err();
        assert_eq!(err.error.code, "LUA_BAD_PORT");
    }
```

Then update every existing test in that module that does `let result = rt.execute(ctx).unwrap();` to destructure the tuple: `let (result, _) = rt.execute(ctx).unwrap();` (there are ~6; `grep -n "rt.execute\|.execute(test_context())" src/plugins/script/lua_runtime.rs`).

In `src/plugins/script/mod.rs`, add a test module (there is none today):

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::{Context, GatewayRequest, GatewayResponse, Protocol};

    fn plugin(inline: &str) -> ScriptPlugin {
        let mut cfg = HashMap::new();
        cfg.insert("runtime".to_string(), serde_json::json!("lua"));
        cfg.insert("inline".to_string(), serde_json::json!(inline));
        ScriptPlugin::from_config(&cfg).unwrap()
    }

    fn ctx() -> Context {
        Context {
            request: GatewayRequest {
                method: "GET".to_string(),
                path: "/".to_string(),
                host: "localhost".to_string(),
                scheme: "http".to_string(),
                headers: HashMap::new(),
                query_params: HashMap::new(),
                body: bytes::Bytes::new(),
                remote_addr: "127.0.0.1:1".to_string(),
                protocol: Protocol::Http1,
            },
            response: GatewayResponse {
                status_code: 0,
                headers: HashMap::new(),
                body: bytes::Bytes::new(),
                stream: None,
            },
            message: HashMap::new(),
            errors: Vec::new(),
        }
    }

    /// The plugin is where the runtime's answer becomes a graph port.
    #[tokio::test]
    async fn test_respond_becomes_the_respond_port() {
        let out = plugin("function execute(ctx) return ctx, \"respond\" end")
            .execute(ctx())
            .await
            .unwrap();
        assert_eq!(out.port, Some("respond"));
    }

    #[tokio::test]
    async fn test_plain_return_is_success() {
        let out = plugin("function execute(ctx) return ctx end").execute(ctx()).await.unwrap();
        assert_eq!(out.port, None);
    }
}
```

(If `GatewayRequest`/`GatewayResponse`/`Protocol` live elsewhere than `crate::context`, copy the import path `lua_runtime.rs`'s test module uses.)

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --no-run 2>&1 | grep -E "^error" | head`
Expected: type errors — `rt.execute(...)` returns `Context`, not a tuple; `RESPOND_PORT` not found.

- [ ] **Step 3: Declare the port**

`src/plugins/ports.rs`, after `FAULT_INJECTION_SPEC`:

```rust
/// `script`: a script that prepared `ctx.response` and asked to answer with
/// it (`return ctx, "respond"`) exits on `respond`. It is the same shape as
/// `abort`/`denied`/`redirect`: a deliberate short-circuit on a declared
/// port, never inferred from the response the script left behind.
pub const SCRIPT_SPEC: PortSpec = PortSpec {
    input: Some("Request context from the previous node."),
    outputs: &[
        SUCCESS,
        PortDecl {
            name: "respond",
            kind: PortKind::Outcome,
            description: "The script prepared ctx.response and returned it with \"respond\"; wire to client (or a custom handler).",
        },
        ERROR,
    ],
};
```

`src/plugins/mod.rs`, in `port_spec`: `"script" => Some(&ports::SCRIPT_SPEC),` next to `"fault-injection"`.

- [ ] **Step 4: Read the second return value**

`src/plugins/script/lua_runtime.rs`. Near the top:

```rust
/// The one outcome port a script may name: `return ctx, "respond"`.
pub(crate) const RESPOND_PORT: &str = "respond";
```

Change the signature to `pub fn execute(&self, ctx: Context) -> Result<(Context, Option<&'static str>), PluginExecutionError>` and replace the `execute_fn.call(ctx_table)` block:

```rust
        let returned: LuaMultiValue = match execute_fn.call(ctx_table) {
            Ok(v) => v,
            Err(e) => {
                return Err(PluginExecutionError {
                    context: ctx,
                    error: GatewayError {
                        node_id: String::new(),
                        code: failure_code(&timed_out, "LUA_EXECUTION_ERROR"),
                        message: format!("Lua execution error: {}", e),
                        metadata: HashMap::new(),
                    },
                });
            }
        };
        let mut returned = returned.into_iter();

        let result_table: LuaTable = match returned.next() {
            Some(LuaValue::Table(t)) => t,
            other => {
                return Err(PluginExecutionError {
                    context: ctx,
                    error: GatewayError {
                        node_id: String::new(),
                        code: "LUA_UNMARSHAL_ERROR".to_string(),
                        message: format!(
                            "execute(ctx) must return the ctx table; got {}",
                            other.map(|v| v.type_name()).unwrap_or("nothing")
                        ),
                        metadata: HashMap::new(),
                    },
                });
            }
        };

        // The optional second value names the port the node leaves on. It is
        // a decision the script states, never something inferred from the
        // response it left behind: a script that set status 403 and returned
        // one value continues on success, exactly as before this existed.
        let port = match returned.next() {
            None | Some(LuaValue::Nil) => None,
            Some(LuaValue::String(s)) if s.to_str().map(|s| s == RESPOND_PORT).unwrap_or(false) => {
                Some(RESPOND_PORT)
            }
            Some(LuaValue::String(s)) if s.to_str().map(|s| s == "success").unwrap_or(false) => None,
            Some(other) => {
                let shown = match &other {
                    LuaValue::String(s) => format!("\"{}\"", s.to_string_lossy()),
                    v => v.type_name().to_string(),
                };
                return Err(PluginExecutionError {
                    context: ctx,
                    error: GatewayError {
                        node_id: String::new(),
                        code: "LUA_BAD_PORT".to_string(),
                        message: format!(
                            "execute(ctx) returned {} as the port; expected \"respond\" or \"success\" (or no second value)",
                            shown
                        ),
                        metadata: HashMap::new(),
                    },
                });
            }
        };
```

and make the tail return the tuple: `Ok(new_ctx) => Ok((new_ctx, port)),`. Import `LuaMultiValue` and `LuaValue` from `mlua` alongside the existing `LuaFunction`/`LuaTable` imports (check the exact prelude the file uses — it may be `mlua::prelude::*` or named imports). Note `LuaValue::type_name()` exists on `mlua::Value`; if the installed mlua names it differently, use `format!("{:?}", other)` for the non-string case and say so in the report.

Also update the doc comment above `execute` (line ~76) to say it "returns the context rebuilt from the table the script returned, and the port the script named, if any".

- [ ] **Step 5: Map it in the plugin**

`src/plugins/script/mod.rs`:

```rust
            ScriptRuntime::Lua(rt) => match rt.execute(ctx) {
                Ok((new_ctx, Some(port))) => Ok(PluginOutput::on_port(new_ctx, port)),
                Ok((new_ctx, None)) => Ok(PluginOutput::success(new_ctx)),
                Err(e) => Err(PluginExecutionError {
                    context: e.context,
                    error: e.error,
                }),
            },
```

Check whether anything else calls `LuaRuntime::execute` (`grep -rn "\.execute(" src/plugins/script src/debug src/graph | grep -i lua`) — the policy-compile validation in `from_config`/`prepare_policy` may run the script's top level only, not `execute`; if a caller exists, destructure the tuple there too.

- [ ] **Step 6: Run the tests, then the full matrix**

Run: `cargo test lua_ && cargo test plugins::script` → all pass, including the six new runtime tests and two plugin tests. Then `cargo fmt --all`, both clippy invocations, full `cargo test`. `ports::test_every_known_type_has_a_valid_spec` now covers `SCRIPT_SPEC`; the `admin::policies` docs tests are unaffected (no new node type).

- [ ] **Step 7: Mutation check**

Copy `src/plugins/script/mod.rs` to scratch. Change `Ok((new_ctx, Some(port))) => Ok(PluginOutput::on_port(new_ctx, port))` to `=> Ok(PluginOutput::success(new_ctx))`. Run `cargo test plugins::script` — `test_respond_becomes_the_respond_port` must FAIL. Restore from the copy. Record in the report.

- [ ] **Step 8: Commit**

```bash
git add src/plugins/ports.rs src/plugins/mod.rs src/plugins/script/lua_runtime.rs src/plugins/script/mod.rs
git commit -m "feat(script): respond outcome port, taken by return ctx, \"respond\"

A script that wanted to answer a request had no way to say so: whatever
it wrote into ctx.response before the upstream was replaced when the
upstream ran, and the Lua guide's worked example -- a bot blocker
\"writing a response directly\" -- had never blocked anything. Every other
deliberate short-circuit in the graph leaves on a declared outcome port
(denied, redirect, abort, hit); script now has one.

The port is stated, never inferred. return ctx is success, exactly as
before; return ctx, \"respond\" leaves on respond; anything else as the
second value is LUA_BAD_PORT on the error port with the original
context, because a script that named an unknown port did not finish
making a decision and the gateway will not pick one for it.

Breaking: PortSpec is per node type, so every existing script node must
wire respond or its policy no longer compiles. The compile error names
the port; the fix is one edge."
```

---

### Task 2: Pin the contract in the engine; document it

**Files:**
- Modify: `src/graph/engine.rs` (tests only, near `test_a_lone_cache_half_is_reported_not_rejected`)
- Modify: `website/docs/reference/plugins/script.md` (Behavior + Errors, lines ~84–110; add a Ports section)
- Modify: `website/docs/guides/lua-scripting.md` (worked example, lines ~76–150)
- Modify: `src/mcp/server.rs:29` (agent instructions)

**Interfaces:**
- Consumes: `SCRIPT_SPEC`, `RESPOND_PORT`, `LUA_BAD_PORT` (Task 1); `compile_test_policy(json)` / `compile_test_policy_err(json)` and `test_context(path)` helpers in `engine.rs` tests; the `mocking` node (`config: {response_status, response_example}`).

- [ ] **Step 1: Write the failing engine tests**

```rust
    /// The breaking change, kept visible: PortSpec is per node type, so a
    /// script node with no `respond` edge no longer compiles. The message
    /// names the port so the fix is obvious.
    #[tokio::test]
    async fn test_a_script_node_must_wire_respond() {
        let err = compile_test_policy_err(serde_json::json!({
            "nodes": [
                { "id": "listener", "type": "listener", "config": {} },
                { "id": "s", "type": "script",
                  "config": { "runtime": "lua", "inline": "function execute(ctx) return ctx end" } },
                { "id": "client", "type": "client", "config": {} }
            ],
            "edges": [
                { "from": "listener.out", "to": "s.in" },
                { "from": "s.success", "to": "client.in" }
            ]
        }));
        assert!(err.contains("respond") && err.contains("'s'"), "{err}");
    }

    /// End to end through the graph: the script's own 403 reaches the
    /// client because the node left on `respond`, skipping the upstream
    /// that would otherwise have replaced it. A browser UA takes success and
    /// gets the upstream's body -- the control that proves the branch.
    #[tokio::test]
    async fn test_a_script_taking_respond_short_circuits_the_upstream() {
        let graph = compile_test_policy(serde_json::json!({
            "nodes": [
                { "id": "listener", "type": "listener", "config": {} },
                { "id": "block", "type": "script", "config": { "runtime": "lua", "inline":
                    "function execute(ctx)\n  local ua = (ctx.request.headers[\"user-agent\"] or {})[1] or \"\"\n  if string.find(string.lower(ua), \"scrapy\") then\n    ctx.response.status_code = 403\n    ctx.response.body = \"blocked\"\n    return ctx, \"respond\"\n  end\n  return ctx\nend" } },
                { "id": "up", "type": "mocking", "config": { "response_status": 200, "response_example": "proxied" } },
                { "id": "client", "type": "client", "config": {} }
            ],
            "edges": [
                { "from": "listener.out", "to": "block.in" },
                { "from": "block.respond", "to": "client.in" },
                { "from": "block.success", "to": "up.in" },
                { "from": "up.success", "to": "client.in" }
            ]
        }));

        let mut bot = test_context("/x");
        bot.request.headers.insert("user-agent".to_string(), vec!["scrapy/2.0".to_string()]);
        let out = graph.execute(bot).await;
        assert_eq!(out.response.status_code, 403);
        assert_eq!(out.response.body, bytes::Bytes::from_static(b"blocked"));

        let mut browser = test_context("/x");
        browser.request.headers.insert("user-agent".to_string(), vec!["Mozilla/5.0".to_string()]);
        let out = graph.execute(browser).await;
        assert_eq!(out.response.status_code, 200);
        assert_eq!(out.response.body, bytes::Bytes::from_static(b"proxied"));
    }
```

If `compile_test_policy` does not accept a `"config": {}` key on `listener`/`client` or requires `"name"`, mirror exactly what the neighbouring tests pass. Check the `mocking` node's config keys in `website/docs/reference/plugins/mocking.md` if the compile fails on them.

- [ ] **Step 2: Run them to verify the state**

Run: `cargo test test_a_script_node_must_wire_respond test_a_script_taking_respond` — both should already PASS if Task 1 landed correctly (they pin behaviour, they do not drive new code). If the first FAILS, the `port_spec` mapping from Task 1 is missing; if the second FAILS, look at the `mocking` config or the header key casing. Then do the mutation check that makes them worth having: temporarily change `"script" => Some(&ports::SCRIPT_SPEC)` back to falling through to `DEFAULT_SPEC` (comment the arm out), run the first test — it must FAIL (the policy now compiles). Restore.

- [ ] **Step 3: Reference docs**

`website/docs/reference/plugins/script.md`. Add a **Ports** section before **Errors**:

````markdown
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
````

In **Behavior**, change *"On success the context rebuilt from the script's return value flows through the **success** port."* to *"The context rebuilt from the script's return value flows through the **success** port, or through **respond** when the script returned `ctx, "respond"` (see [Ports](#ports))."* In the **Errors** table add:

```markdown
| `LUA_BAD_PORT` | — | The second return value was not `"respond"` or `"success"`; the message shows what was returned. The mutated context is discarded. |
```

- [ ] **Step 4: Guide**

`website/docs/guides/lua-scripting.md`, the worked example (from *"`examples/lua-scripts/plugins/block-user-agents.lua` flags known bot/scraper user agents…"* through the wiring excerpt and the *"Run it with…"* line). Replace the prose sentence with:

> `examples/lua-scripts/plugins/block-user-agents.lua` rejects known bot/scraper user agents from the script itself: it prepares the 403 in `ctx.response` and returns `ctx, "respond"`, so the node leaves on its `respond` port and the upstream never runs. A script that only sets `ctx.response` and returns one value does **not** stop the request — the upstream replaces that response — which is why the port is named explicitly.

Replace the embedded Lua block with the Task 3 script verbatim (write the script first if you do the tasks out of order; the two must match byte for byte — a reader copies from the guide). Replace the wiring excerpt with:

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
      - from: block-bots.respond   # the script answered; straight to the client
        to: client.in
      - from: block-bots.success   # not a bot; on to the upstream
        to: backend.in
      - from: backend.success
        to: client.in
```

Keep the *"Run it with `docker compose -f examples/lua-scripts/compose.yaml up`…"* line. Add, after the `ctx` table section (wherever the guide lists what `execute` returns), one short paragraph: *"`execute` may return a second value naming the port to leave on: `"respond"` or `"success"`. Anything else fails the node with `LUA_BAD_PORT`, and the request takes the `error` port with the context as it was before the script ran."*

- [ ] **Step 5: Agent instructions**

`src/mcp/server.rs:29`, after *"Mutate the table you were given and `return ctx`;"* insert: *"to answer the request from the script, prepare `ctx.response` and `return ctx, \"respond\"` — the node's `respond` port must be wired (to `client`, usually);"*. Keep the string on one line as the surrounding text is; mind the escaped quotes.

- [ ] **Step 6: Build the site, run the matrix, commit**

`cd website && npm run build` (Docusaurus v3; admonitions `:::type[Title]`), then `cargo fmt --all`, both clippy invocations, full `cargo test`.

```bash
git add src/graph/engine.rs website/docs/reference/plugins/script.md website/docs/guides/lua-scripting.md src/mcp/server.rs
git commit -m "docs(script): the respond port, in the reference, the guide and the agent instructions

Two engine tests pin the contract from the graph's side: a script node
with no respond edge fails to compile naming the port, and a script that
returns ctx, \"respond\" reaches the client with its own 403 while a
browser UA still gets the upstream's body."
```

---

### Task 3: The runnable example and E2E-SCRIPT-01

**Precondition:** PR #64 merged into `develop`; this branch rebased on it (`git fetch origin && git rebase origin/develop`). `examples/lua-scripts/` must exist with the `is-bot`/`reject` nodes from #64.

**Files:**
- Modify: `examples/lua-scripts/plugins/block-user-agents.lua`, `examples/lua-scripts/config/gateway.yaml`, `examples/lua-scripts/plugins/README.md`, `examples/lua-scripts/compose.yaml` (header comment), `examples/README.md` (Scripts section)
- Create: `e2e/tests/script.spec.ts`
- Modify: `e2e/E2E_TESTBOOK.md`, `website/docs/reference/roadmap.md`, `docs/superpowers/specs/2026-09-20-script-respond-port-design.md` (§5 e2e row)

- [ ] **Step 1: The script answers itself again**

`examples/lua-scripts/plugins/block-user-agents.lua`, full content:

```lua
-- block-user-agents.lua
-- Rejects requests from known scraper/bot User-Agent patterns with a 403.
--
-- The script prepares the response and returns it with "respond", so the
-- node leaves on its `respond` port (wired to client) and the upstream never
-- runs. Setting ctx.response alone would not stop the request: the upstream
-- replaces the response. The port is named, never inferred.

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
            ctx.response.status_code = 403
            ctx.response.body = '{"error": "forbidden", "message": "Blocked user agent"}'
            ctx.response.headers["content-type"] = { "application/json" }
            ctx.message.blocked_ua = ua
            return ctx, "respond"
        end
    end

    return ctx
end
```

- [ ] **Step 2: The policy loses two nodes and gains one edge**

`examples/lua-scripts/config/gateway.yaml`: delete the `is-bot` (`condition`) and `reject` (`response-rewrite`) nodes and their four edges (`block-bots.success → is-bot.in`, `is-bot.true → reject.in`, `reject.success → client.in`, `is-bot.false → timer-start.in`); add:

```yaml
      - from: block-bots.respond
        to: client.in
      - from: block-bots.success
        to: timer-start.in
```

Replace the comment above `block-bots` with:

```yaml
      # Lua: reject known bot user agents. The script prepares the 403 and
      # returns `ctx, "respond"`, so the node leaves on `respond` (wired to
      # client below) and the upstream never runs.
```

`examples/lua-scripts/plugins/README.md`: the `block-user-agents.lua` row becomes *"Rejects known bot/scraper User-Agents with a `403` prepared in the script, returned with `"respond"` so the node leaves on its `respond` port"*; replace the paragraph *"A script cannot reject a request by writing `ctx.response` before the upstream…"* with *"To answer a request from a script, prepare `ctx.response` and `return ctx, "respond"`; the node's `respond` port must be wired (to `client`, usually). Setting `ctx.response` alone does not stop the request — the upstream replaces it."* Add `respond` to the *Referencing a script* snippet as a comment line: `# wire <id>.respond → client.in as well as <id>.success`. The header comment in `compose.yaml` and the Scripts section of `examples/README.md` keep their `scrapy/2.0 → 403` line; add *"(the script answers on its `respond` port)"* to it in both.

- [ ] **Step 3: Run the example against the release binary**

`cargo build --release`, then with `whoami` in Docker (`docker run -d --rm --name ex-whoami -p 8088:80 traefik/whoami:v1.10`), copy `examples/lua-scripts/plugins` to `C:\etc\gateway\plugins` (the policy's `source:` path resolves there on Windows), and run `UPSTREAM_HOST=127.0.0.1 UPSTREAM_PORT=8088 ./target/release/featherbit.exe --system-config examples/lua-scripts/config/system.yaml --gateway-config examples/lua-scripts/config/gateway.yaml`. Expect: `curl -A scrapy/2.0 -i localhost:8080/api/users` → `HTTP/1.1 403` with the JSON body; plain `curl -si …` → `200` with `X-Request-Id` and `x-response-time`. Also `docker compose -f examples/lua-scripts/compose.yaml config -q`. Stop the binary, remove the container and `C:\etc\gateway`.

- [ ] **Step 4: E2E-SCRIPT-01**

`e2e/tests/script.spec.ts`:

```ts
/**
 * Script node scenarios. See E2E_TESTBOOK.md ("Scripts").
 *
 * The runtime and engine tests already pin the return protocol; what only
 * this level shows is a real route where the script's own 403 reaches the
 * client over the wire because the node left on `respond`, while a browser
 * UA still gets the upstream's body.
 */
import {expect, request, test} from '@playwright/test';

import {adminApi} from '../helpers/admin';
import {GATEWAY_URL} from '../playwright.config';

const BLOCKER = [
  'function execute(ctx)',
  '  local ua = (ctx.request.headers["user-agent"] or {})[1] or ""',
  '  if string.find(string.lower(ua), "scrapy") then',
  '    ctx.response.status_code = 403',
  '    ctx.response.body = \'{"error": "forbidden"}\'',
  '    ctx.response.headers["content-type"] = { "application/json" }',
  '    return ctx, "respond"',
  '  end',
  '  return ctx',
  'end',
].join('\n');

const policy = {
  name: 'e2e-script-respond',
  nodes: [
    {id: 'listener', type: 'listener'},
    {id: 'block', type: 'script', config: {runtime: 'lua', inline: BLOCKER}},
    {id: 'backend', type: 'mocking', config: {response_status: 200, response_example: 'proxied'}},
    {id: 'client', type: 'client'},
  ],
  edges: [
    {from: 'listener.out', to: 'block.in'},
    {from: 'block.respond', to: 'client.in'},
    {from: 'block.success', to: 'backend.in'},
    {from: 'backend.success', to: 'client.in'},
  ],
};
const route = {name: 'e2e-script-respond', match: {path: '/e2e-script'}, policy: policy.name};

test.describe('Scripts', () => {
  test.beforeAll(async () => {
    const api = await adminApi();
    const p = await api.put(`/api/policies/${policy.name}`, {data: policy});
    expect(p.ok(), await p.text()).toBeTruthy();
    await api.delete(`/api/routes/${route.name}`);
    const r = await api.post('/api/routes', {data: route});
    expect(r.ok(), await r.text()).toBeTruthy();
    await api.dispose();
  });

  test.afterAll(async () => {
    const api = await adminApi();
    await api.delete(`/api/routes/${route.name}`);
    await api.delete(`/api/policies/${policy.name}`);
    await api.dispose();
  });

  test('E2E-SCRIPT-01: a script answering with "respond" short-circuits the upstream', async () => {
    const dp = await request.newContext({baseURL: GATEWAY_URL});

    const bot = await dp.get('/e2e-script', {headers: {'user-agent': 'scrapy/2.0'}});
    expect(bot.status()).toBe(403);
    expect(await bot.json()).toEqual({error: 'forbidden'});

    const browser = await dp.get('/e2e-script', {headers: {'user-agent': 'Mozilla/5.0'}});
    expect(browser.status()).toBe(200);
    expect(await browser.text()).toBe('proxied');

    await dp.dispose();
  });
});
```

Check `e2e/tests/response-cache.spec.ts`'s `afterAll` for how it deletes policies/routes and match it. In `e2e/E2E_TESTBOOK.md` add, before `## Notifications`:

```markdown
## Scripts — `tests/script.spec.ts`

| ID | Scenario | Expected |
|---|---|---|
| E2E-SCRIPT-01 | A route whose `script` node prepares a 403 for `User-Agent: scrapy/*` and returns `ctx, "respond"`, with `respond` wired to `client` and `success` to a `mocking` upstream | `scrapy/2.0` gets the script's own `403` JSON body — the upstream never ran; `Mozilla/5.0` gets `200 proxied` from the upstream |
```

- [ ] **Step 5: Roadmap and spec note**

`website/docs/reference/roadmap.md`: in the Lua / scripting row add *"A script answers a request itself with `return ctx, "respond"` (the node's `respond` outcome port). **Breaking in 0.11.0:** every `script` node must wire `respond`."* In the spec, §5's e2e row: change *"the script source is written to a temp dir the test owns and referenced by absolute path"* to *"the script is passed `inline`, so the test owns nothing on disk"*.

- [ ] **Step 6: Run everything**

```
cd website && npm run build && cd ..
cargo build --release          # no UI change; ui/dist is whatever the branch already has
cd e2e && npx playwright test script && cd ..     # 1 passed
cd e2e && FEATHERBIT_TEST_REDIS_URL=redis://127.0.0.1:6379 npx playwright test && cd ..   # full suite: 155 passed, 1 skipped (needs a redis on 6379)
```

Mutation check on the e2e: temporarily change `block.respond` to `block.success` in the spec's policy — the policy fails to compile (`respond` unwired), so instead change the script's `return ctx, "respond"` to `return ctx` and re-run `npx playwright test script`: it must FAIL (200 `proxied` for the bot). Restore.

- [ ] **Step 7: Commit**

```bash
git add examples/lua-scripts e2e/tests/script.spec.ts e2e/E2E_TESTBOOK.md website/docs/reference/roadmap.md docs/superpowers/specs/2026-09-20-script-respond-port-design.md
git commit -m "docs(examples): block-user-agents answers from the script on its respond port

The condition/response-rewrite detour #64 added to make the example true
is gone: the script prepares the 403 and returns ctx, \"respond\", one
edge carries it to the client. E2E-SCRIPT-01 drives the same shape over
the wire: scrapy gets the script's body, a browser gets the upstream's."
```

---

## Self-Review

**Spec coverage.** §2.1 port → Task 1 Step 3. §2.2 return protocol (`respond`/`success`/`nil`/other) → Task 1 Steps 1, 4. §2.3 runtime signature + `LUA_BAD_PORT` original context → Task 1 Step 4 (the `Err` paths use `ctx`, the original). §2.4 streaming: no change, no task — correct. §3 breaking change → Task 2 test `test_a_script_node_must_wire_respond`, roadmap note in Task 3. §4 examples/docs/MCP → Tasks 2 and 3 (guide's embedded script must equal the Task 3 file — called out). §5 tests: runtime ×6, plugin ×2, ports (existing), engine ×2, e2e — Tasks 1–3; mutation checks in Tasks 1, 2, 3. §6 out of scope — nothing added.

**Placeholders.** The "if mlua names `type_name` differently" and "check the import path" notes are contingencies with a stated fallback, not TODOs.

**Type consistency.** `LuaRuntime::execute -> Result<(Context, Option<&'static str>), PluginExecutionError>` used identically in Tasks 1 (runtime, plugin) and the plugin tests; `RESPOND_PORT: &str = "respond"` matches the `PortDecl` name and every edge `block.respond` / `block-bots.respond`; `"LUA_BAD_PORT"` spelled the same in code, tests and docs.
