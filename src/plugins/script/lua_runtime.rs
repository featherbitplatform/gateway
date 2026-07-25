//! Lua scripting runtime for the `script` plugin, built on mlua's Luau VM.
//!
//! Marshals the gateway `Context` into a Lua table, calls the script's
//! global `execute(ctx)` function, and marshals the returned table back into
//! a `Context`. Also installs a sandboxed `require` restricted to a
//! configured modules directory.

use mlua::prelude::*;
use std::collections::HashMap;
use std::path::PathBuf;

use crate::context::{Context, GatewayError, GatewayRequest, GatewayResponse, Protocol};
use crate::plugins::PluginExecutionError;

/// Holds a validated Lua script and executes it against a `Context`.
///
/// A fresh Lua VM is created for every execution, so scripts cannot leak
/// state between requests; only the source text is retained between calls.
pub struct LuaRuntime {
    /// The full script source, re-loaded into a fresh VM per execution.
    source: String,
    /// Directory the sandboxed `require` resolves modules from; `None`
    /// disables `require`.
    modules_path: Option<PathBuf>,
    /// Configured execution timeout in milliseconds (currently stored but
    /// not enforced by the VM).
    #[allow(dead_code)] // see roadmap: script execution timeouts
    timeout_ms: u64,
}

impl LuaRuntime {
    /// Compiles and validates the script in a throwaway VM, failing early if
    /// the source has syntax errors, its top level errors on load, or it does
    /// not define a global `execute` function. This runs once at
    /// policy-compile time, not per request.
    pub fn new(
        source: &str,
        timeout_ms: u64,
        modules_path: Option<PathBuf>,
    ) -> Result<Self, String> {
        // Validate the script compiles
        let lua = Lua::new();
        setup_module_loader(&lua, &modules_path);
        lua.load(source)
            .exec()
            .map_err(|e| format!("Lua compilation error: {}", e))?;

        // Verify execute function exists
        lua.globals()
            .get::<LuaFunction>("execute")
            .map_err(|_| "Lua script must define an 'execute(ctx)' function".to_string())?;

        Ok(Self {
            source: source.to_string(),
            modules_path,
            timeout_ms,
        })
    }

    /// Runs the script's `execute(ctx)` against the given context in a fresh
    /// VM and returns the context rebuilt from the table the script returned.
    ///
    /// Every failure mode (load, marshalling either way, missing `execute`,
    /// or a runtime error raised by the script) returns a
    /// `PluginExecutionError` carrying the original context, with a
    /// distinguishing error code (`LUA_LOAD_ERROR`, `LUA_MARSHAL_ERROR`,
    /// `LUA_MISSING_EXECUTE`, `LUA_EXECUTION_ERROR`, `LUA_UNMARSHAL_ERROR`),
    /// so the graph engine routes through the error port exactly like a
    /// native plugin failure.
    pub fn execute(&self, ctx: Context) -> Result<Context, PluginExecutionError> {
        let lua = Lua::new();
        setup_module_loader(&lua, &self.modules_path);

        if let Err(e) = lua.load(&self.source).exec() {
            return Err(PluginExecutionError {
                context: ctx,
                error: GatewayError {
                    node_id: String::new(),
                    code: "LUA_LOAD_ERROR".to_string(),
                    message: format!("Failed to load Lua script: {}", e),
                    metadata: HashMap::new(),
                },
            });
        }

        let ctx_table = match context_to_lua(&lua, &ctx) {
            Ok(t) => t,
            Err(e) => {
                return Err(PluginExecutionError {
                    context: ctx,
                    error: GatewayError {
                        node_id: String::new(),
                        code: "LUA_MARSHAL_ERROR".to_string(),
                        message: format!("Failed to marshal context to Lua: {}", e),
                        metadata: HashMap::new(),
                    },
                });
            }
        };

        let execute_fn: LuaFunction = match lua.globals().get("execute") {
            Ok(f) => f,
            Err(e) => {
                return Err(PluginExecutionError {
                    context: ctx,
                    error: GatewayError {
                        node_id: String::new(),
                        code: "LUA_MISSING_EXECUTE".to_string(),
                        message: format!("Missing execute function: {}", e),
                        metadata: HashMap::new(),
                    },
                });
            }
        };

        let result_table: LuaTable = match execute_fn.call(ctx_table) {
            Ok(t) => t,
            Err(e) => {
                return Err(PluginExecutionError {
                    context: ctx,
                    error: GatewayError {
                        node_id: String::new(),
                        code: "LUA_EXECUTION_ERROR".to_string(),
                        message: format!("Lua execution error: {}", e),
                        metadata: HashMap::new(),
                    },
                });
            }
        };

        // Carry over the fields scripts never see: the wire protocol and the
        // errors accumulated by earlier nodes must survive a script node.
        let protocol = ctx.request.protocol.clone();
        let errors = ctx.errors.clone();
        match lua_to_context(&result_table, protocol, errors) {
            Ok(new_ctx) => Ok(new_ctx),
            Err(e) => Err(PluginExecutionError {
                context: ctx,
                error: GatewayError {
                    node_id: String::new(),
                    code: "LUA_UNMARSHAL_ERROR".to_string(),
                    message: format!("Failed to unmarshal context from Lua: {}", e),
                    metadata: HashMap::new(),
                },
            }),
        }
    }
}

/// Registers a custom `require()` loader that reads `.lua` files from the modules directory.
///
/// The loader is sandboxed: module names containing `..`, `/`, or `\` are
/// rejected, so only files directly inside `modules_path` can be loaded
/// (resolved as `<modules_path>/<name>.lua`). When `modules_path` is `None`,
/// no `require` is installed. Note: modules are re-evaluated on every
/// `require`; results are not cached.
fn setup_module_loader(lua: &Lua, modules_path: &Option<PathBuf>) {
    let Some(base_path) = modules_path.clone() else {
        return;
    };

    // In Luau, we use the require override approach
    let loader = lua
        .create_function(move |lua, module_name: String| {
            // Sanitize: no path traversal
            if module_name.contains("..") || module_name.contains('/') || module_name.contains('\\')
            {
                return Err(LuaError::runtime(format!(
                    "Invalid module name '{}': path traversal not allowed",
                    module_name
                )));
            }

            // Try module_name.lua
            let file_path = base_path.join(format!("{}.lua", module_name));
            let source = std::fs::read_to_string(&file_path).map_err(|e| {
                LuaError::runtime(format!(
                    "Cannot load module '{}' from {:?}: {}",
                    module_name, file_path, e
                ))
            })?;

            // Execute the module and return its result
            lua.load(&source).eval::<LuaValue>().map_err(|e| {
                LuaError::runtime(format!("Error loading module '{}': {}", module_name, e))
            })
        })
        .expect("Failed to create module loader");

    lua.globals()
        .set("require", loader)
        .expect("Failed to set require");
}

/// Marshals a `Context` into a Lua table with `request`, `response`, and
/// `message` sub-tables. Header and query-param values become 1-indexed
/// arrays of strings; bodies become Lua strings; `message` values are
/// converted from JSON. `errors` is not exposed to scripts.
fn context_to_lua(lua: &Lua, ctx: &Context) -> LuaResult<LuaTable> {
    let table = lua.create_table()?;

    // request
    let req = lua.create_table()?;
    req.set("method", ctx.request.method.as_str())?;
    req.set("path", ctx.request.path.as_str())?;
    req.set("host", ctx.request.host.as_str())?;
    req.set("scheme", ctx.request.scheme.as_str())?;
    req.set("remote_addr", ctx.request.remote_addr.as_str())?;

    let headers = lua.create_table()?;
    for (k, v) in &ctx.request.headers {
        let vals = lua.create_table()?;
        for (i, val) in v.iter().enumerate() {
            vals.set(i + 1, val.as_str())?;
        }
        headers.set(k.as_str(), vals)?;
    }
    req.set("headers", headers)?;

    let query = lua.create_table()?;
    for (k, v) in &ctx.request.query_params {
        let vals = lua.create_table()?;
        for (i, val) in v.iter().enumerate() {
            vals.set(i + 1, val.as_str())?;
        }
        query.set(k.as_str(), vals)?;
    }
    req.set("query_params", query)?;

    req.set("body", lua.create_string(&ctx.request.body)?)?;
    table.set("request", req)?;

    // response
    let resp = lua.create_table()?;
    resp.set("status_code", ctx.response.status_code)?;
    let resp_headers = lua.create_table()?;
    for (k, v) in &ctx.response.headers {
        let vals = lua.create_table()?;
        for (i, val) in v.iter().enumerate() {
            vals.set(i + 1, val.as_str())?;
        }
        resp_headers.set(k.as_str(), vals)?;
    }
    resp.set("headers", resp_headers)?;
    resp.set("body", lua.create_string(&ctx.response.body)?)?;
    table.set("response", resp)?;

    // message
    let msg = lua.create_table()?;
    for (k, v) in &ctx.message {
        let lua_val = json_to_lua(lua, v)?;
        msg.set(k.as_str(), lua_val)?;
    }
    table.set("message", msg)?;

    Ok(table)
}

/// Rebuilds a `Context` from the table returned by the script's `execute`.
///
/// `request` and `response` (including their `headers` and bodies) are
/// required and fail unmarshalling if malformed; `query_params` and
/// `message` are optional. `protocol` and `errors` are not exposed to Lua,
/// so the caller passes the original context's values through unchanged.
fn lua_to_context(
    table: &LuaTable,
    protocol: Protocol,
    errors: Vec<GatewayError>,
) -> LuaResult<Context> {
    let req_table: LuaTable = table.get("request")?;
    let resp_table: LuaTable = table.get("response")?;

    let mut request_headers = HashMap::new();
    let headers_table: LuaTable = req_table.get("headers")?;
    for pair in headers_table.pairs::<String, LuaTable>() {
        let (k, v) = pair?;
        let mut vals = Vec::new();
        for val in v.sequence_values::<String>() {
            vals.push(val?);
        }
        request_headers.insert(k, vals);
    }

    let mut query_params = HashMap::new();
    if let Ok(qp_table) = req_table.get::<LuaTable>("query_params") {
        for pair in qp_table.pairs::<String, LuaTable>() {
            let (k, v) = pair?;
            let mut vals = Vec::new();
            for val in v.sequence_values::<String>() {
                vals.push(val?);
            }
            query_params.insert(k, vals);
        }
    }

    let body_str: mlua::String = req_table.get("body")?;
    let request = GatewayRequest {
        method: req_table.get("method")?,
        path: req_table.get("path")?,
        host: req_table.get("host")?,
        scheme: req_table.get("scheme")?,
        headers: request_headers,
        query_params,
        body: bytes::Bytes::from(body_str.as_bytes().to_vec()),
        remote_addr: req_table.get("remote_addr")?,
        protocol,
    };

    let mut response_headers = HashMap::new();
    let resp_headers_table: LuaTable = resp_table.get("headers")?;
    for pair in resp_headers_table.pairs::<String, LuaTable>() {
        let (k, v) = pair?;
        let mut vals = Vec::new();
        for val in v.sequence_values::<String>() {
            vals.push(val?);
        }
        response_headers.insert(k, vals);
    }

    let resp_body_str: mlua::String = resp_table.get("body")?;
    let response = GatewayResponse {
        status_code: resp_table.get("status_code")?,
        headers: response_headers,
        body: bytes::Bytes::from(resp_body_str.as_bytes().to_vec()),
    };

    let mut message = HashMap::new();
    if let Ok(msg_table) = table.get::<LuaTable>("message") {
        for pair in msg_table.pairs::<String, LuaValue>() {
            let (k, v) = pair?;
            message.insert(k, lua_to_json(&v));
        }
    }

    Ok(Context {
        request,
        response,
        message,
        errors,
    })
}

/// Converts a JSON value to the corresponding Lua value (arrays become
/// 1-indexed tables, objects become string-keyed tables).
fn json_to_lua(lua: &Lua, value: &serde_json::Value) -> LuaResult<LuaValue> {
    match value {
        serde_json::Value::Null => Ok(LuaValue::Nil),
        serde_json::Value::Bool(b) => Ok(LuaValue::Boolean(*b)),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Ok(LuaValue::Integer(i as _))
            } else {
                Ok(LuaValue::Number(n.as_f64().unwrap_or(0.0)))
            }
        }
        serde_json::Value::String(s) => Ok(LuaValue::String(lua.create_string(s)?)),
        serde_json::Value::Array(arr) => {
            let table = lua.create_table()?;
            for (i, v) in arr.iter().enumerate() {
                table.set(i + 1, json_to_lua(lua, v)?)?;
            }
            Ok(LuaValue::Table(table))
        }
        serde_json::Value::Object(obj) => {
            let table = lua.create_table()?;
            for (k, v) in obj {
                table.set(k.as_str(), json_to_lua(lua, v)?)?;
            }
            Ok(LuaValue::Table(table))
        }
    }
}

/// Converts a Lua value back to JSON. Tables with a non-zero sequence length
/// become JSON arrays, other tables become objects with string keys;
/// unconvertible values (functions, userdata, non-UTF-8 strings) degrade to
/// null or empty strings rather than erroring.
fn lua_to_json(value: &LuaValue) -> serde_json::Value {
    match value {
        LuaValue::Nil => serde_json::Value::Null,
        LuaValue::Boolean(b) => serde_json::Value::Bool(*b),
        LuaValue::Integer(i) => serde_json::json!(*i),
        LuaValue::Number(n) => serde_json::json!(*n),
        LuaValue::String(s) => {
            serde_json::Value::String(std::str::from_utf8(&s.as_bytes()).unwrap_or("").to_string())
        }
        LuaValue::Table(t) => {
            let len = t.raw_len();
            if len > 0 {
                let arr: Vec<serde_json::Value> = (1..=len)
                    .filter_map(|i| t.get::<LuaValue>(i).ok().map(|v| lua_to_json(&v)))
                    .collect();
                serde_json::Value::Array(arr)
            } else {
                let mut map = serde_json::Map::new();
                if let Ok(pairs) = t
                    .clone()
                    .pairs::<String, LuaValue>()
                    .collect::<Result<Vec<_>, _>>()
                {
                    for (k, v) in pairs {
                        map.insert(k, lua_to_json(&v));
                    }
                }
                serde_json::Value::Object(map)
            }
        }
        _ => serde_json::Value::Null,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::Protocol;

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
            },
            message: HashMap::new(),
            errors: Vec::new(),
        }
    }

    #[test]
    fn test_lua_modify_path() {
        let rt = LuaRuntime::new(
            r#"
            function execute(ctx)
                ctx.request.path = "/modified"
                return ctx
            end
            "#,
            5000,
            None,
        )
        .unwrap();

        let ctx = test_context();
        let result = rt.execute(ctx).unwrap();
        assert_eq!(result.request.path, "/modified");
    }

    #[test]
    fn test_lua_set_message() {
        let rt = LuaRuntime::new(
            r#"
            function execute(ctx)
                ctx.message.enriched = true
                ctx.message.user_id = "abc123"
                return ctx
            end
            "#,
            5000,
            None,
        )
        .unwrap();

        let ctx = test_context();
        let result = rt.execute(ctx).unwrap();
        assert_eq!(
            result.message.get("enriched"),
            Some(&serde_json::json!(true))
        );
        assert_eq!(
            result.message.get("user_id"),
            Some(&serde_json::json!("abc123"))
        );
    }

    #[test]
    fn test_lua_add_header() {
        let rt = LuaRuntime::new(
            r#"
            function execute(ctx)
                ctx.request.headers["x-custom"] = {"hello"}
                return ctx
            end
            "#,
            5000,
            None,
        )
        .unwrap();

        let ctx = test_context();
        let result = rt.execute(ctx).unwrap();
        assert_eq!(
            result.request.headers.get("x-custom"),
            Some(&vec!["hello".to_string()])
        );
    }

    #[test]
    fn test_lua_preserves_errors_and_protocol() {
        let rt = LuaRuntime::new(
            r#"
            function execute(ctx)
                return ctx
            end
            "#,
            5000,
            None,
        )
        .unwrap();

        let mut ctx = test_context();
        ctx.request.protocol = Protocol::Http2;
        ctx.errors.push(GatewayError {
            node_id: "upstream".to_string(),
            code: "UPSTREAM_CONNECTION_ERROR".to_string(),
            message: "boom".to_string(),
            metadata: HashMap::new(),
        });

        let result = rt.execute(ctx).unwrap();
        assert_eq!(result.request.protocol, Protocol::Http2);
        assert_eq!(result.errors.len(), 1);
        assert_eq!(result.errors[0].code, "UPSTREAM_CONNECTION_ERROR");
    }

    #[test]
    fn test_lua_error_handling() {
        let rt = LuaRuntime::new(
            r#"
            function execute(ctx)
                error("something went wrong")
                return ctx
            end
            "#,
            5000,
            None,
        )
        .unwrap();

        let ctx = test_context();
        let result = rt.execute(ctx);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.error.code, "LUA_EXECUTION_ERROR");
    }

    #[test]
    fn test_lua_require_module() {
        // Create a temp directory with a module
        let tmp = std::env::temp_dir().join("gw_lua_test");
        let _ = std::fs::create_dir_all(&tmp);
        std::fs::write(
            tmp.join("helpers.lua"),
            r#"
            local M = {}
            function M.greet(name)
                return "hello " .. name
            end
            return M
            "#,
        )
        .unwrap();

        let rt = LuaRuntime::new(
            r#"
            local helpers = require("helpers")

            function execute(ctx)
                ctx.message.greeting = helpers.greet("world")
                return ctx
            end
            "#,
            5000,
            Some(tmp.clone()),
        )
        .unwrap();

        let ctx = test_context();
        let result = rt.execute(ctx).unwrap();
        assert_eq!(
            result.message.get("greeting"),
            Some(&serde_json::json!("hello world"))
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }
}
