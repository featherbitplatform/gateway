//! The `client` node — the fixed terminal point of every policy graph.

use async_trait::async_trait;
use std::collections::HashMap;

use crate::context::Context;
use crate::plugins::{Plugin, PluginOutput, PluginResult};

/// The Client node is the terminal node in a routing policy.
/// When the context reaches this node, the response is sent to the client.
/// It is a passthrough — it does not modify the context.
///
/// Graph execution stops here: whatever `Context.response` holds at this point
/// is what the server writes back to the caller. It takes no configuration.
pub struct ClientPlugin;

#[async_trait]
impl Plugin for ClientPlugin {
    fn plugin_type(&self) -> &str {
        "client"
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
