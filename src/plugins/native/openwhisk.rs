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
//! `api_host`, `namespace`, `package`, and `action` support
//! `{{namespace.path}}` references (plain render, no legacy `$var`
//! interpolation) — the endpoint is rebuilt per request so a value like
//! `{{request.headers.x-tenant}}` in `namespace` routes each request to a
//! different OpenWhisk namespace. `authorization` (built from
//! `service_token`) is never templated — it is a credential, not an
//! endpoint field. `namespace`/`package`/`action` are percent-encoded at
//! render time before insertion into the URL path (see
//! [`encode_path_segment`]); `api_host` is not encoded (see its field doc).
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
use crate::vars::template::Template;

use super::faas;

/// Invokes an OpenWhisk action and maps its reply into `Context.response`.
pub struct OpenWhiskPlugin {
    /// Supports `{{namespace.path}}` references — resolved per request when
    /// building the endpoint URL. No legacy `$var` interpolation (endpoint
    /// fields never supported it, so this sweep must not start).
    ///
    /// **Warning**: unlike `namespace`/`package`/`action`, the rendered value
    /// is spliced into the URL unencoded (it's the origin + scheme, not a
    /// path segment — percent-encoding would corrupt it). Templating this
    /// field from request-controlled data lets a client redirect the entire
    /// outbound call to an arbitrary host (SSRF); prefer a static value or
    /// one derived only from `{{env.*}}`.
    api_host: Template,
    /// Pre-computed `Basic <base64(service_token)>` header value. Never
    /// templated — a credential, not an endpoint field.
    authorization: String,
    /// Supports `{{namespace.path}}` references, same as `api_host`. Unlike
    /// `api_host`, the rendered value is percent-encoded
    /// ([`encode_path_segment`]) before insertion into the URL path — safe
    /// to template from request data.
    namespace: Template,
    /// Supports `{{namespace.path}}` references, same as `namespace`
    /// (percent-encoded at render time).
    package: Option<Template>,
    /// Supports `{{namespace.path}}` references, same as `namespace`
    /// (percent-encoded at render time).
    action: Template,
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
    ///   Supports `{{namespace.path}}` references, rendered per request when
    ///   building the endpoint URL (a trailing `/` is trimmed after render).
    /// - `service_token` (string, **required**): `user:pass` action token, sent
    ///   base64-encoded as HTTP Basic auth. Missing/empty is a config error.
    ///   Never templated — a credential, not an endpoint field.
    /// - `action` (string, **required**): the action name to invoke; supports
    ///   `{{namespace.path}}` references.
    /// - `namespace` (string, default `_`): the OpenWhisk namespace; supports
    ///   `{{namespace.path}}` references.
    /// - `package` (string, optional): the package the action belongs to;
    ///   supports `{{namespace.path}}` references.
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
            .to_string();
        // Discard warnings here — the compile-time walk (a later task)
        // reports well-formed-but-unknown references; execution must not.
        // The trailing-slash trim moves to `endpoint()` (post-render) since a
        // templated value's trailing slash can't be known at load time.
        let api_host = Template::parse(&api_host).0;

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
        let action = Template::parse(&action).0;

        let namespace = config
            .get("namespace")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .unwrap_or("_")
            .to_string();
        let namespace = Template::parse(&namespace).0;

        let package = config
            .get("package")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(|s| Template::parse(s).0);

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

    /// Builds the OpenWhisk action-invocation URL, rendering `api_host` /
    /// `namespace` / `package` / `action` against `ctx` so each field's
    /// `{{namespace.path}}` references resolve per request.
    ///
    /// `namespace`/`package`/`action` are percent-encoded
    /// ([`encode_path_segment`]) after rendering: since these fields are
    /// templatable from request data (e.g. a header), an unescaped `/` or
    /// `?` in the rendered value would otherwise break out of its path
    /// segment or inject extra query parameters ahead of the plugin's own
    /// `blocking`/`result`/`timeout`. `api_host` is deliberately not
    /// encoded — see the warning on its field doc.
    fn endpoint(&self, ctx: &Context) -> String {
        let api_host = self.api_host.render(ctx);
        let api_host = api_host.trim_end_matches('/');
        let namespace = encode_path_segment(&self.namespace.render(ctx));
        let action = encode_path_segment(&self.action.render(ctx));
        let package = self
            .package
            .as_ref()
            .map(|p| format!("{}/", encode_path_segment(&p.render(ctx))))
            .unwrap_or_default();
        format!(
            "{}/api/v1/namespaces/{}/actions/{}{}?blocking=true&result={}&timeout={}",
            api_host, namespace, package, action, self.result, self.timeout_ms
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
            url: self.endpoint(ctx),
            headers,
            body: ctx.request.body.clone(),
            timeout: self.timeout,
            ssl_verify: self.ssl_verify,
            tls: None,
        }
    }
}

/// Percent-encodes `s` for safe insertion as a single URL path segment.
///
/// Byte-wise: any byte outside the RFC 3986 `unreserved` set (`ALPHA` /
/// `DIGIT` / `-` / `.` / `_` / `~`) becomes `%XX` (uppercase hex). This is
/// deliberately stricter than a full path-escaper (it also encodes `/`),
/// which is exactly the point here — `namespace`/`package`/`action` are each
/// meant to occupy exactly one path segment, and a rendered value from
/// request data (header, query, ...) must not be able to smuggle in a `/`
/// (segment breakout, e.g. `foo/actions/bar`) or a `?`/`&` (query-string
/// injection ahead of the plugin's own `blocking`/`result`/`timeout`
/// params). A literal, already-safe value (`guest`, `my_pkg`) round-trips
/// byte-identical since every one of its characters is in `unreserved`.
fn encode_path_segment(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{:02X}", b)),
        }
    }
    out
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
            p.endpoint(&ctx()),
            "https://ow.example.com/api/v1/namespaces/guest/actions/mypkg/hello?blocking=true&result=false&timeout=3000"
        );
    }

    #[test]
    fn test_endpoint_renders_namespace_template_per_request() {
        // CONTROLLER-APPROVED SCOPE ADDITION: `namespace` (and `api_host`,
        // `package`, `action`) render `{{namespace.path}}` references per
        // request, closing the FaaS-family templating gap found in the
        // Task 3 review.
        let mut cfg = base_config();
        cfg["namespace"] = serde_json::json!("tenant-{{request.headers.x-tenant}}");
        let p = plugin(cfg);

        let mut request_ctx = ctx();
        request_ctx
            .request
            .headers
            .insert("x-tenant".to_string(), vec!["acme".to_string()]);
        assert_eq!(
            p.endpoint(&request_ctx),
            "https://ow.example.com/api/v1/namespaces/tenant-acme/actions/hello?blocking=true&result=true&timeout=3000"
        );

        // A different request routes to a different namespace.
        let mut other_ctx = ctx();
        other_ctx
            .request
            .headers
            .insert("x-tenant".to_string(), vec!["globex".to_string()]);
        assert_eq!(
            p.endpoint(&other_ctx),
            "https://ow.example.com/api/v1/namespaces/tenant-globex/actions/hello?blocking=true&result=true&timeout=3000"
        );
    }

    #[test]
    fn test_endpoint_literal_namespace_is_byte_identical() {
        // A literal (non-templated) config must render byte-identically
        // across requests — no accidental per-request drift from the
        // templating change.
        let p = plugin(base_config());
        let expected =
            "https://ow.example.com/api/v1/namespaces/guest/actions/hello?blocking=true&result=true&timeout=3000";
        assert_eq!(p.endpoint(&ctx()), expected);

        let mut other_ctx = ctx();
        other_ctx.request.host = "totally-different-host".to_string();
        assert_eq!(p.endpoint(&other_ctx), expected);
    }

    #[test]
    fn test_encode_path_segment_passes_literal_values_byte_identical() {
        // Existing literal configs (already-safe unreserved characters) must
        // round-trip unchanged.
        assert_eq!(encode_path_segment("guest"), "guest");
        assert_eq!(encode_path_segment("my_pkg"), "my_pkg");
        assert_eq!(encode_path_segment("my-pkg.v2"), "my-pkg.v2");
        assert_eq!(encode_path_segment("_-~.Az09"), "_-~.Az09");
    }

    #[test]
    fn test_encode_path_segment_escapes_reserved_bytes() {
        assert_eq!(
            encode_path_segment("foo/actions/bar"),
            "foo%2Factions%2Fbar"
        );
        assert_eq!(encode_path_segment("hello?x=1"), "hello%3Fx%3D1");
        assert_eq!(encode_path_segment("a&b"), "a%26b");
        assert_eq!(encode_path_segment(" "), "%20");
    }

    #[test]
    fn test_endpoint_namespace_slash_cannot_break_out_of_path_segment() {
        // SECURITY: a templated `namespace` rendered from request data must
        // not be able to inject an extra path segment via an unescaped `/`.
        let mut cfg = base_config();
        cfg["namespace"] = serde_json::json!("{{request.headers.x-tenant}}");
        let p = plugin(cfg);

        let mut request_ctx = ctx();
        request_ctx
            .request
            .headers
            .insert("x-tenant".to_string(), vec!["foo/actions/bar".to_string()]);
        assert_eq!(
            p.endpoint(&request_ctx),
            "https://ow.example.com/api/v1/namespaces/foo%2Factions%2Fbar/actions/hello?blocking=true&result=true&timeout=3000"
        );
    }

    #[test]
    fn test_endpoint_action_question_mark_cannot_inject_query_params() {
        // SECURITY: a templated `action` rendered from request data must not
        // be able to inject extra query parameters ahead of the plugin's own
        // `blocking`/`result`/`timeout`.
        let mut cfg = base_config();
        cfg["action"] = serde_json::json!("{{request.headers.x-action}}");
        let p = plugin(cfg);

        let mut request_ctx = ctx();
        request_ctx
            .request
            .headers
            .insert("x-action".to_string(), vec!["hello?x=1".to_string()]);
        let endpoint = p.endpoint(&request_ctx);
        assert_eq!(
            endpoint,
            "https://ow.example.com/api/v1/namespaces/guest/actions/hello%3Fx%3D1?blocking=true&result=true&timeout=3000"
        );
        // Exactly one '?' in the whole URL — the plugin's own query string.
        assert_eq!(endpoint.matches('?').count(), 1);
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
