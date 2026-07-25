//! The `body-transformer` node — rewrites the request and/or response body
//! from a template.
//!
//! # Deviation from APISIX (read this first)
//!
//! APISIX's `body-transformer` renders full **lua-resty-template** templates
//! (arbitrary Lua expressions, loops, helpers) over bodies decoded from
//! `xml`, `json`, `encoded`, `args`, `plain`, or `multipart` input.
//! featherbit implements a deliberate subset:
//!
//! - `input_format` supports **`json` only** (other formats are rejected at
//!   config load; omitting it defaults to `json`).
//! - Templates are plain strings with two placeholder forms:
//!   - `{{body.x.y}}` — a dotted path resolved from the parsed JSON body
//!     (numeric segments index arrays; `{{body}}` is the whole document).
//!     Strings are inserted raw (unquoted), numbers/bools verbatim, missing
//!     values and `null` as the empty string, and objects/arrays as compact
//!     JSON.
//!   - `{{$var}}` — a context variable via [`crate::vars::resolve`]
//!     (`$uri`, `$http_x_id`, `$arg_page`, ...).
//!
//!   Text outside `{{ }}` additionally passes through
//!   [`crate::vars::interpolate`], so bare `$var` references work there too.
//! - `template_is_base64` is not supported (templates are stored literally
//!   in YAML) and is rejected at config load.

use async_trait::async_trait;
use bytes::Bytes;
use std::collections::HashMap;

use crate::context::{Context, GatewayError};
use crate::plugins::util::content_codec::{self, ContentEncoding};
use crate::plugins::{Plugin, PluginExecutionError, PluginOutput, PluginResult};

/// Rewrites `context.request.body` and/or `context.response.body` by
/// rendering a template against the parsed JSON body and context variables.
///
/// A node with a `request` transform must sit **before** the `upstream`
/// node; a node with a `response` transform must sit **after** it (the
/// response body is empty until the upstream runs). Configuring both on one
/// node only makes sense in single-node positions where both sides are
/// populated — normally use two nodes.
///
/// Failures (a non-empty body that is not valid JSON, or an undecodable
/// response encoding) exit through the `error` port with code
/// `BODY_DECODE_FAILED` — status 400 for request bodies, 502 for response
/// bodies. An empty body renders the template with all `{{body...}}`
/// placeholders empty.
pub struct BodyTransformerPlugin {
    request: Option<String>,
    response: Option<String>,
}

/// Validates one `request`/`response` transform object and returns its
/// template string.
fn parse_transform(
    config: &HashMap<String, serde_json::Value>,
    key: &str,
) -> Result<Option<String>, String> {
    let Some(raw) = config.get(key) else {
        return Ok(None);
    };
    let obj = raw
        .as_object()
        .ok_or_else(|| format!("body-transformer: '{}' must be an object", key))?;

    let template = obj
        .get("template")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| format!("body-transformer: '{}.template' is required", key))?;

    match obj.get("input_format").and_then(|v| v.as_str()) {
        None | Some("json") => {}
        Some(other) => {
            return Err(format!(
                "body-transformer: '{}.input_format' '{}' is not supported — featherbit implements 'json' only",
                key, other
            ));
        }
    }

    if obj.get("template_is_base64").and_then(|v| v.as_bool()) == Some(true) {
        return Err(format!(
            "body-transformer: '{}.template_is_base64' is not supported — store the template literally",
            key
        ));
    }

    // Fail fast on unbalanced placeholder braces.
    let mut rest = template;
    while let Some(open) = rest.find("{{") {
        match rest[open + 2..].find("}}") {
            Some(close) => rest = &rest[open + 2 + close + 2..],
            None => {
                return Err(format!(
                    "body-transformer: '{}.template' has an unclosed '{{{{' placeholder",
                    key
                ));
            }
        }
    }

    Ok(Some(template.to_string()))
}

/// Resolves a dotted path (`x.y.0`) inside a JSON value; an empty path
/// returns the value itself.
fn json_path<'a>(mut value: &'a serde_json::Value, path: &str) -> Option<&'a serde_json::Value> {
    if path.is_empty() {
        return Some(value);
    }
    for seg in path.split('.') {
        value = match seg.parse::<usize>() {
            Ok(idx) => value.get(idx)?,
            Err(_) => value.get(seg)?,
        };
    }
    Some(value)
}

/// Stringifies a resolved JSON value for template insertion: strings raw,
/// scalars verbatim, `null` empty, containers as compact JSON.
fn json_to_template_string(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Null => String::new(),
        serde_json::Value::Number(n) => n.to_string(),
        serde_json::Value::Bool(b) => b.to_string(),
        other => serde_json::to_string(other).unwrap_or_default(),
    }
}

/// Renders a template: `{{body.path}}` from the parsed body, `{{$var}}` via
/// [`crate::vars::resolve`], and `$var` interpolation on the literal text
/// between placeholders. Unknown placeholders resolve to the empty string;
/// values substituted from the body are **not** re-interpolated.
fn render(ctx: &Context, template: &str, body: &serde_json::Value) -> String {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;

    while let Some(open) = rest.find("{{") {
        // Literal text before the placeholder gets $var interpolation.
        out.push_str(&crate::vars::interpolate(ctx, &rest[..open]));
        let after = &rest[open + 2..];
        match after.find("}}") {
            Some(close) => {
                let expr = after[..close].trim();
                if let Some(var) = expr.strip_prefix('$') {
                    if let Some(v) = crate::vars::resolve(ctx, var) {
                        out.push_str(&v);
                    }
                } else if expr == "body" {
                    out.push_str(&json_to_template_string(body));
                } else if let Some(path) = expr.strip_prefix("body.") {
                    if let Some(v) = json_path(body, path) {
                        out.push_str(&json_to_template_string(v));
                    }
                }
                // any other placeholder -> empty string
                rest = &after[close + 2..];
            }
            None => {
                // Unclosed placeholder (rejected at config load; kept literal
                // here for robustness).
                out.push_str(&crate::vars::interpolate(
                    ctx,
                    rest[open..].to_string().as_str(),
                ));
                rest = "";
                break;
            }
        }
    }
    out.push_str(&crate::vars::interpolate(ctx, rest));
    out
}

/// Parses a body as JSON; empty bodies become `null` so templates still
/// render (with empty `{{body...}}` placeholders).
fn parse_body(body: &Bytes) -> Result<serde_json::Value, String> {
    if body.is_empty() {
        return Ok(serde_json::Value::Null);
    }
    serde_json::from_slice(body).map_err(|e| format!("body is not valid JSON: {}", e))
}

impl BodyTransformerPlugin {
    /// Builds the plugin from node config.
    ///
    /// Accepted keys (at least one of `request` / `response` is required):
    /// - `request` (object): transform applied to the request body.
    ///   - `template` (string, required): the output body; see the module
    ///     docs for the `{{body.path}}` / `{{$var}}` placeholder subset.
    ///   - `input_format` (string, default `json`): only `json` is accepted.
    /// - `response` (object): same shape, applied to the response body.
    ///
    /// Rejected at config load: missing/empty templates, unbalanced `{{`,
    /// any `input_format` other than `json`, and `template_is_base64: true`
    /// (both documented deviations from APISIX).
    ///
    /// ```yaml
    /// type: body-transformer
    /// config:
    ///   request:
    ///     input_format: json
    ///     template: '{"name":"{{body.user.name}}","trace":"{{$http_x_request_id}}"}'
    /// ```
    pub fn from_config(config: &HashMap<String, serde_json::Value>) -> Result<Self, String> {
        let request = parse_transform(config, "request")?;
        let response = parse_transform(config, "response")?;
        if request.is_none() && response.is_none() {
            return Err(
                "body-transformer: at least one of 'request' or 'response' is required".to_string(),
            );
        }
        Ok(Self { request, response })
    }

    /// Builds the `error`-port rejection for an undecodable body.
    fn fail(&self, mut ctx: Context, status: u16, detail: String) -> PluginExecutionError {
        ctx.response.status_code = status;
        ctx.response.body = Bytes::from(
            serde_json::json!({ "error": "body_decode_failed", "message": detail }).to_string(),
        );
        ctx.response.headers.insert(
            "content-type".to_string(),
            vec!["application/json".to_string()],
        );
        PluginExecutionError {
            context: ctx,
            error: GatewayError {
                node_id: String::new(),
                code: "BODY_DECODE_FAILED".to_string(),
                message: detail,
                metadata: HashMap::new(),
            },
        }
    }
}

#[async_trait]
impl Plugin for BodyTransformerPlugin {
    fn plugin_type(&self) -> &str {
        "body-transformer"
    }

    async fn execute(
        &self,
        mut ctx: Context,
        _named_inputs: &HashMap<String, serde_json::Value>,
    ) -> PluginResult {
        if let Some(template) = &self.request {
            let parsed = match parse_body(&ctx.request.body) {
                Ok(v) => v,
                Err(e) => return Err(self.fail(ctx, 400, format!("request {}", e))),
            };
            let rendered = render(&ctx, template, &parsed);
            ctx.request.body = Bytes::from(rendered);
            // Body-mutation convention: stale framing headers must go.
            ctx.request.headers.remove("content-length");
            ctx.request.headers.remove("content-encoding");
        }

        if let Some(template) = &self.response {
            // Decode a compressed upstream body before parsing it.
            let encoding = match ctx
                .response
                .headers
                .get("content-encoding")
                .and_then(|v| v.first())
                .map(|v| ContentEncoding::parse(v))
                .transpose()
            {
                Ok(enc) => enc.flatten(),
                Err(e) => return Err(self.fail(ctx, 502, format!("response {}", e))),
            };
            let body = match encoding {
                Some(enc) => match content_codec::decode(&enc, &ctx.response.body) {
                    Ok(decoded) => decoded,
                    Err(e) => return Err(self.fail(ctx, 502, format!("response {}", e))),
                },
                None => ctx.response.body.clone(),
            };
            let parsed = match parse_body(&body) {
                Ok(v) => v,
                Err(e) => return Err(self.fail(ctx, 502, format!("response {}", e))),
            };
            let rendered = render(&ctx, template, &parsed);
            ctx.response.body = Bytes::from(rendered);
            // Body left decoded: remove content-encoding along with the
            // stale content-length (body-mutation convention).
            ctx.response.headers.remove("content-length");
            ctx.response.headers.remove("content-encoding");
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

    fn test_context(req_body: &str, resp_body: &str) -> Context {
        let mut headers = HashMap::new();
        headers.insert("x-request-id".to_string(), vec!["req-42".to_string()]);
        headers.insert(
            "content-length".to_string(),
            vec![req_body.len().to_string()],
        );
        let mut query = HashMap::new();
        query.insert("page".to_string(), vec!["3".to_string()]);
        let mut resp_headers = HashMap::new();
        resp_headers.insert(
            "content-length".to_string(),
            vec![resp_body.len().to_string()],
        );

        Context {
            request: GatewayRequest {
                method: "POST".to_string(),
                path: "/api/users".to_string(),
                host: "localhost".to_string(),
                scheme: "http".to_string(),
                headers,
                query_params: query,
                body: Bytes::from(req_body.to_string()),
                remote_addr: "127.0.0.1:12345".to_string(),
                protocol: Protocol::Http1,
            },
            response: GatewayResponse {
                status_code: 200,
                headers: resp_headers,
                body: Bytes::from(resp_body.to_string()),
            },
            message: HashMap::new(),
            errors: Vec::new(),
        }
    }

    fn request_plugin(template: &str) -> BodyTransformerPlugin {
        let mut config = HashMap::new();
        config.insert(
            "request".to_string(),
            serde_json::json!({ "template": template }),
        );
        BodyTransformerPlugin::from_config(&config).unwrap()
    }

    #[tokio::test]
    async fn test_body_transformer_request_template() {
        let p = request_plugin(
            r#"{"name":"{{body.user.name}}","first_tag":"{{body.user.tags.0}}","count":{{body.count}},"uri":"$uri","trace":"{{$http_x_request_id}}"}"#,
        );
        let ctx = test_context(r#"{"user":{"name":"jack","tags":["a","b"]},"count":7}"#, "");
        let out = p.execute(ctx, &HashMap::new()).await.unwrap();
        let parsed: serde_json::Value = serde_json::from_slice(&out.context.request.body).unwrap();
        assert_eq!(parsed["name"], "jack");
        assert_eq!(parsed["first_tag"], "a");
        assert_eq!(parsed["count"], 7);
        assert_eq!(parsed["uri"], "/api/users");
        assert_eq!(parsed["trace"], "req-42");
        assert!(!out.context.request.headers.contains_key("content-length"));
    }

    #[tokio::test]
    async fn test_body_transformer_missing_fields_render_empty() {
        let p = request_plugin("[{{body.missing.deep}}][{{body.n}}][{{$arg_nope}}]");
        let ctx = test_context(r#"{"n":null}"#, "");
        let out = p.execute(ctx, &HashMap::new()).await.unwrap();
        assert_eq!(out.context.request.body, Bytes::from("[][][]"));
    }

    #[tokio::test]
    async fn test_body_transformer_whole_body_and_containers() {
        let p = request_plugin(r#"{"wrapped":{{body}},"list":{{body.list}}}"#);
        let ctx = test_context(r#"{"list":[1,2]}"#, "");
        let out = p.execute(ctx, &HashMap::new()).await.unwrap();
        let parsed: serde_json::Value = serde_json::from_slice(&out.context.request.body).unwrap();
        assert_eq!(parsed["wrapped"], serde_json::json!({"list": [1, 2]}));
        assert_eq!(parsed["list"], serde_json::json!([1, 2]));
    }

    #[tokio::test]
    async fn test_body_transformer_empty_body_renders() {
        let p = request_plugin(r#"{"q":"$arg_page","body_was":"{{body}}"}"#);
        let ctx = test_context("", "");
        let out = p.execute(ctx, &HashMap::new()).await.unwrap();
        let parsed: serde_json::Value = serde_json::from_slice(&out.context.request.body).unwrap();
        assert_eq!(parsed["q"], "3");
        assert_eq!(parsed["body_was"], "");
    }

    #[tokio::test]
    async fn test_body_transformer_invalid_request_body_errors() {
        let p = request_plugin("{{body.a}}");
        let ctx = test_context("not json", "");
        let err = p.execute(ctx, &HashMap::new()).await.unwrap_err();
        assert_eq!(err.error.code, "BODY_DECODE_FAILED");
        assert_eq!(err.context.response.status_code, 400);
    }

    #[tokio::test]
    async fn test_body_transformer_response_template() {
        let mut config = HashMap::new();
        config.insert(
            "response".to_string(),
            serde_json::json!({ "template": r#"{"status":"{{body.result}}","code":$status}"# }),
        );
        let p = BodyTransformerPlugin::from_config(&config).unwrap();
        let ctx = test_context("", r#"{"result":"ok"}"#);
        let out = p.execute(ctx, &HashMap::new()).await.unwrap();
        let parsed: serde_json::Value = serde_json::from_slice(&out.context.response.body).unwrap();
        assert_eq!(parsed["status"], "ok");
        assert_eq!(parsed["code"], 200);
        assert!(!out.context.response.headers.contains_key("content-length"));
    }

    #[tokio::test]
    async fn test_body_transformer_response_gzip_decoded() {
        let mut config = HashMap::new();
        config.insert(
            "response".to_string(),
            serde_json::json!({ "template": "value={{body.v}}" }),
        );
        let p = BodyTransformerPlugin::from_config(&config).unwrap();
        let mut ctx = test_context("", "");
        ctx.response.body =
            content_codec::encode(&ContentEncoding::Gzip, &Bytes::from(r#"{"v":9}"#), 6).unwrap();
        ctx.response
            .headers
            .insert("content-encoding".to_string(), vec!["gzip".to_string()]);
        let out = p.execute(ctx, &HashMap::new()).await.unwrap();
        assert_eq!(out.context.response.body, Bytes::from("value=9"));
        // decoded body left plain -> encoding header removed
        assert!(!out
            .context
            .response
            .headers
            .contains_key("content-encoding"));
    }

    #[test]
    fn test_body_transformer_config_rejections() {
        let bad = [
            // neither request nor response
            serde_json::json!({}),
            // missing template
            serde_json::json!({ "request": {} }),
            // unsupported input_format (documented deviation)
            serde_json::json!({ "request": { "template": "x", "input_format": "xml" } }),
            // base64 templates unsupported (documented deviation)
            serde_json::json!({ "request": { "template": "eA==", "template_is_base64": true } }),
            // unclosed placeholder
            serde_json::json!({ "request": { "template": "{{body.a" } }),
            // transform must be an object
            serde_json::json!({ "request": "template" }),
        ];
        for case in bad {
            let config: HashMap<String, serde_json::Value> =
                serde_json::from_value(case.clone()).unwrap();
            assert!(
                BodyTransformerPlugin::from_config(&config).is_err(),
                "should reject: {case}"
            );
        }
    }
}
