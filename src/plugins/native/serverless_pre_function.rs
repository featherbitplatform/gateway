//! The `serverless-pre-function` node — runs one or more inline Lua functions
//! against the `Context`, threading it through each in sequence.
//!
//! Port of APISIX's `serverless-pre-function` plugin. In APISIX the two
//! serverless plugins (`serverless-pre-function` / `serverless-post-function`)
//! are identical except for the phase they run in; here they share all logic
//! ([`ServerlessRunner`]) and differ only in their registered node-type name.
//!
//! ## Deviations from APISIX
//!
//! - **Function contract.** APISIX functions are `return function(conf, ctx)
//!   ... end` chunks invoked with `(conf, ctx)`. featherbit reuses the
//!   `script` plugin's Lua runtime, so each function is a script that defines a
//!   global `function execute(ctx) ... return ctx end`, receiving and returning
//!   the marshalled Context table (see the `script` plugin docs for the table
//!   shape). This is the same contract as the `script` node.
//! - **Phase by graph position.** APISIX's `phase` field selects a request
//!   lifecycle phase. featherbit expresses phase through *placement in the
//!   policy graph*: a `serverless-pre-function` node is wired before the
//!   `upstream` node, a `serverless-post-function` node after it. The `phase`
//!   config key is accepted for compatibility but is inert.

use async_trait::async_trait;
use std::collections::HashMap;
use std::path::PathBuf;

use crate::context::Context;
use crate::plugins::script::lua_runtime::LuaRuntime;
use crate::plugins::{Plugin, PluginExecutionError, PluginOutput, PluginResult};

/// Shared engine behind both serverless nodes: a compiled list of Lua
/// functions run in order against the Context.
///
/// Each entry is a fully validated [`LuaRuntime`] (compiled at config load, so
/// bad Lua fails policy compilation, not a live request). At request time the
/// Context is threaded through every function: the table returned by one
/// function's `execute` becomes the input to the next. If any function errors,
/// that error is propagated (routing the Context through the node's `error`
/// port); otherwise the final Context flows through the `success` port.
pub struct ServerlessRunner {
    functions: Vec<LuaRuntime>,
    plugin_type: &'static str,
}

impl ServerlessRunner {
    /// Builds the runner from node config, compiling every function up front.
    ///
    /// Accepted keys:
    /// - `functions` (array of strings, **required**, ≥1): each string is Lua
    ///   source defining a global `execute(ctx)` function. Each is compiled and
    ///   validated here; an empty array, a non-string entry, a Lua syntax
    ///   error, or a missing `execute` all fail at config load.
    /// - `phase` (string, optional): accepted for APISIX compatibility but
    ///   **inert** — placement in the graph determines phase.
    /// - `timeout_ms` (integer, default `5000`): per-function execution timeout
    ///   passed to the Lua runtime (stored, not yet enforced by the VM).
    pub fn from_config(
        config: &HashMap<String, serde_json::Value>,
        plugin_type: &'static str,
    ) -> Result<Self, String> {
        let raw = config
            .get("functions")
            .ok_or_else(|| format!("{}: 'functions' is required", plugin_type))?;

        let arr = raw.as_array().ok_or_else(|| {
            format!(
                "{}: 'functions' must be an array of Lua strings",
                plugin_type
            )
        })?;

        if arr.is_empty() {
            return Err(format!(
                "{}: 'functions' must contain at least one function",
                plugin_type
            ));
        }

        let timeout_ms = config
            .get("timeout_ms")
            .and_then(|v| v.as_u64())
            .unwrap_or(5000);

        let modules_path: Option<PathBuf> = config
            .get("modules_path")
            .and_then(|v| v.as_str())
            .map(PathBuf::from);

        let mut functions = Vec::with_capacity(arr.len());
        for (i, item) in arr.iter().enumerate() {
            let src = item.as_str().ok_or_else(|| {
                format!(
                    "{}: functions[{}] must be a Lua source string",
                    plugin_type, i
                )
            })?;
            let rt = LuaRuntime::new(src, timeout_ms, modules_path.clone()).map_err(|e| {
                format!("{}: functions[{}] failed to compile: {}", plugin_type, i, e)
            })?;
            functions.push(rt);
        }

        Ok(Self {
            functions,
            plugin_type,
        })
    }

    /// Runs each function in order, threading the Context. Propagates the first
    /// error encountered.
    ///
    /// Neither `serverless-pre-function` nor `serverless-post-function`
    /// declares a `respond` port (or any outcome port) — they use the
    /// default success/error pair. Each function therefore runs through
    /// [`LuaRuntime::execute_without_port`], which rejects any second return
    /// value other than `"success"` (e.g. `return ctx, "respond"`, or any
    /// other name) as `LUA_BAD_PORT` rather than silently dropping it:
    /// silently dropping it would let someone copy the `script` node's
    /// respond idiom into a serverless function and get neither a response
    /// nor an error. The context reported on the error is the one that went
    /// *into* that function's `execute` call (not the mutated table it
    /// returned), matching the `script` node's rule that a function which
    /// did not finish making a decision keeps nothing it wrote.
    pub fn run(&self, mut ctx: Context) -> Result<Context, PluginExecutionError> {
        for func in &self.functions {
            ctx = func.execute_without_port(ctx)?;
        }
        Ok(ctx)
    }
}

/// The `serverless-pre-function` node. Runs its Lua functions before the
/// upstream call (by convention of its graph placement).
pub struct ServerlessPreFunctionPlugin {
    runner: ServerlessRunner,
}

impl ServerlessPreFunctionPlugin {
    /// Builds the plugin from node config.
    ///
    /// ```yaml
    /// type: serverless-pre-function
    /// config:
    ///   phase: access          # accepted for compatibility; inert
    ///   timeout_ms: 2000
    ///   functions:
    ///     - |
    ///       function execute(ctx)
    ///         ctx.request.headers["x-serverless"] = {"pre"}
    ///         return ctx
    ///       end
    /// ```
    pub fn from_config(config: &HashMap<String, serde_json::Value>) -> Result<Self, String> {
        Ok(Self {
            runner: ServerlessRunner::from_config(config, "serverless-pre-function")?,
        })
    }
}

#[async_trait]
impl Plugin for ServerlessPreFunctionPlugin {
    fn plugin_type(&self) -> &str {
        self.runner.plugin_type
    }

    async fn execute(&self, ctx: Context) -> PluginResult {
        let ctx = self.runner.run(ctx)?;
        Ok(PluginOutput::success(ctx))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::{GatewayRequest, GatewayResponse, Protocol};

    fn test_context() -> Context {
        Context {
            request: GatewayRequest {
                method: "GET".to_string(),
                path: "/test".to_string(),
                host: "localhost".to_string(),
                scheme: "http".to_string(),
                headers: HashMap::new(),
                query_params: HashMap::new(),
                body: bytes::Bytes::new(),
                remote_addr: "127.0.0.1:1234".to_string(),
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

    fn cfg(functions: serde_json::Value) -> HashMap<String, serde_json::Value> {
        let mut c = HashMap::new();
        c.insert("functions".to_string(), functions);
        c
    }

    #[tokio::test]
    async fn test_serverless_pre_function_mutates_and_threads() {
        // Two functions: first sets a header, second reads it into message.
        let p = ServerlessPreFunctionPlugin::from_config(&cfg(serde_json::json!([
            "function execute(ctx)\n  ctx.request.headers[\"x-step\"] = {\"one\"}\n  return ctx\nend",
            "function execute(ctx)\n  ctx.message.seen = ctx.request.headers[\"x-step\"][1]\n  return ctx\nend"
        ])))
        .unwrap();

        let out = p.execute(test_context()).await.unwrap();
        assert_eq!(
            out.context.request.headers.get("x-step"),
            Some(&vec!["one".to_string()])
        );
        // ordering: second function observed the first's mutation
        assert_eq!(
            out.context.message.get("seen"),
            Some(&serde_json::json!("one"))
        );
    }

    #[tokio::test]
    async fn test_serverless_pre_function_order() {
        // Each function appends to a message array; order must be preserved.
        let p = ServerlessPreFunctionPlugin::from_config(&cfg(serde_json::json!([
            "function execute(ctx)\n  ctx.message.trail = {\"a\"}\n  return ctx\nend",
            "function execute(ctx)\n  local t = ctx.message.trail\n  t[#t+1] = \"b\"\n  ctx.message.trail = t\n  return ctx\nend"
        ])))
        .unwrap();

        let out = p.execute(test_context()).await.unwrap();
        assert_eq!(
            out.context.message.get("trail"),
            Some(&serde_json::json!(["a", "b"]))
        );
    }

    #[test]
    fn test_serverless_pre_function_compile_error_fails_config() {
        // Syntax error in Lua fails from_config.
        let err = ServerlessPreFunctionPlugin::from_config(&cfg(serde_json::json!([
            "function execute(ctx) this is not lua"
        ])));
        assert!(err.is_err());

        // Missing execute function fails too.
        let err =
            ServerlessPreFunctionPlugin::from_config(&cfg(serde_json::json!(["local x = 1"])));
        assert!(err.is_err());
    }

    #[test]
    fn test_serverless_pre_function_empty_functions_rejected() {
        assert!(
            ServerlessPreFunctionPlugin::from_config(&cfg(serde_json::json!([])).clone()).is_err()
        );
        assert!(ServerlessPreFunctionPlugin::from_config(&HashMap::new()).is_err());
        // non-string entry
        assert!(ServerlessPreFunctionPlugin::from_config(&cfg(serde_json::json!([123]))).is_err());
    }

    #[tokio::test]
    async fn test_serverless_pre_function_error_propagates() {
        let p = ServerlessPreFunctionPlugin::from_config(&cfg(serde_json::json!([
            "function execute(ctx)\n  error(\"boom\")\n  return ctx\nend"
        ])))
        .unwrap();
        let err = p.execute(test_context()).await.unwrap_err();
        assert_eq!(err.error.code, "LUA_EXECUTION_ERROR");
    }

    /// Neither serverless node declares a `respond` port: a function that
    /// names one (the `script` node's idiom) must fail loudly, not be
    /// silently dropped -- silently dropping it would give a script author
    /// who copied that idiom into a serverless function no response and no
    /// error. The mutated header must not survive onto the error context,
    /// same rule as the `script` node's own bad-port case.
    #[tokio::test]
    async fn test_serverless_pre_function_named_port_is_lua_bad_port() {
        let p = ServerlessPreFunctionPlugin::from_config(&cfg(serde_json::json!([
            "function execute(ctx)\n  ctx.request.headers[\"x-mutated\"] = {\"yes\"}\n  return ctx, \"respond\"\nend"
        ])))
        .unwrap();
        let err = p.execute(test_context()).await.unwrap_err();
        assert_eq!(err.error.code, "LUA_BAD_PORT");
        assert!(
            !err.context.request.headers.contains_key("x-mutated"),
            "the mutated table must be discarded on a bad port"
        );
    }

    /// Naming success explicitly is accepted -- it's the same as a bare
    /// `return ctx` -- and the request continues normally.
    #[tokio::test]
    async fn test_serverless_pre_function_explicit_success_port_continues() {
        let p = ServerlessPreFunctionPlugin::from_config(&cfg(serde_json::json!([
            "function execute(ctx)\n  ctx.message.seen = true\n  return ctx, \"success\"\nend"
        ])))
        .unwrap();
        let out = p.execute(test_context()).await.unwrap();
        assert_eq!(
            out.context.message.get("seen"),
            Some(&serde_json::json!(true))
        );
    }

    /// A port name that is neither `"respond"` nor `"success"` is rejected
    /// one layer down, inside the shared Lua runtime's
    /// `execute_without_port` -- the message must say this node type has no
    /// outcome port at all, not misdirect the author toward `respond`,
    /// which these nodes do not have.
    #[tokio::test]
    async fn test_serverless_pre_function_bogus_port_is_lua_bad_port() {
        let p = ServerlessPreFunctionPlugin::from_config(&cfg(serde_json::json!([
            "function execute(ctx)\n  return ctx, \"bogus\"\nend"
        ])))
        .unwrap();
        let err = p.execute(test_context()).await.unwrap_err();
        assert_eq!(err.error.code, "LUA_BAD_PORT");
        assert!(
            err.error.message.contains("declares no outcome port"),
            "{}",
            err.error.message
        );
    }
}
