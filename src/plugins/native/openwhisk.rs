//! Apache OpenWhisk serverless-upstream plugin (`openwhisk`).
//!
//! Port of APISIX's `openwhisk` plugin (3.17). Invokes an OpenWhisk action
//! (blocking, with the result inlined) and returns the action's reply as the
//! gateway response — it **replaces the upstream**, so the node's `success`
//! port should be wired straight to `client.in`.
//!
//! The request body is POSTed as the action parameters to
//! `<api_host>/api/v1/namespaces/<namespace>/actions/<package/><action>?blocking=true&result=<result>&timeout=<ms>`
//! with an `Authorization: Basic <base64(service_token)>` header.
//!
//! # Response mapping
//!
//! OpenWhisk returns a JSON envelope. An action may return just a body, or set
//! `statusCode` and `headers` explicitly. [`map_response`] mirrors APISIX:
//! `statusCode` (when present) becomes the response status, `headers` are
//! applied, and `body` (or the raw envelope when absent) becomes the body. A
//! non-JSON envelope fails the node with `503` through the error port.

use async_trait::async_trait;
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use bytes::Bytes;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use crate::context::{Context, GatewayError};
use crate::outbound::{OutboundClient, OutboundRequest, OutboundResponse};
use crate::plugins::resources::PluginResources;
use crate::plugins::{Plugin, PluginExecutionError, PluginOutput, PluginResult};

use super::faas;

/// Invokes an OpenWhisk action and maps its reply into `Context.response`.
pub struct OpenWhiskPlugin {
    api_host: String,
    /// Pre-computed `Basic <base64(service_token)>` header value.
    authorization: String,
    namespace: String,
    package: Option<String>,
    action: String,
    result: bool,
    ssl_verify: bool,
    timeout: Duration,
    /// `timeout` in milliseconds, passed through as the action query param.
    timeout_ms: u64,
    client: Arc<OutboundClient>,
}

impl OpenWhiskPlugin {
    /// Builds the plugin from node config.
    ///
    /// Accepted keys:
    /// - `api_host` (string, **required**): the OpenWhisk API host
    ///   (e.g. `https://ow.example.com`). Missing/empty is a config error.
    /// - `service_token` (string, **required**): `user:pass` action token, sent
    ///   base64-encoded as HTTP Basic auth. Missing/empty is a config error.
    /// - `action` (string, **required**): the action name to invoke.
    /// - `namespace` (string, default `_`): the OpenWhisk namespace.
    /// - `package` (string, optional): the package the action belongs to.
    /// - `result` (bool, default `true`): request `result=true` (inline the
    ///   action result rather than the full activation record).
    /// - `ssl_verify` (bool, default `true`): verify TLS certificates.
    /// - `timeout` (integer ms, default `3000`): whole-call deadline, also
    ///   passed as the action `timeout` query parameter.
    ///
    /// ```yaml
    /// type: openwhisk
    /// config:
    ///   api_host: https://ow.example.com
    ///   service_token: ${OPENWHISK_TOKEN}
    ///   namespace: guest
    ///   action: hello
    ///   result: true
    ///   ssl_verify: true
    ///   timeout: 3000
    /// ```
    pub fn from_config(
        config: &HashMap<String, serde_json::Value>,
        resources: &Arc<PluginResources>,
    ) -> Result<Self, String> {
        let api_host = config
            .get("api_host")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| "openwhisk plugin requires 'api_host'".to_string())?
            .trim_end_matches('/')
            .to_string();

        let service_token = config
            .get("service_token")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| "openwhisk plugin requires 'service_token'".to_string())?;
        let authorization = format!("Basic {}", BASE64.encode(service_token));

        let action = config
            .get("action")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| "openwhisk plugin requires 'action'".to_string())?
            .to_string();

        let namespace = config
            .get("namespace")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .unwrap_or("_")
            .to_string();

        let package = config
            .get("package")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(String::from);

        let result = config
            .get("result")
            .and_then(|v| v.as_bool())
            .unwrap_or(true);

        let ssl_verify = config
            .get("ssl_verify")
            .and_then(|v| v.as_bool())
            .unwrap_or(true);

        let timeout_ms = config
            .get("timeout")
            .and_then(|v| v.as_u64())
            .unwrap_or(3000);

        Ok(Self {
            api_host,
            authorization,
            namespace,
            package,
            action,
            result,
            ssl_verify,
            timeout: Duration::from_millis(timeout_ms),
            timeout_ms,
            client: resources.outbound.clone(),
        })
    }

    /// Builds the OpenWhisk action-invocation URL.
    fn endpoint(&self) -> String {
        let package = self
            .package
            .as_ref()
            .map(|p| format!("{}/", p))
            .unwrap_or_default();
        format!(
            "{}/api/v1/namespaces/{}/actions/{}{}?blocking=true&result={}&timeout={}",
            self.api_host, self.namespace, package, self.action, self.result, self.timeout_ms
        )
    }

    /// Builds the outbound POST request carrying the client body as the action
    /// parameters.
    fn build_request(&self, ctx: &Context) -> OutboundRequest {
        let headers = vec![
            ("authorization".to_string(), self.authorization.clone()),
            ("content-type".to_string(), "application/json".to_string()),
        ];
        OutboundRequest {
            method: http::Method::POST,
            url: self.endpoint(),
            headers,
            body: ctx.request.body.clone(),
            timeout: self.timeout,
            ssl_verify: self.ssl_verify,
        }
    }
}

/// The status, headers, and body mapped out of an OpenWhisk envelope.
struct MappedResponse {
    status: u16,
    headers: HashMap<String, Vec<String>>,
    body: Bytes,
}

/// Maps an OpenWhisk activation reply into a `(status, headers, body)` triple.
///
/// An empty body passes the transport status/body through untouched. A
/// well-formed JSON envelope may override the status via `statusCode`, set
/// response `headers`, and provide `body` (string or nested JSON); when
/// `statusCode`/`body` are absent the transport status / raw envelope are used.
/// A non-JSON, non-empty body is an error (mapped to `503` by the caller).
fn map_response(status: u16, raw: &Bytes) -> Result<MappedResponse, String> {
    if raw.is_empty() {
        return Ok(MappedResponse {
            status,
            headers: HashMap::new(),
            body: raw.clone(),
        });
    }

    let envelope: serde_json::Value = serde_json::from_slice(raw)
        .map_err(|e| format!("failed to parse openwhisk response: {}", e))?;

    let mut headers: HashMap<String, Vec<String>> = HashMap::new();
    if let Some(hdrs) = envelope.get("headers").and_then(|v| v.as_object()) {
        for (name, value) in hdrs {
            let value = match value {
                serde_json::Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            headers
                .entry(name.to_ascii_lowercase())
                .or_default()
                .push(value);
        }
    }

    let code = envelope
        .get("statusCode")
        .and_then(|v| v.as_u64())
        .map(|c| c as u16)
        .unwrap_or(status);

    let body = match envelope.get("body") {
        Some(serde_json::Value::String(s)) => Bytes::from(s.clone().into_bytes()),
        Some(other) => Bytes::from(other.to_string().into_bytes()),
        None => raw.clone(),
    };

    Ok(MappedResponse {
        status: code,
        headers,
        body,
    })
}

#[async_trait]
impl Plugin for OpenWhiskPlugin {
    fn plugin_type(&self) -> &str {
        "openwhisk"
    }

    async fn execute(
        &self,
        mut ctx: Context,
        _named_inputs: &HashMap<String, serde_json::Value>,
    ) -> PluginResult {
        let request = self.build_request(&ctx);

        let response: OutboundResponse = match self.client.request(request).await {
            Ok(resp) => resp,
            Err(e) => {
                let (status, message) = faas::classify_error("openwhisk", &e);
                return Err(reject(ctx, status, message));
            }
        };

        match map_response(response.status, &response.body) {
            Ok(mapped) => {
                ctx.response.status_code = mapped.status;
                ctx.response.headers = mapped.headers;
                ctx.response.body = mapped.body;
                Ok(PluginOutput {
                    context: ctx,
                    named_outputs: HashMap::new(),
                })
            }
            Err(message) => Err(reject(ctx, 503, message)),
        }
    }
}

/// Builds the `OPENWHISK_CALLOUT_ERROR` rejection carrying the context.
fn reject(mut ctx: Context, status: u16, message: String) -> PluginExecutionError {
    ctx.response.status_code = status;
    PluginExecutionError {
        context: ctx,
        error: GatewayError {
            node_id: String::new(),
            code: "OPENWHISK_CALLOUT_ERROR".to_string(),
            message,
            metadata: HashMap::new(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::{GatewayRequest, GatewayResponse, Protocol};

    fn ctx() -> Context {
        Context {
            request: GatewayRequest {
                method: "POST".to_string(),
                path: "/orig".to_string(),
                host: "gw".to_string(),
                scheme: "http".to_string(),
                headers: HashMap::new(),
                query_params: HashMap::new(),
                body: Bytes::from_static(b"{\"name\":\"x\"}"),
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

    fn plugin(config: serde_json::Value) -> OpenWhiskPlugin {
        let map: HashMap<String, serde_json::Value> = serde_json::from_value(config).unwrap();
        OpenWhiskPlugin::from_config(&map, &PluginResources::empty()).unwrap()
    }

    fn base_config() -> serde_json::Value {
        serde_json::json!({
            "api_host": "https://ow.example.com",
            "service_token": "user:pass",
            "namespace": "guest",
            "action": "hello"
        })
    }

    #[test]
    fn test_requires_api_host_and_token_and_action() {
        assert!(OpenWhiskPlugin::from_config(&HashMap::new(), &PluginResources::empty()).is_err());
        let mut cfg: HashMap<String, serde_json::Value> =
            serde_json::from_value(serde_json::json!({ "api_host": "https://x" })).unwrap();
        assert!(OpenWhiskPlugin::from_config(&cfg, &PluginResources::empty()).is_err());
        cfg.insert("service_token".to_string(), serde_json::json!("t"));
        // still missing action
        assert!(OpenWhiskPlugin::from_config(&cfg, &PluginResources::empty()).is_err());
    }

    #[test]
    fn test_endpoint_and_auth() {
        let p = plugin(base_config());
        let req = p.build_request(&ctx());
        assert_eq!(
            req.url,
            "https://ow.example.com/api/v1/namespaces/guest/actions/hello?blocking=true&result=true&timeout=3000"
        );
        let authz = req
            .headers
            .iter()
            .find(|(k, _)| k == "authorization")
            .map(|(_, v)| v.clone())
            .unwrap();
        // "user:pass" base64 = dXNlcjpwYXNz
        assert_eq!(authz, "Basic dXNlcjpwYXNz");
        // body forwarded as action params
        assert_eq!(req.body, Bytes::from_static(b"{\"name\":\"x\"}"));
    }

    #[test]
    fn test_endpoint_with_package_and_result_false() {
        let mut cfg = base_config();
        cfg["package"] = serde_json::json!("mypkg");
        cfg["result"] = serde_json::json!(false);
        let p = plugin(cfg);
        assert_eq!(
            p.endpoint(),
            "https://ow.example.com/api/v1/namespaces/guest/actions/mypkg/hello?blocking=true&result=false&timeout=3000"
        );
    }

    #[test]
    fn test_map_response_status_code_and_headers_and_body() {
        let raw = Bytes::from_static(
            br#"{"statusCode":201,"headers":{"Content-Type":"application/json"},"body":"hi"}"#,
        );
        let mapped = map_response(200, &raw).unwrap();
        assert_eq!(mapped.status, 201);
        assert_eq!(mapped.body, Bytes::from_static(b"hi"));
        assert_eq!(
            mapped.headers.get("content-type"),
            Some(&vec!["application/json".to_string()])
        );
    }

    #[test]
    fn test_map_response_falls_back_to_transport_status() {
        let raw = Bytes::from_static(br#"{"greeting":"hello"}"#);
        let mapped = map_response(200, &raw).unwrap();
        assert_eq!(mapped.status, 200);
        // no `body` field -> raw envelope passed through
        assert_eq!(mapped.body, raw);
    }

    #[test]
    fn test_map_response_empty_body_passthrough() {
        let mapped = map_response(204, &Bytes::new()).unwrap();
        assert_eq!(mapped.status, 204);
        assert!(mapped.body.is_empty());
    }

    #[test]
    fn test_map_response_invalid_json_errors() {
        assert!(map_response(200, &Bytes::from_static(b"not json")).is_err());
    }

    #[tokio::test]
    async fn test_callout_failure_routes_error() {
        let mut cfg = base_config();
        cfg["api_host"] = serde_json::json!("http://127.0.0.1:1");
        cfg["timeout"] = serde_json::json!(200);
        let p = plugin(cfg);
        let err = p.execute(ctx(), &HashMap::new()).await.unwrap_err();
        assert_eq!(err.error.code, "OPENWHISK_CALLOUT_ERROR");
        assert!(err.context.response.status_code >= 502);
    }
}
