//! The `logging` node — emits a structured JSON access-log line for the
//! current request/response via `tracing` (target `access_log`).

use async_trait::async_trait;
use std::collections::HashMap;
use tracing::info;

use crate::context::Context;
use crate::plugins::{Plugin, PluginOutput, PluginResult};

/// Logs a JSON record (method, path, host, remote address, status, response
/// body size, and any accumulated errors) at `info` level under the
/// `access_log` target, then passes the context through unchanged.
pub struct LoggingPlugin {
    include_headers: bool,
    #[allow(dead_code)] // parsed config; body logging not yet emitted
    include_body: bool,
}

impl LoggingPlugin {
    /// Builds the plugin from node config.
    ///
    /// Accepted keys (all optional; this constructor never errors):
    /// - `include_headers` (bool, default `false`): also log request and
    ///   response headers.
    /// - `include_body` (bool, default `false`): reserved flag, currently
    ///   parsed but not acted on by `execute` (only the response body *size*
    ///   is logged).
    ///
    /// ```yaml
    /// type: logging
    /// config:
    ///   include_headers: true
    /// ```
    pub fn from_config(config: &HashMap<String, serde_json::Value>) -> Result<Self, String> {
        let include_headers = config
            .get("include_headers")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        let include_body = config
            .get("include_body")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        Ok(Self {
            include_headers,
            include_body,
        })
    }
}

#[async_trait]
impl Plugin for LoggingPlugin {
    fn plugin_type(&self) -> &str {
        "logging"
    }

    async fn execute(
        &self,
        ctx: Context,
        _named_inputs: &HashMap<String, serde_json::Value>,
    ) -> PluginResult {
        let mut fields = serde_json::json!({
            "method": ctx.request.method,
            "path": ctx.request.path,
            "host": ctx.request.host,
            "remote_addr": ctx.request.remote_addr,
            "status": ctx.response.status_code,
            "response_body_bytes": ctx.response.body.len(),
        });

        if self.include_headers {
            fields["request_headers"] =
                serde_json::to_value(&ctx.request.headers).unwrap_or_default();
            fields["response_headers"] =
                serde_json::to_value(&ctx.response.headers).unwrap_or_default();
        }

        if !ctx.errors.is_empty() {
            fields["errors"] = serde_json::to_value(&ctx.errors).unwrap_or_default();
        }

        info!(target: "access_log", "{}", fields);

        Ok(PluginOutput {
            context: ctx,
            named_outputs: HashMap::new(),
        })
    }
}

#[cfg(test)]
mod tests {
    //! featherbit-native structured access logger. It is a fire-and-forget
    //! pass-through: the log line goes to the tracing subscriber (asserting its
    //! content needs a subscriber, out of scope here), so these tests pin the two
    //! things unit-testable in isolation: config parsing and that the plugin never
    //! alters the request/response it logs.
    use super::*;
    use crate::context::{GatewayRequest, GatewayResponse, Protocol};
    use bytes::Bytes;

    fn ctx() -> Context {
        Context {
            request: GatewayRequest {
                method: "GET".to_string(),
                path: "/hello".to_string(),
                host: "h".to_string(),
                scheme: "http".to_string(),
                headers: HashMap::new(),
                query_params: HashMap::new(),
                body: Bytes::from_static(b"req"),
                remote_addr: "1.2.3.4:5".to_string(),
                protocol: Protocol::Http1,
            },
            response: GatewayResponse {
                status_code: 200,
                headers: HashMap::new(),
                body: Bytes::from_static(b"resp"),
            },
            message: HashMap::new(),
            errors: Vec::new(),
        }
    }

    #[tokio::test]
    async fn test_passes_context_through_unchanged() {
        let mut map = HashMap::new();
        map.insert("include_headers".to_string(), serde_json::json!(true));
        let plugin = LoggingPlugin::from_config(&map).unwrap();

        let out = plugin.execute(ctx(), &HashMap::new()).await.unwrap();
        // A logger must not mutate the traffic it observes.
        assert_eq!(out.context.response.status_code, 200);
        assert_eq!(out.context.response.body, Bytes::from_static(b"resp"));
        assert_eq!(out.context.request.path, "/hello");
    }

    #[test]
    fn test_config_defaults_off() {
        let plugin = LoggingPlugin::from_config(&HashMap::new()).unwrap();
        assert!(!plugin.include_headers);
        assert!(!plugin.include_body);
    }
}
