//! The `error-handler` node — turns accumulated gateway errors into a JSON
//! error response. Typically wired to other nodes' error ports.

use async_trait::async_trait;
use bytes::Bytes;
use std::collections::HashMap;

use crate::context::Context;
use crate::plugins::{Plugin, PluginOutput, PluginResult};

/// Overwrites `Context.response` with a configured status code and a body
/// rendered from a template. The template may reference the most recent entry
/// in `Context.errors` via `{{error.code}}`, `{{error.message}}`, and
/// `{{error.node_id}}` placeholders; the content type is forced to
/// `application/json`.
pub struct ErrorHandlerPlugin {
    status_code: u16,
    body_template: String,
}

impl ErrorHandlerPlugin {
    /// Builds the plugin from node config.
    ///
    /// Accepted keys (all optional; this constructor never errors):
    /// - `status_code` (integer, default `500`): HTTP status of the error
    ///   response.
    /// - `body_template` (string, default a generic
    ///   `{"error": "internal_error", ...}` JSON body): response body, with
    ///   `{{error.code}}`, `{{error.message}}`, and `{{error.node_id}}`
    ///   substituted from the last error in the context at execution time.
    ///
    /// ```yaml
    /// type: error-handler
    /// config:
    ///   status_code: 502
    ///   body_template: '{"error": "{{error.code}}", "message": "{{error.message}}"}'
    /// ```
    pub fn from_config(config: &HashMap<String, serde_json::Value>) -> Result<Self, String> {
        let status_code = config
            .get("status_code")
            .and_then(|v| v.as_u64())
            .unwrap_or(500) as u16;

        let body_template = config
            .get("body_template")
            .and_then(|v| v.as_str())
            .unwrap_or(r#"{"error": "internal_error", "message": "An unexpected error occurred"}"#)
            .to_string();

        Ok(Self {
            status_code,
            body_template,
        })
    }
}

#[async_trait]
impl Plugin for ErrorHandlerPlugin {
    fn plugin_type(&self) -> &str {
        "error-handler"
    }

    async fn execute(
        &self,
        mut ctx: Context,
        _named_inputs: &HashMap<String, serde_json::Value>,
    ) -> PluginResult {
        // Render the template using the last error in the context
        let body = if let Some(last_error) = ctx.errors.last() {
            self.body_template
                .replace("{{error.code}}", &last_error.code)
                .replace("{{error.message}}", &last_error.message)
                .replace("{{error.node_id}}", &last_error.node_id)
        } else {
            self.body_template.clone()
        };

        ctx.response.status_code = self.status_code;
        ctx.response.body = Bytes::from(body);
        ctx.response.headers.insert(
            "content-type".to_string(),
            vec!["application/json".to_string()],
        );

        Ok(PluginOutput {
            context: ctx,
            named_outputs: HashMap::new(),
        })
    }
}

#[cfg(test)]
mod tests {
    //! featherbit-native plugin (no direct APISIX counterpart), so these derive
    //! from featherbit's own spec: render the body template with the last error's
    //! fields substituted, and apply the configured status code.
    use super::*;
    use crate::context::{GatewayError, GatewayRequest, GatewayResponse, Protocol};

    fn ctx_with_error(err: Option<GatewayError>) -> Context {
        Context {
            request: GatewayRequest {
                method: "GET".to_string(),
                path: "/hello".to_string(),
                host: "h".to_string(),
                scheme: "http".to_string(),
                headers: HashMap::new(),
                query_params: HashMap::new(),
                body: Bytes::new(),
                remote_addr: "1.2.3.4:5".to_string(),
                protocol: Protocol::Http1,
            },
            response: GatewayResponse {
                status_code: 0,
                headers: HashMap::new(),
                body: Bytes::new(),
            },
            message: HashMap::new(),
            errors: err.into_iter().collect(),
        }
    }

    fn err(code: &str, message: &str, node_id: &str) -> GatewayError {
        GatewayError {
            node_id: node_id.to_string(),
            code: code.to_string(),
            message: message.to_string(),
            metadata: HashMap::new(),
        }
    }

    fn plugin(config: serde_json::Value) -> ErrorHandlerPlugin {
        let map: HashMap<String, serde_json::Value> =
            config.as_object().unwrap().clone().into_iter().collect();
        ErrorHandlerPlugin::from_config(&map).unwrap()
    }

    #[tokio::test]
    async fn test_renders_error_fields_and_status() {
        let p = plugin(serde_json::json!({
            "status_code": 502,
            "body_template": "{\"code\":\"{{error.code}}\",\"msg\":\"{{error.message}}\",\"node\":\"{{error.node_id}}\"}"
        }));
        let out = p
            .execute(
                ctx_with_error(Some(err("UPSTREAM_ERROR", "connection refused", "backend"))),
                &HashMap::new(),
            )
            .await
            .unwrap();

        assert_eq!(out.context.response.status_code, 502);
        let body = String::from_utf8(out.context.response.body.to_vec()).unwrap();
        assert!(body.contains("UPSTREAM_ERROR"));
        assert!(body.contains("connection refused"));
        assert!(body.contains("backend"));
        // No placeholders left unrendered.
        assert!(!body.contains("{{"));
    }

    #[tokio::test]
    async fn test_uses_the_last_error() {
        // The template renders the most recent error, matching graph semantics
        // where errors accumulate and the handler reports the latest.
        let mut ctx = ctx_with_error(Some(err("FIRST", "first", "a")));
        ctx.errors.push(err("SECOND", "second", "b"));
        let out = plugin(serde_json::json!({ "body_template": "{{error.code}}" }))
            .execute(ctx, &HashMap::new())
            .await
            .unwrap();
        assert_eq!(
            String::from_utf8(out.context.response.body.to_vec()).unwrap(),
            "SECOND"
        );
    }

    #[tokio::test]
    async fn test_no_error_leaves_template_literal() {
        let out = plugin(serde_json::json!({ "body_template": "{{error.code}}" }))
            .execute(ctx_with_error(None), &HashMap::new())
            .await
            .unwrap();
        // With no error to substitute, the raw template is emitted unchanged.
        assert_eq!(
            String::from_utf8(out.context.response.body.to_vec()).unwrap(),
            "{{error.code}}"
        );
    }
}
