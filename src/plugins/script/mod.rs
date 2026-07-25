//! Scripted plugin host (`script`).
//!
//! Runs user-provided scripts as graph nodes behind the same `Plugin` trait
//! as native plugins. Scripts are parsed and validated once at policy-compile
//! time (in `from_config`), not per request; script failures surface as
//! `PluginExecutionError` exactly like native failures, routing through the
//! node's error port.

pub mod lua_runtime;

use async_trait::async_trait;
use std::collections::HashMap;
use std::path::PathBuf;

use crate::context::Context;
use crate::plugins::{Plugin, PluginExecutionError, PluginOutput, PluginResult};

/// Executes a scripted plugin as a graph node.
///
/// The script receives the full `Context` (request, response, message) and
/// returns a possibly modified copy; anything it writes into `ctx.message`
/// is visible to downstream nodes. Currently only the Lua (Luau) runtime is
/// supported.
pub struct ScriptPlugin {
    runtime: ScriptRuntime,
}

/// Dispatch over the available scripting runtimes (only Lua today; Python is
/// planned but not implemented).
enum ScriptRuntime {
    Lua(lua_runtime::LuaRuntime),
}

impl ScriptPlugin {
    /// Builds the plugin from node config, loading and validating the script
    /// immediately so bad scripts fail at policy-compile time.
    ///
    /// Accepted keys:
    /// - `runtime` (string, default `"lua"`): scripting runtime; any other
    ///   value is an error.
    /// - `source` (string): path to a script file, read at compile time.
    /// - `inline` (string): script text embedded in the config. One of
    ///   `source` or `inline` is required (`source` wins if both are set);
    ///   omitting both is an error, as is an unreadable `source` file.
    /// - `timeout_ms` (integer, default `5000`): script execution timeout;
    ///   currently stored by the Lua runtime but not yet enforced.
    /// - `modules_path` (string, default: the `source` script's parent
    ///   directory; none for `inline`): directory the sandboxed `require`
    ///   resolves modules from.
    ///
    /// ```yaml
    /// type: script
    /// config:
    ///   runtime: lua
    ///   source: scripts/enrich.lua
    ///   timeout_ms: 2000
    /// ```
    ///
    /// The script must define a global `execute(ctx)` function that returns
    /// the (possibly modified) context table:
    ///
    /// ```lua
    /// function execute(ctx)
    ///     ctx.request.headers["x-enriched"] = {"true"}
    ///     ctx.message.user_tier = "gold"
    ///     return ctx
    /// end
    /// ```
    pub fn from_config(config: &HashMap<String, serde_json::Value>) -> Result<Self, String> {
        let runtime_name = config
            .get("runtime")
            .and_then(|v| v.as_str())
            .unwrap_or("lua");

        let source_path = config
            .get("source")
            .and_then(|v| v.as_str())
            .map(String::from);

        let inline_source = config
            .get("inline")
            .and_then(|v| v.as_str())
            .map(String::from);

        if source_path.is_none() && inline_source.is_none() {
            return Err("script plugin requires 'source' or 'inline'".to_string());
        }

        let source = if let Some(ref path) = source_path {
            std::fs::read_to_string(path)
                .map_err(|e| format!("Failed to read script '{}': {}", path, e))?
        } else {
            inline_source.unwrap()
        };

        let timeout_ms = config
            .get("timeout_ms")
            .and_then(|v| v.as_u64())
            .unwrap_or(5000);

        // modules_path: explicit config, or derive from the script's parent directory
        let modules_path = config
            .get("modules_path")
            .and_then(|v| v.as_str())
            .map(PathBuf::from)
            .or_else(|| {
                source_path
                    .as_ref()
                    .and_then(|p| PathBuf::from(p).parent().map(|p| p.to_path_buf()))
            });

        let runtime = match runtime_name {
            "lua" => {
                let rt = lua_runtime::LuaRuntime::new(&source, timeout_ms, modules_path)?;
                ScriptRuntime::Lua(rt)
            }
            other => {
                return Err(format!("Unknown runtime: '{}' — supported: lua", other));
            }
        };

        Ok(Self { runtime })
    }
}

#[async_trait]
impl Plugin for ScriptPlugin {
    fn plugin_type(&self) -> &str {
        "script"
    }

    async fn execute(
        &self,
        ctx: Context,
        _named_inputs: &HashMap<String, serde_json::Value>,
    ) -> PluginResult {
        match &self.runtime {
            ScriptRuntime::Lua(rt) => match rt.execute(ctx) {
                Ok(new_ctx) => Ok(PluginOutput {
                    context: new_ctx,
                    named_outputs: HashMap::new(),
                }),
                Err(e) => Err(PluginExecutionError {
                    context: e.context,
                    error: e.error,
                }),
            },
        }
    }
}
