//! The `listener` node — the fixed entry point of every policy graph.

use async_trait::async_trait;
use std::collections::HashMap;

use crate::context::Context;
use crate::plugins::{Plugin, PluginOutput, PluginResult};

/// The Listener node is a passthrough — it exists as the graph entry/exit point.
/// The actual listener logic (accepting connections, building the initial Context)
/// lives in the server module. This node simply passes the context through.
///
/// Every policy graph starts execution at a `listener` node; it takes no
/// configuration.
pub struct ListenerPlugin;

#[async_trait]
impl Plugin for ListenerPlugin {
    fn plugin_type(&self) -> &str {
        "listener"
    }

    async fn execute(
        &self,
        ctx: Context,
        _named_inputs: &HashMap<String, serde_json::Value>,
    ) -> PluginResult {
        Ok(PluginOutput {
            context: ctx,
            named_outputs: HashMap::new(),
        })
    }
}
