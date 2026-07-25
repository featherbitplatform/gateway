//! The `request-id` node — ensures every request carries a unique id header
//! and optionally echoes it on the response.
//!
//! Port of APISIX's `request-id` plugin. featherbit implements the `uuid`
//! algorithm (UUID v4); APISIX's `nanoid`, `range_id`, `ksuid` and `uuidv7`
//! algorithms are not supported and are rejected at config load.

use async_trait::async_trait;
use std::collections::HashMap;

use crate::context::Context;
use crate::plugins::{Plugin, PluginOutput, PluginResult};

/// Guarantees a request-id header on the request: when the header is absent
/// (or empty) a fresh UUID v4 is generated and set; an id supplied by the
/// client is kept as-is. With `include_in_response` the same id is also set
/// on the response, unless the response already carries one.
///
/// This plugin never fails at execution time.
///
/// Note on placement: the `upstream` node replaces `context.response.headers`
/// wholesale with the upstream's response headers. To guarantee the id on the
/// response, place a second `request-id` node after `upstream` — it finds the
/// request header already set, reuses the same id, and stamps it on the
/// response.
pub struct RequestIdPlugin {
    /// Lowercased name of the id header.
    header_name: String,
    /// When true, the id is echoed on the response (if not already present).
    include_in_response: bool,
}

impl RequestIdPlugin {
    /// Builds the plugin from node config.
    ///
    /// Accepted keys (all optional):
    /// - `header_name` (string, default `X-Request-Id`): header carrying the
    ///   id (lowercased internally, per featherbit's header convention).
    /// - `include_in_response` (bool, default `true`): also set the id on the
    ///   response when the response does not already carry the header.
    /// - `algorithm` (string, default `uuid`): id generation algorithm. Only
    ///   `uuid` (UUID v4) is supported; any other value (APISIX also offers
    ///   `nanoid`, `range_id`, `ksuid`, `uuidv7`) is a config error.
    ///
    /// ```yaml
    /// type: request-id
    /// config:
    ///   header_name: X-Request-Id
    ///   include_in_response: true
    ///   algorithm: uuid
    /// ```
    pub fn from_config(config: &HashMap<String, serde_json::Value>) -> Result<Self, String> {
        let header_name = config
            .get("header_name")
            .and_then(|v| v.as_str())
            .unwrap_or("X-Request-Id")
            .to_lowercase();

        let include_in_response = config
            .get("include_in_response")
            .and_then(|v| v.as_bool())
            .unwrap_or(true);

        match config.get("algorithm").and_then(|v| v.as_str()) {
            None | Some("uuid") => {}
            Some(other) => {
                return Err(format!(
                    "request-id algorithm '{}' is not supported — supported: uuid",
                    other
                ));
            }
        }

        Ok(Self {
            header_name,
            include_in_response,
        })
    }
}

/// Returns the first non-empty value of `header` in `headers`.
fn first_non_empty(headers: &HashMap<String, Vec<String>>, header: &str) -> Option<String> {
    headers
        .get(header)
        .and_then(|v| v.first())
        .filter(|v| !v.is_empty())
        .cloned()
}

#[async_trait]
impl Plugin for RequestIdPlugin {
    fn plugin_type(&self) -> &str {
        "request-id"
    }

    async fn execute(
        &self,
        mut ctx: Context,
        _named_inputs: &HashMap<String, serde_json::Value>,
    ) -> PluginResult {
        // Keep a client-supplied id; generate one otherwise (APISIX rewrite phase).
        let id = match first_non_empty(&ctx.request.headers, &self.header_name) {
            Some(existing) => existing,
            None => {
                let id = uuid::Uuid::new_v4().to_string();
                ctx.request
                    .headers
                    .insert(self.header_name.clone(), vec![id.clone()]);
                id
            }
        };

        // Echo on the response unless it already carries the header
        // (APISIX header_filter phase).
        if self.include_in_response
            && first_non_empty(&ctx.response.headers, &self.header_name).is_none()
        {
            ctx.response
                .headers
                .insert(self.header_name.clone(), vec![id]);
        }

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
    use bytes::Bytes;

    fn test_context() -> Context {
        Context {
            request: GatewayRequest {
                method: "GET".to_string(),
                path: "/".to_string(),
                host: "localhost".to_string(),
                scheme: "http".to_string(),
                headers: HashMap::new(),
                query_params: HashMap::new(),
                body: Bytes::new(),
                remote_addr: "127.0.0.1:12345".to_string(),
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

    #[tokio::test]
    async fn test_request_id_generates_uuid_when_absent() {
        let plugin = RequestIdPlugin::from_config(&HashMap::new()).unwrap();
        let result = plugin
            .execute(test_context(), &HashMap::new())
            .await
            .unwrap();
        let ctx = result.context;

        let req_id = &ctx.request.headers.get("x-request-id").unwrap()[0];
        // Must be a valid UUID
        assert!(
            uuid::Uuid::parse_str(req_id).is_ok(),
            "not a uuid: {req_id}"
        );
        // Echoed on the response by default
        assert_eq!(
            ctx.response.headers.get("x-request-id"),
            Some(&vec![req_id.clone()])
        );
    }

    #[tokio::test]
    async fn test_request_id_keeps_client_supplied_id() {
        let plugin = RequestIdPlugin::from_config(&HashMap::new()).unwrap();
        let mut ctx = test_context();
        ctx.request
            .headers
            .insert("x-request-id".to_string(), vec!["client-id-1".to_string()]);

        let result = plugin.execute(ctx, &HashMap::new()).await.unwrap();
        assert_eq!(
            result.context.request.headers.get("x-request-id"),
            Some(&vec!["client-id-1".to_string()])
        );
        assert_eq!(
            result.context.response.headers.get("x-request-id"),
            Some(&vec!["client-id-1".to_string()])
        );
    }

    #[tokio::test]
    async fn test_request_id_empty_header_regenerated() {
        let plugin = RequestIdPlugin::from_config(&HashMap::new()).unwrap();
        let mut ctx = test_context();
        ctx.request
            .headers
            .insert("x-request-id".to_string(), vec!["".to_string()]);

        let result = plugin.execute(ctx, &HashMap::new()).await.unwrap();
        let req_id = &result.context.request.headers.get("x-request-id").unwrap()[0];
        assert!(uuid::Uuid::parse_str(req_id).is_ok());
    }

    #[tokio::test]
    async fn test_request_id_custom_header_name() {
        let mut config = HashMap::new();
        config.insert(
            "header_name".to_string(),
            serde_json::json!("X-Correlation-Id"),
        );
        let plugin = RequestIdPlugin::from_config(&config).unwrap();
        let result = plugin
            .execute(test_context(), &HashMap::new())
            .await
            .unwrap();
        assert!(result
            .context
            .request
            .headers
            .contains_key("x-correlation-id"));
    }

    #[tokio::test]
    async fn test_request_id_include_in_response_false() {
        let mut config = HashMap::new();
        config.insert("include_in_response".to_string(), serde_json::json!(false));
        let plugin = RequestIdPlugin::from_config(&config).unwrap();
        let result = plugin
            .execute(test_context(), &HashMap::new())
            .await
            .unwrap();
        assert!(!result.context.response.headers.contains_key("x-request-id"));
    }

    #[tokio::test]
    async fn test_request_id_does_not_override_existing_response_header() {
        let plugin = RequestIdPlugin::from_config(&HashMap::new()).unwrap();
        let mut ctx = test_context();
        ctx.response
            .headers
            .insert("x-request-id".to_string(), vec!["upstream-id".to_string()]);

        let result = plugin.execute(ctx, &HashMap::new()).await.unwrap();
        assert_eq!(
            result.context.response.headers.get("x-request-id"),
            Some(&vec!["upstream-id".to_string()])
        );
    }

    #[test]
    fn test_request_id_algorithm_validation() {
        let mut config = HashMap::new();
        config.insert("algorithm".to_string(), serde_json::json!("uuid"));
        assert!(RequestIdPlugin::from_config(&config).is_ok());

        for unsupported in ["nanoid", "range_id", "ksuid", "uuidv7"] {
            let mut config = HashMap::new();
            config.insert("algorithm".to_string(), serde_json::json!(unsupported));
            let err = RequestIdPlugin::from_config(&config).err().unwrap();
            assert!(
                err.contains(unsupported),
                "error should name '{unsupported}'"
            );
            assert!(
                err.contains("uuid"),
                "error should list supported algorithms"
            );
        }
    }
}
