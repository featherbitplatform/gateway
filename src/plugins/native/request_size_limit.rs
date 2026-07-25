//! Request body size limit plugin (`request-size-limit`).
//!
//! Rejects requests whose body exceeds a configured byte limit with a 413
//! error routed through the node's error port.

use async_trait::async_trait;
use bytes::Bytes;
use std::collections::HashMap;

use crate::context::{Context, GatewayError};
use crate::plugins::{Plugin, PluginExecutionError, PluginOutput, PluginResult};

/// Enforces a maximum request body size.
///
/// Compares the already-buffered body length against `max_bytes`; oversized
/// requests get a 413 JSON response and a `PAYLOAD_TOO_LARGE` error carrying
/// the context so the graph engine routes through the error port. Does not
/// write to `context.message`.
pub struct RequestSizeLimitPlugin {
    /// Maximum allowed request body size in bytes.
    max_bytes: usize,
}

impl RequestSizeLimitPlugin {
    /// Builds the plugin from node config. Never fails.
    ///
    /// Accepted keys:
    /// - `max_bytes` (integer, default `1048576` = 1 MiB): maximum request
    ///   body size in bytes.
    ///
    /// ```yaml
    /// type: request-size-limit
    /// config:
    ///   max_bytes: 262144
    /// ```
    pub fn from_config(config: &HashMap<String, serde_json::Value>) -> Result<Self, String> {
        let max_bytes = config
            .get("max_bytes")
            .and_then(|v| v.as_u64())
            .unwrap_or(1_048_576) as usize; // 1MB default

        Ok(Self { max_bytes })
    }
}

#[async_trait]
impl Plugin for RequestSizeLimitPlugin {
    fn plugin_type(&self) -> &str {
        "request-size-limit"
    }

    async fn execute(
        &self,
        ctx: Context,
        _named_inputs: &HashMap<String, serde_json::Value>,
    ) -> PluginResult {
        let body_len = ctx.request.body.len();
        if body_len > self.max_bytes {
            let mut ctx = ctx;
            ctx.response.status_code = 413;
            ctx.response.body = Bytes::from(
                r#"{"error": "payload_too_large", "message": "Request body exceeds size limit"}"#,
            );
            ctx.response.headers.insert(
                "content-type".to_string(),
                vec!["application/json".to_string()],
            );
            return Err(PluginExecutionError {
                context: ctx,
                error: GatewayError {
                    node_id: String::new(),
                    code: "PAYLOAD_TOO_LARGE".to_string(),
                    message: format!("Body size {} exceeds limit {}", body_len, self.max_bytes),
                    metadata: HashMap::new(),
                },
            });
        }

        Ok(PluginOutput {
            context: ctx,
            named_outputs: HashMap::new(),
        })
    }
}

#[cfg(test)]
mod tests {
    //! featherbit-native plugin (APISIX handles body limits at the nginx layer,
    //! not as a portable plugin), so these derive from featherbit's own spec:
    //! reject bodies larger than `max_bytes` with 413, measured on the actual
    //! buffered body length.
    use super::*;
    use crate::context::{GatewayRequest, GatewayResponse, Protocol};

    fn ctx(body: &[u8]) -> Context {
        Context {
            request: GatewayRequest {
                method: "POST".to_string(),
                path: "/hello".to_string(),
                host: "h".to_string(),
                scheme: "http".to_string(),
                headers: HashMap::new(),
                query_params: HashMap::new(),
                body: Bytes::copy_from_slice(body),
                remote_addr: "1.2.3.4:5".to_string(),
                protocol: Protocol::Http1,
            },
            response: GatewayResponse {
                status_code: 0,
                headers: HashMap::new(),
                body: Bytes::new(),
            },
            message: HashMap::new(),
            errors: Vec::new(),
        }
    }

    fn plugin(max_bytes: u64) -> RequestSizeLimitPlugin {
        let mut map = HashMap::new();
        map.insert("max_bytes".to_string(), serde_json::json!(max_bytes));
        RequestSizeLimitPlugin::from_config(&map).unwrap()
    }

    #[tokio::test]
    async fn test_body_under_limit_passes() {
        let out = plugin(10).execute(ctx(b"12345"), &HashMap::new()).await;
        assert!(out.is_ok());
    }

    #[tokio::test]
    async fn test_body_at_limit_passes() {
        // Boundary: exactly max_bytes is allowed (only strictly greater fails).
        let out = plugin(5).execute(ctx(b"12345"), &HashMap::new()).await;
        assert!(out.is_ok());
    }

    #[tokio::test]
    async fn test_body_over_limit_rejected_413() {
        let err = plugin(5)
            .execute(ctx(b"123456"), &HashMap::new())
            .await
            .unwrap_err();
        assert_eq!(err.error.code, "PAYLOAD_TOO_LARGE");
        assert_eq!(err.context.response.status_code, 413);
    }

    #[tokio::test]
    async fn test_empty_body_passes() {
        assert!(plugin(0).execute(ctx(b""), &HashMap::new()).await.is_ok());
    }

    #[test]
    fn test_default_limit_is_1mib() {
        let p = RequestSizeLimitPlugin::from_config(&HashMap::new()).unwrap();
        assert_eq!(p.max_bytes, 1_048_576);
    }
}
