//! Lua scripting runtime for the `script` plugin, built on mlua's Luau VM.
//!
//! Marshals the gateway `Context` into a Lua table, calls the script's
//! global `execute(ctx)` function, and marshals the returned table back into
//! a `Context`. Also installs a sandboxed `require` restricted to a
//! configured modules directory.

use mlua::prelude::*;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

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
    /// Wall-clock budget for one execution, covering both loading the
    /// source and the `execute(ctx)` call. Enforced by a Luau VM interrupt;
    /// `0` disables enforcement.
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
        // Validate the script compiles. This executes the source's top
        // level, so it needs the same deadline the request path gets: a
        // `while true do end` at the top level would otherwise hang the
        // Admin API thread serving `PUT /api/policies`, with no request
        // involved.
        let lua = Lua::new();
        let timed_out = install_deadline(&lua, timeout_ms);
        setup_module_loader(&lua, &modules_path);
        lua.load(source).exec().map_err(|e| {
            if timed_out.load(Ordering::Relaxed) {
                format!(
                    "Lua script exceeded its {}ms timeout while loading",
                    timeout_ms
                )
            } else {
                format!("Lua compilation error: {}", e)
            }
        })?;

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
    /// `LUA_MISSING_EXECUTE`, `LUA_EXECUTION_ERROR`, `LUA_UNMARSHAL_ERROR`,
    /// and `LUA_TIMEOUT` when the script outran `timeout_ms`), so the graph
    /// engine routes through the error port exactly like a native plugin
    /// failure.
    ///
    /// `timeout_ms` is a single budget covering both loading the source and
    /// the `execute(ctx)` call.
    pub fn execute(&self, ctx: Context) -> Result<Context, PluginExecutionError> {
        let lua = Lua::new();
        let timed_out = install_deadline(&lua, self.timeout_ms);
        setup_module_loader(&lua, &self.modules_path);

        if let Err(e) = lua.load(&self.source).exec() {
            return Err(PluginExecutionError {
                context: ctx,
                error: GatewayError {
                    node_id: String::new(),
                    code: failure_code(&timed_out, "LUA_LOAD_ERROR"),
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
                        code: failure_code(&timed_out, "LUA_EXECUTION_ERROR"),
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
/// Lua text for a scalar. Numbers and booleans are accepted because a script
/// that writes `ctx.response.status_code = 200` or a numeric header value
/// means the obvious thing.
fn scalar_text(value: &LuaValue) -> Option<String> {
    match value {
        LuaValue::String(s) => Some(String::from_utf8_lossy(&s.as_bytes()).into_owned()),
        LuaValue::Integer(i) => Some(i.to_string()),
        LuaValue::Number(n) => Some(n.to_string()),
        LuaValue::Boolean(b) => Some(b.to_string()),
        _ => None,
    }
}

/// A required string field, named in the error so the script author knows
/// which one to fix.
fn field_string(table: &LuaTable, field: &str, at: &str) -> Result<String, String> {
    let value: LuaValue = table
        .get(field)
        .map_err(|e| format!("{at}.{field} could not be read: {e}"))?;
    scalar_text(&value).ok_or_else(|| {
        format!(
            "{at}.{field} must be a string, got {} — keep the field the script received",
            value.type_name()
        )
    })
}

/// A header or query-parameter map. The canonical shape is a list of strings
/// per key (that is what the script is handed), but `headers["x-user"] =
/// "alice"` is the natural thing to write, so a bare scalar is accepted as a
/// one-element list. Anything else names the offending key.
fn field_string_lists(
    table: &LuaTable,
    field: &str,
    at: &str,
) -> Result<HashMap<String, Vec<String>>, String> {
    let value: LuaValue = table
        .get(field)
        .map_err(|e| format!("{at}.{field} could not be read: {e}"))?;
    let map = match value {
        LuaValue::Nil => return Ok(HashMap::new()),
        LuaValue::Table(t) => t,
        other => {
            return Err(format!(
                "{at}.{field} must be a table of name -> value, got {}",
                other.type_name()
            ))
        }
    };

    let mut out = HashMap::new();
    for pair in map.pairs::<LuaValue, LuaValue>() {
        let (k, v) = pair.map_err(|e| format!("{at}.{field}: {e}"))?;
        let key = scalar_text(&k)
            .ok_or_else(|| format!("{at}.{field} has a non-string key ({})", k.type_name()))?;
        let values = match v {
            LuaValue::Table(list) => {
                let mut vals = Vec::new();
                for (i, item) in list.sequence_values::<LuaValue>().enumerate() {
                    let item = item.map_err(|e| format!("{at}.{field}['{key}'][{}]: {e}", i + 1))?;
                    vals.push(scalar_text(&item).ok_or_else(|| {
                        format!(
                            "{at}.{field}['{key}'][{}] must be a string, got {}",
                            i + 1,
                            item.type_name()
                        )
                    })?);
                }
                vals
            }
            // `headers["x-user"] = "alice"` — accept it as {"alice"}.
            scalar => vec![scalar_text(&scalar).ok_or_else(|| {
                format!(
                    "{at}.{field}['{key}'] must be a string or a table of strings (e.g. {{\"a\", \"b\"}}), got {}",
                    scalar.type_name()
                )
            })?],
        };
        out.insert(key, values);
    }
    Ok(out)
}

/// A body field: a string, a scalar, or nil for "no body".
fn field_body(table: &LuaTable, at: &str) -> Result<bytes::Bytes, String> {
    let value: LuaValue = table
        .get("body")
        .map_err(|e| format!("{at}.body could not be read: {e}"))?;
    match value {
        LuaValue::Nil => Ok(bytes::Bytes::new()),
        LuaValue::String(s) => Ok(bytes::Bytes::from(s.as_bytes().to_vec())),
        other => scalar_text(&other).map(bytes::Bytes::from).ok_or_else(|| {
            format!(
                "{at}.body must be a string, got {} — encode tables yourself (e.g. with a JSON string)",
                other.type_name()
            )
        }),
    }
}

/// The context table a script returned, field by field. Every failure names
/// the field: an opaque "error converting Lua string to table" left script
/// authors (and agents) guessing at the shape.
fn lua_to_context(
    table: &LuaTable,
    protocol: Protocol,
    errors: Vec<GatewayError>,
) -> Result<Context, String> {
    let sub_table = |field: &str| -> Result<LuaTable, String> {
        let value: LuaValue = table
            .get(field)
            .map_err(|e| format!("ctx.{field} could not be read: {e}"))?;
        match value {
            LuaValue::Table(t) => Ok(t),
            other => Err(format!(
                "ctx.{field} must be a table, got {} — return the context you were given (`return ctx`), with your changes applied",
                other.type_name()
            )),
        }
    };
    let req_table = sub_table("request")?;
    let resp_table = sub_table("response")?;

    let request = GatewayRequest {
        method: field_string(&req_table, "method", "ctx.request")?,
        path: field_string(&req_table, "path", "ctx.request")?,
        host: field_string(&req_table, "host", "ctx.request")?,
        scheme: field_string(&req_table, "scheme", "ctx.request")?,
        headers: field_string_lists(&req_table, "headers", "ctx.request")?,
        query_params: field_string_lists(&req_table, "query_params", "ctx.request")?,
        body: field_body(&req_table, "ctx.request")?,
        remote_addr: field_string(&req_table, "remote_addr", "ctx.request")?,
        protocol,
    };

    let status_value: LuaValue = resp_table
        .get("status_code")
        .map_err(|e| format!("ctx.response.status_code could not be read: {e}"))?;
    let status_code = match &status_value {
        LuaValue::Integer(i) => u16::try_from(*i).ok(),
        LuaValue::Number(n) => (n.fract() == 0.0)
            .then_some(*n as i64)
            .and_then(|i| u16::try_from(i).ok()),
        LuaValue::String(s) => String::from_utf8_lossy(&s.as_bytes()).parse().ok(),
        _ => None,
    }
    .ok_or_else(|| {
        format!(
            "ctx.response.status_code must be an HTTP status number (e.g. 200), got {}",
            status_value.type_name()
        )
    })?;

    let response = GatewayResponse {
        status_code,
        headers: field_string_lists(&resp_table, "headers", "ctx.response")?,
        body: field_body(&resp_table, "ctx.response")?,
        // Rebuilding from the Lua table always discards any stream: safe only
        // because `script` does not opt out of `Plugin::reads_response_body()`
        // (defaults to `true`), so the policy compiler never marks an
        // upstream stream-capable when a script node sits downstream — a
        // stream can never reach here.
        stream: None,
    };

    let mut message = HashMap::new();
    if let Ok(LuaValue::Table(msg_table)) = table.get::<LuaValue>("message") {
        for pair in msg_table.pairs::<String, LuaValue>() {
            let (k, v) = pair.map_err(|e| format!("ctx.message: {e}"))?;
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

/// Installs a wall-clock deadline on `lua`, returning the flag its interrupt
/// sets when the budget runs out.
///
/// Luau calls the interrupt at VM instruction boundaries, so a tight loop is
/// caught; time spent inside a Rust callback or `require`'s file IO is not.
/// Returning an error from the interrupt propagates it through whatever the
/// VM was executing, which is what stops the script.
///
/// The flag -- not the error message -- is what distinguishes a deadline
/// abort from an ordinary script fault, so the two never blur together if
/// the message is ever reworded.
///
/// `timeout_ms: 0` installs nothing: an explicit opt-out for a trusted
/// long-running script.
fn install_deadline(lua: &Lua, timeout_ms: u64) -> Arc<AtomicBool> {
    let timed_out = Arc::new(AtomicBool::new(false));
    if timeout_ms == 0 {
        return timed_out;
    }

    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    let flag = Arc::clone(&timed_out);
    lua.set_interrupt(move |_| {
        if Instant::now() >= deadline {
            flag.store(true, Ordering::Relaxed);
            Err(LuaError::runtime(format!(
                "script exceeded its {}ms timeout",
                timeout_ms
            )))
        } else {
            Ok(LuaVmState::Continue)
        }
    });
    timed_out
}

/// The error code for a failed Lua call: `LUA_TIMEOUT` when the deadline
/// tripped, otherwise the caller's code for that failure site.
fn failure_code(timed_out: &Arc<AtomicBool>, default_code: &str) -> String {
    if timed_out.load(Ordering::Relaxed) {
        "LUA_TIMEOUT".to_string()
    } else {
        default_code.to_string()
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
                stream: None,
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

    /// Runs `body` as the whole of `execute`, returning the result.
    fn run(body: &str) -> Result<Context, PluginExecutionError> {
        let rt =
            LuaRuntime::new(&format!("function execute(ctx)\n{body}\nend"), 5000, None).unwrap();
        rt.execute(test_context())
    }

    /// The shape a script author naturally writes — a bare string header
    /// value — used to fail with an opaque conversion error.
    #[test]
    fn test_lua_scalar_header_and_query_values_are_accepted() {
        let ctx = run(r#"
            ctx.request.headers["x-user"] = "alice"
            ctx.request.headers["x-retry"] = 3
            ctx.request.query_params["page"] = "2"
            ctx.response.headers["x-served"] = "yes"
            return ctx
        "#)
        .unwrap();
        assert_eq!(ctx.request.headers["x-user"], vec!["alice"]);
        assert_eq!(ctx.request.headers["x-retry"], vec!["3"]);
        assert_eq!(ctx.request.query_params["page"], vec!["2"]);
        assert_eq!(ctx.response.headers["x-served"], vec!["yes"]);
    }

    #[test]
    fn test_lua_list_header_values_still_work() {
        let ctx = run(r#"
            ctx.request.headers["accept"] = {"text/plain", "application/json"}
            return ctx
        "#)
        .unwrap();
        assert_eq!(
            ctx.request.headers["accept"],
            vec!["text/plain", "application/json"]
        );
    }

    #[test]
    fn test_lua_nil_body_and_numeric_status_are_accepted() {
        let ctx = run(r#"
            ctx.request.body = nil
            ctx.response.status_code = 201
            ctx.response.body = "ok"
            return ctx
        "#)
        .unwrap();
        assert!(ctx.request.body.is_empty());
        assert_eq!(ctx.response.status_code, 201);
        assert_eq!(ctx.response.body.as_ref(), b"ok");
    }

    #[test]
    fn test_lua_unmarshal_errors_name_the_offending_field() {
        // A table where a string belongs.
        let err = run(r#"
            ctx.request.path = {"/oops"}
            return ctx
        "#)
        .unwrap_err();
        assert_eq!(err.error.code, "LUA_UNMARSHAL_ERROR");
        assert!(
            err.error
                .message
                .contains("ctx.request.path must be a string"),
            "{}",
            err.error.message
        );

        // A table where a body belongs: the message says to encode it.
        let err = run(r#"
            ctx.response.body = { ok = true }
            return ctx
        "#)
        .unwrap_err();
        assert!(
            err.error
                .message
                .contains("ctx.response.body must be a string"),
            "{}",
            err.error.message
        );

        // A nested table inside a header list.
        let err = run(r#"
            ctx.request.headers["x"] = {{"nested"}}
            return ctx
        "#)
        .unwrap_err();
        assert!(
            err.error.message.contains("ctx.request.headers['x'][1]"),
            "{}",
            err.error.message
        );

        // A fresh table instead of the context that was handed in.
        let err = run(r#"
            return { message = { a = 1 } }
        "#)
        .unwrap_err();
        assert!(
            err.error.message.contains("ctx.request must be a table"),
            "{}",
            err.error.message
        );
        assert!(
            err.error.message.contains("return ctx"),
            "{}",
            err.error.message
        );

        // A status code that is not a status code.
        let err = run(r#"
            ctx.response.status_code = "fine"
            return ctx
        "#)
        .unwrap_err();
        assert!(
            err.error
                .message
                .contains("status_code must be an HTTP status number"),
            "{}",
            err.error.message
        );
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

    /// A script that never returns must be stopped by `timeout_ms`. Before
    /// enforcement landed this call did not fail -- it hung, pinning the
    /// tokio worker thread that was polling it, so N runaway scripts wedged
    /// N of the runtime's workers.
    #[test]
    fn test_lua_runaway_script_is_stopped_by_timeout() {
        let rt = LuaRuntime::new(
            r#"
            function execute(ctx)
                while true do end
                return ctx
            end
            "#,
            50,
            None,
        )
        .unwrap();

        let started = std::time::Instant::now();
        let result = rt.execute(test_context());
        let elapsed = started.elapsed();

        let err = result.expect_err("a script that never returns must not succeed");
        assert_eq!(
            err.error.code, "LUA_TIMEOUT",
            "a timeout must be distinguishable from an ordinary script fault"
        );
        assert!(
            elapsed < std::time::Duration::from_secs(5),
            "the interrupt should fire near its 50ms budget, took {:?}",
            elapsed
        );
    }

    /// The interrupt must not trip on a script that finishes inside its
    /// budget: a false positive here would break every working script.
    #[test]
    fn test_lua_script_within_budget_is_not_timed_out() {
        let rt = LuaRuntime::new(
            r#"
            function execute(ctx)
                local total = 0
                for i = 1, 100000 do total = total + i end
                ctx.message.total = total
                return ctx
            end
            "#,
            5000,
            None,
        )
        .unwrap();

        let result = rt.execute(test_context()).unwrap();
        assert_eq!(
            result.message.get("total").and_then(|v| v.as_f64()),
            Some(5000050000.0)
        );
    }

    /// The same hang exists at policy-compile time: `new` validates a script
    /// by executing its top level, so a loop there blocks the Admin API
    /// thread serving `PUT /api/policies` -- no request needed to trigger it.
    #[test]
    fn test_lua_compile_time_validation_is_bounded() {
        let started = std::time::Instant::now();
        let result = LuaRuntime::new("while true do end", 50, None);
        let elapsed = started.elapsed();

        assert!(
            result.is_err(),
            "a top-level infinite loop must fail to compile, not hang"
        );
        assert!(
            elapsed < std::time::Duration::from_secs(5),
            "compile-time validation should be bounded, took {:?}",
            elapsed
        );
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
