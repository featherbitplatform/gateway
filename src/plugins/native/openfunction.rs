//! OpenFunction serverless-upstream plugin (`openfunction`).
//!
//! Port of APISIX's `openfunction` plugin (3.17). Forwards the request to an
//! OpenFunction endpoint and returns the function's reply as the gateway
//! response — it **replaces the upstream**, so the node's `success` port should
//! be wired straight to `client.in`.
//!
//! Authorization: when `authorization.service_token` is set, it is sent as an
//! `Authorization: Basic <base64(service_token)>` header (matching APISIX,
//! which base64-encodes the raw token verbatim).
//!
//! On a callout failure the node rejects through its `error` port with
//! `OPENFUNCTION_CALLOUT_ERROR` (a 502/503/504 depending on the failure).

use async_trait::async_trait;
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use crate::context::{Context, GatewayError};
use crate::outbound::{OutboundClient, OutboundRequest, OutboundResponse};
use crate::plugins::resources::PluginResources;
use crate::plugins::{Plugin, PluginExecutionError, PluginOutput, PluginResult};

use super::faas;

/// Forwards the request to an OpenFunction endpoint and maps its reply into
/// `Context.response`.
pub struct OpenFunctionPlugin {
    function_uri: String,
    /// Pre-computed `Basic <base64(service_token)>` header value, if configured.
    authorization: Option<String>,
    ssl_verify: bool,
    timeout: Duration,
    client: Arc<OutboundClient>,
}

impl OpenFunctionPlugin {
    /// Builds the plugin from node config.
    ///
    /// Accepted keys:
    /// - `function_uri` (string, **required**): the OpenFunction URL. A
    ///   missing/empty value is a config-load error.
    /// - `authorization` (object, optional):
    ///   - `service_token` (string) — sent as `Authorization: Basic
    ///     <base64(service_token)>`.
    /// - `ssl_verify` (bool, default `true`): verify TLS certificates.
    /// - `timeout` (integer ms, default `3000`): whole-call deadline.
    ///
    /// ```yaml
    /// type: openfunction
    /// config:
    ///   function_uri: http://openfunction.svc/default/hello
    ///   authorization:
    ///     service_token: ${OPENFUNCTION_TOKEN}
    ///   ssl_verify: true
    ///   timeout: 3000
    /// ```
    pub fn from_config(
        config: &HashMap<String, serde_json::Value>,
        resources: &Arc<PluginResources>,
    ) -> Result<Self, String> {
        let function_uri = config
            .get("function_uri")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| "openfunction plugin requires 'function_uri'".to_string())?
            .to_string();

        let authorization = config
            .get("authorization")
            .and_then(|v| v.as_object())
            .and_then(|a| a.get("service_token"))
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(|token| format!("Basic {}", BASE64.encode(token)));

        let ssl_verify = config
            .get("ssl_verify")
            .and_then(|v| v.as_bool())
            .unwrap_or(true);

        let timeout = Duration::from_millis(
            config
                .get("timeout")
                .and_then(|v| v.as_u64())
                .unwrap_or(3000),
        );

        Ok(Self {
            function_uri,
            authorization,
            ssl_verify,
            timeout,
            client: resources.outbound.clone(),
        })
    }

    /// Builds the outbound request: the client's method/body/query forwarded to
    /// `function_uri` plus the optional `Authorization: Basic` header.
    fn build_request(&self, ctx: &Context) -> Result<OutboundRequest, String> {
        let (mut headers, url, method) = faas::forward_parts(&self.function_uri, ctx)?;
        if let Some(authz) = &self.authorization {
            headers.retain(|(k, _)| k != "authorization");
            headers.push(("authorization".to_string(), authz.clone()));
        }
        Ok(OutboundRequest {
            method,
            url,
            headers,
            body: ctx.request.body.clone(),
            timeout: self.timeout,
            ssl_verify: self.ssl_verify,
        })
    }
}

/// Copies the FaaS reply into `Context.response`.
fn apply_response(ctx: &mut Context, response: OutboundResponse) {
    ctx.response.status_code = response.status;
    ctx.response.headers = response.headers;
    ctx.response.body = response.body;
}

#[async_trait]
impl Plugin for OpenFunctionPlugin {
    fn plugin_type(&self) -> &str {
        "openfunction"
    }

    async fn execute(
        &self,
        mut ctx: Context,
        _named_inputs: &HashMap<String, serde_json::Value>,
    ) -> PluginResult {
        let request = match self.build_request(&ctx) {
            Ok(req) => req,
            Err(message) => return Err(reject(ctx, 502, message)),
        };

        match self.client.request(request).await {
            Ok(response) => {
                apply_response(&mut ctx, response);
                Ok(PluginOutput {
                    context: ctx,
                    named_outputs: HashMap::new(),
                })
            }
            Err(e) => {
                let (status, message) = faas::classify_error("openfunction", &e);
                Err(reject(ctx, status, message))
            }
        }
    }
}

/// Builds the `OPENFUNCTION_CALLOUT_ERROR` rejection carrying the context.
fn reject(mut ctx: Context, status: u16, message: String) -> PluginExecutionError {
    ctx.response.status_code = status;
    PluginExecutionError {
        context: ctx,
        error: GatewayError {
            node_id: String::new(),
            code: "OPENFUNCTION_CALLOUT_ERROR".to_string(),
            message,
            metadata: HashMap::new(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::{GatewayRequest, GatewayResponse, Protocol};
    use bytes::Bytes;

    fn ctx() -> Context {
        Context {
            request: GatewayRequest {
                method: "POST".to_string(),
                path: "/orig".to_string(),
                host: "gw".to_string(),
                scheme: "http".to_string(),
                headers: HashMap::new(),
                query_params: HashMap::new(),
                body: Bytes::from_static(b"payload"),
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

    fn plugin(config: serde_json::Value) -> OpenFunctionPlugin {
        let map: HashMap<String, serde_json::Value> = serde_json::from_value(config).unwrap();
        OpenFunctionPlugin::from_config(&map, &PluginResources::empty()).unwrap()
    }

    #[test]
    fn test_requires_function_uri() {
        assert!(
            OpenFunctionPlugin::from_config(&HashMap::new(), &PluginResources::empty()).is_err()
        );
    }

    #[test]
    fn test_basic_auth_header_base64() {
        let p = plugin(serde_json::json!({
            "function_uri": "http://of.svc/default/hello",
            "authorization": { "service_token": "user:pass" }
        }));
        let req = p.build_request(&ctx()).unwrap();
        let authz = req
            .headers
            .iter()
            .find(|(k, _)| k == "authorization")
            .map(|(_, v)| v.clone())
            .unwrap();
        // "user:pass" base64 = dXNlcjpwYXNz
        assert_eq!(authz, "Basic dXNlcjpwYXNz");
        assert_eq!(req.url, "http://of.svc/default/hello");
    }

    #[test]
    fn test_no_auth_when_absent() {
        let p = plugin(serde_json::json!({ "function_uri": "http://of.svc/fn" }));
        let req = p.build_request(&ctx()).unwrap();
        assert!(!req.headers.iter().any(|(k, _)| k == "authorization"));
    }

    #[tokio::test]
    async fn test_callout_failure_routes_error() {
        let p = plugin(serde_json::json!({
            "function_uri": "http://127.0.0.1:1/fn",
            "timeout": 200
        }));
        let err = p.execute(ctx(), &HashMap::new()).await.unwrap_err();
        assert_eq!(err.error.code, "OPENFUNCTION_CALLOUT_ERROR");
        assert!(err.context.response.status_code >= 502);
    }
}
