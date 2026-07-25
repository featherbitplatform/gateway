//! The `serverless-post-function` node — runs one or more inline Lua functions
//! against the `Context`, threading it through each in sequence.
//!
//! Port of APISIX's `serverless-post-function` plugin. It shares **all** logic
//! with [`serverless-pre-function`](super::serverless_pre_function) via the
//! shared [`ServerlessRunner`]; the only difference is the registered
//! node-type name (and, by convention, its placement in the policy graph:
//! after the `upstream` node rather than before it).
//!
//! See [`super::serverless_pre_function`] for the full documentation of the
//! function contract and the deviations from APISIX (the `execute(ctx)`
//! contract and phase-by-graph-position).

use async_trait::async_trait;
use std::collections::HashMap;

use crate::context::Context;
use crate::plugins::native::serverless_pre_function::ServerlessRunner;
use crate::plugins::{Plugin, PluginOutput, PluginResult};

/// The `serverless-post-function` node. Runs its Lua functions after the
/// upstream call (by convention of its graph placement).
pub struct ServerlessPostFunctionPlugin {
    runner: ServerlessRunner,
}

impl ServerlessPostFunctionPlugin {
    /// Builds the plugin from node config. Same config shape as
    /// [`serverless-pre-function`](super::serverless_pre_function::ServerlessPreFunctionPlugin::from_config).
    ///
    /// ```yaml
    /// type: serverless-post-function
    /// config:
    ///   phase: body_filter     # accepted for compatibility; inert
    ///   functions:
    ///     - |
    ///       function execute(ctx)
    ///         ctx.response.headers["x-served-by"] = {"featherbit"}
    ///         return ctx
    ///       end
    /// ```
    pub fn from_config(config: &HashMap<String, serde_json::Value>) -> Result<Self, String> {
        Ok(Self {
            runner: ServerlessRunner::from_config(config, "serverless-post-function")?,
        })
    }
}

#[async_trait]
impl Plugin for ServerlessPostFunctionPlugin {
    fn plugin_type(&self) -> &str {
        "serverless-post-function"
    }

    async fn execute(
        &self,
        ctx: Context,
        _named_inputs: &HashMap<String, serde_json::Value>,
    ) -> PluginResult {
        let ctx = self.runner.run(ctx)?;
        Ok(PluginOutput {
            context: ctx,
            named_outputs: HashMap::new(),
        })
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
                status_code: 200,
                headers: HashMap::new(),
                body: bytes::Bytes::new(),
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
    async fn test_serverless_post_function_mutates_response() {
        let p = ServerlessPostFunctionPlugin::from_config(&cfg(serde_json::json!([
            "function execute(ctx)\n  ctx.response.headers[\"x-served-by\"] = {\"featherbit\"}\n  return ctx\nend"
        ])))
        .unwrap();
        let out = p.execute(test_context(), &HashMap::new()).await.unwrap();
        assert_eq!(
            out.context.response.headers.get("x-served-by"),
            Some(&vec!["featherbit".to_string()])
        );
        assert_eq!(p.plugin_type(), "serverless-post-function");
    }

    #[test]
    fn test_serverless_post_function_empty_rejected() {
        assert!(ServerlessPostFunctionPlugin::from_config(&HashMap::new()).is_err());
    }
}
