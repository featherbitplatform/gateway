//! Azure Functions serverless-upstream plugin (`azure-functions`).
//!
//! Port of APISIX's `azure-functions` plugin (3.17). Forwards the request to an
//! Azure Function endpoint and returns the function's reply as the gateway
//! response — it **replaces the upstream**, so the node's `success` port should
//! be wired straight to `client.in`.
//!
//! Authorization is via the Azure function key headers: `authorization.apikey`
//! is sent as `x-functions-key` and `authorization.clientid` as
//! `x-functions-clientid` (only when the client did not already supply them).
//!
//! On a callout failure the node rejects through its `error` port with
//! `AZURE_FUNCTIONS_CALLOUT_ERROR` (a 502/503/504 depending on the failure).
//!
//! # Deviations from APISIX
//!
//! - The `plugin_metadata` master-key fallback is not implemented; keys come
//!   from the node's `authorization` block only.

use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use crate::context::{Context, GatewayError};
use crate::outbound::{OutboundClient, OutboundRequest, OutboundResponse};
use crate::plugins::resources::PluginResources;
use crate::plugins::{Plugin, PluginExecutionError, PluginOutput, PluginResult};

use super::faas;

/// Forwards the request to an Azure Function and maps its reply into
/// `Context.response`.
pub struct AzureFunctionsPlugin {
    function_uri: String,
    apikey: Option<String>,
    clientid: Option<String>,
    ssl_verify: bool,
    timeout: Duration,
    client: Arc<OutboundClient>,
}

impl AzureFunctionsPlugin {
    /// Builds the plugin from node config.
    ///
    /// Accepted keys:
    /// - `function_uri` (string, **required**): the Azure Function URL. A
    ///   missing/empty value is a config-load error.
    /// - `authorization` (object, optional):
    ///   - `apikey` (string) — sent as the `x-functions-key` header.
    ///   - `clientid` (string) — sent as the `x-functions-clientid` header.
    /// - `ssl_verify` (bool, default `true`): verify TLS certificates.
    /// - `timeout` (integer ms, default `3000`): whole-call deadline.
    ///
    /// ```yaml
    /// type: azure-functions
    /// config:
    ///   function_uri: https://app.azurewebsites.net/api/HttpTrigger
    ///   authorization:
    ///     apikey: ${AZURE_FUNCTION_KEY}
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
            .ok_or_else(|| "azure-functions plugin requires 'function_uri'".to_string())?
            .to_string();

        let authz = config.get("authorization").and_then(|v| v.as_object());
        let apikey = authz
            .and_then(|a| a.get("apikey"))
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(String::from);
        let clientid = authz
            .and_then(|a| a.get("clientid"))
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(String::from);

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
            apikey,
            clientid,
            ssl_verify,
            timeout,
            client: resources.outbound.clone(),
        })
    }

    /// Builds the outbound request: the client's method/body/query forwarded to
    /// `function_uri` plus the Azure function key headers.
    fn build_request(&self, ctx: &Context) -> Result<OutboundRequest, String> {
        let (mut headers, url, method) = faas::forward_parts(&self.function_uri, ctx)?;
        let client_has = |name: &str| {
            ctx.request
                .headers
                .keys()
                .any(|k| k.eq_ignore_ascii_case(name))
        };
        // Only set the key headers when the client did not already send them.
        if !client_has("x-functions-key") && !client_has("x-functions-clientid") {
            if let Some(apikey) = &self.apikey {
                headers.push(("x-functions-key".to_string(), apikey.clone()));
            }
            if let Some(clientid) = &self.clientid {
                headers.push(("x-functions-clientid".to_string(), clientid.clone()));
            }
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
impl Plugin for AzureFunctionsPlugin {
    fn plugin_type(&self) -> &str {
        "azure-functions"
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
                let (status, message) = faas::classify_error("azure-functions", &e);
                Err(reject(ctx, status, message))
            }
        }
    }
}

/// Builds the `AZURE_FUNCTIONS_CALLOUT_ERROR` rejection carrying the context.
fn reject(mut ctx: Context, status: u16, message: String) -> PluginExecutionError {
    ctx.response.status_code = status;
    PluginExecutionError {
        context: ctx,
        error: GatewayError {
            node_id: String::new(),
            code: "AZURE_FUNCTIONS_CALLOUT_ERROR".to_string(),
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

    fn plugin(config: serde_json::Value) -> AzureFunctionsPlugin {
        let map: HashMap<String, serde_json::Value> = serde_json::from_value(config).unwrap();
        AzureFunctionsPlugin::from_config(&map, &PluginResources::empty()).unwrap()
    }

    #[test]
    fn test_requires_function_uri() {
        assert!(
            AzureFunctionsPlugin::from_config(&HashMap::new(), &PluginResources::empty()).is_err()
        );
    }

    #[test]
    fn test_build_request_sets_key_headers_and_url() {
        let p = plugin(serde_json::json!({
            "function_uri": "https://app.azurewebsites.net/api/Trigger",
            "authorization": { "apikey": "K", "clientid": "C" }
        }));
        let req = p.build_request(&ctx()).unwrap();
        assert_eq!(req.url, "https://app.azurewebsites.net/api/Trigger");
        assert_eq!(req.method, http::Method::POST);
        let get = |n: &str| {
            req.headers
                .iter()
                .find(|(k, _)| k == n)
                .map(|(_, v)| v.as_str())
        };
        assert_eq!(get("x-functions-key"), Some("K"));
        assert_eq!(get("x-functions-clientid"), Some("C"));
        // Host overridden with the function endpoint.
        assert_eq!(get("host"), Some("app.azurewebsites.net"));
    }

    #[test]
    fn test_client_supplied_key_not_overwritten() {
        let p = plugin(serde_json::json!({
            "function_uri": "https://app.azurewebsites.net/api/Trigger",
            "authorization": { "apikey": "K" }
        }));
        let mut c = ctx();
        c.request.headers.insert(
            "x-functions-key".to_string(),
            vec!["client-key".to_string()],
        );
        let req = p.build_request(&c).unwrap();
        let keys: Vec<&str> = req
            .headers
            .iter()
            .filter(|(k, _)| k == "x-functions-key")
            .map(|(_, v)| v.as_str())
            .collect();
        // The plugin must not append its own key on top of the client's.
        assert_eq!(keys, vec!["client-key"]);
    }

    #[tokio::test]
    async fn test_callout_failure_routes_error() {
        let p = plugin(serde_json::json!({
            "function_uri": "http://127.0.0.1:1/fn",
            "timeout": 200
        }));
        let err = p.execute(ctx(), &HashMap::new()).await.unwrap_err();
        assert_eq!(err.error.code, "AZURE_FUNCTIONS_CALLOUT_ERROR");
        assert!(err.context.response.status_code >= 502);
    }
}
