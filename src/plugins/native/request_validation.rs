//! The `request-validation` node — validates request headers and/or body
//! against JSON Schemas before the request reaches the upstream, rejecting
//! non-conforming requests with a configurable status code.
//!
//! Port of APISIX's `request-validation` plugin. Schemas are compiled once at
//! config load (invalid schemas fail policy compilation, not requests).
//! Bodies are validated as JSON by default; `application/x-www-form-urlencoded`
//! bodies are decoded into a flat object first, mirroring the Lua plugin's
//! `ngx.decode_args` shape (duplicate keys become arrays, keys without `=`
//! become `true`).

use async_trait::async_trait;
use bytes::Bytes;
use std::collections::HashMap;

use crate::context::{Context, GatewayError};
use crate::plugins::{Plugin, PluginExecutionError, PluginOutput, PluginResult};

/// Validates `context.request` headers and body against compiled JSON
/// Schemas. On failure the request is rejected through the `error` port with
/// error code `VALIDATION_FAILED` and a JSON response using `rejected_code`.
///
/// Headers are validated as a single-value object (first value per header,
/// names lowercased), matching the shape APISIX passes to its schema check.
/// After a successful JSON-body validation the body is re-serialized from the
/// parsed document, so the JSON that was validated is exactly the JSON the
/// upstream receives (guards against JSON-interoperability smuggling).
pub struct RequestValidationPlugin {
    header_schema: Option<jsonschema::Validator>,
    body_schema: Option<jsonschema::Validator>,
    rejected_code: u16,
    rejected_msg: Option<String>,
}

/// Compiles an optional schema key into a validator, failing fast with the
/// key name on malformed schemas.
fn compile_schema(
    config: &HashMap<String, serde_json::Value>,
    key: &str,
) -> Result<Option<jsonschema::Validator>, String> {
    match config.get(key) {
        None => Ok(None),
        Some(raw) => {
            if !raw.is_object() {
                return Err(format!(
                    "request-validation: '{}' must be a JSON Schema object",
                    key
                ));
            }
            jsonschema::validator_for(raw)
                .map(Some)
                .map_err(|e| format!("request-validation: invalid '{}': {}", key, e))
        }
    }
}

/// Decodes an `application/x-www-form-urlencoded` body into a flat JSON
/// object, mirroring `ngx.decode_args`: `a=1&a=2` becomes `{"a": ["1","2"]}`,
/// a bare `flag` (no `=`) becomes `{"flag": true}`, values are
/// percent-decoded and `+` maps to space.
fn decode_urlencoded(body: &str) -> serde_json::Value {
    let mut map = serde_json::Map::new();
    for pair in body.split('&') {
        if pair.is_empty() {
            continue;
        }
        let (key, value) = match pair.split_once('=') {
            Some((k, v)) => (urldecode(k), serde_json::Value::String(urldecode(v))),
            None => (urldecode(pair), serde_json::Value::Bool(true)),
        };
        match map.get_mut(&key) {
            None => {
                map.insert(key, value);
            }
            Some(serde_json::Value::Array(arr)) => arr.push(value),
            Some(existing) => {
                let first = existing.take();
                *existing = serde_json::Value::Array(vec![first, value]);
            }
        }
    }
    serde_json::Value::Object(map)
}

/// Percent-decodes a urlencoded component (`+` becomes space; malformed
/// escapes pass through literally).
fn urldecode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < bytes.len() => {
                if let (Some(h), Some(l)) = (hex_val(bytes[i + 1]), hex_val(bytes[i + 2])) {
                    out.push(h * 16 + l);
                    i += 3;
                    continue;
                }
                out.push(b'%');
            }
            b => out.push(b),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

impl RequestValidationPlugin {
    /// Builds the plugin from node config.
    ///
    /// Accepted keys:
    /// - `header_schema` (object): JSON Schema applied to the request headers,
    ///   seen as `{name: first_value}` with lowercase names.
    /// - `body_schema` (object): JSON Schema applied to the parsed request
    ///   body (JSON, or urlencoded decoded to a flat object).
    /// - `rejected_code` (integer 200–599, default `400`): response status
    ///   for rejected requests.
    /// - `rejected_msg` (string): fixed message returned instead of the
    ///   validator's error description.
    ///
    /// At least one of `header_schema` / `body_schema` is required; both are
    /// compiled here so malformed schemas fail at config load.
    ///
    /// ```yaml
    /// type: request-validation
    /// config:
    ///   rejected_code: 422
    ///   body_schema:
    ///     type: object
    ///     required: [name]
    ///     properties:
    ///       name: { type: string, minLength: 1 }
    /// ```
    pub fn from_config(config: &HashMap<String, serde_json::Value>) -> Result<Self, String> {
        let header_schema = compile_schema(config, "header_schema")?;
        let body_schema = compile_schema(config, "body_schema")?;

        if header_schema.is_none() && body_schema.is_none() {
            return Err(
                "request-validation: at least one of 'header_schema' or 'body_schema' is required"
                    .to_string(),
            );
        }

        let rejected_code = match config.get("rejected_code") {
            None => 400,
            Some(v) => {
                let code = v
                    .as_u64()
                    .filter(|c| (200..=599).contains(c))
                    .ok_or("request-validation: 'rejected_code' must be an integer in 200..=599")?;
                code as u16
            }
        };

        let rejected_msg = config
            .get("rejected_msg")
            .and_then(|v| v.as_str())
            .map(String::from);

        Ok(Self {
            header_schema,
            body_schema,
            rejected_code,
            rejected_msg,
        })
    }

    /// Writes the rejection onto the response and returns the error that
    /// routes the context through the node's `error` port.
    fn reject(&self, mut ctx: Context, detail: String) -> PluginExecutionError {
        let message = self.rejected_msg.clone().unwrap_or(detail);
        ctx.response.status_code = self.rejected_code;
        ctx.response.body = Bytes::from(
            serde_json::json!({ "error": "validation_failed", "message": message }).to_string(),
        );
        ctx.response.headers.insert(
            "content-type".to_string(),
            vec!["application/json".to_string()],
        );
        PluginExecutionError {
            context: ctx,
            error: GatewayError {
                node_id: String::new(),
                code: "VALIDATION_FAILED".to_string(),
                message,
                metadata: HashMap::new(),
            },
        }
    }
}

#[async_trait]
impl Plugin for RequestValidationPlugin {
    fn plugin_type(&self) -> &str {
        "request-validation"
    }

    async fn execute(
        &self,
        mut ctx: Context,
        _named_inputs: &HashMap<String, serde_json::Value>,
    ) -> PluginResult {
        if let Some(validator) = &self.header_schema {
            let headers: serde_json::Map<String, serde_json::Value> = ctx
                .request
                .headers
                .iter()
                .filter_map(|(k, v)| {
                    v.first()
                        .map(|first| (k.clone(), serde_json::Value::String(first.clone())))
                })
                .collect();
            if let Err(e) = validator.validate(&serde_json::Value::Object(headers)) {
                return Err(self.reject(ctx, format!("header validation failed: {}", e)));
            }
        }

        if let Some(validator) = &self.body_schema {
            if ctx.request.body.is_empty() {
                return Err(self.reject(ctx, "request body is required".to_string()));
            }

            let is_urlencoded = ctx
                .request
                .headers
                .get("content-type")
                .and_then(|v| v.first())
                .map(|ct| {
                    ct.to_lowercase()
                        .starts_with("application/x-www-form-urlencoded")
                })
                .unwrap_or(false);

            let (parsed, body_is_json) = if is_urlencoded {
                let text = String::from_utf8_lossy(&ctx.request.body).into_owned();
                (decode_urlencoded(&text), false)
            } else {
                match serde_json::from_slice::<serde_json::Value>(&ctx.request.body) {
                    Ok(v) => (v, true),
                    Err(e) => {
                        return Err(
                            self.reject(ctx, format!("failed to decode the request body: {}", e))
                        );
                    }
                }
            };

            if let Err(e) = validator.validate(&parsed) {
                return Err(self.reject(ctx, format!("body validation failed: {}", e)));
            }

            if body_is_json {
                // Ensure the JSON we validated is the JSON the upstream sees
                // (JSON interoperability hardening, mirroring APISIX).
                // Body-mutation convention: drop the stale content-length.
                ctx.request.body = Bytes::from(serde_json::to_vec(&parsed).unwrap_or_default());
                ctx.request.headers.remove("content-length");
            }
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

    fn test_context(body: &str, content_type: Option<&str>) -> Context {
        let mut headers = HashMap::new();
        headers.insert("x-api-version".to_string(), vec!["2".to_string()]);
        if let Some(ct) = content_type {
            headers.insert("content-type".to_string(), vec![ct.to_string()]);
        }
        if !body.is_empty() {
            headers.insert("content-length".to_string(), vec![body.len().to_string()]);
        }

        Context {
            request: GatewayRequest {
                method: "POST".to_string(),
                path: "/api".to_string(),
                host: "localhost".to_string(),
                scheme: "http".to_string(),
                headers,
                query_params: HashMap::new(),
                body: Bytes::from(body.to_string()),
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

    fn body_plugin(schema: serde_json::Value) -> RequestValidationPlugin {
        let mut config = HashMap::new();
        config.insert("body_schema".to_string(), schema);
        RequestValidationPlugin::from_config(&config).unwrap()
    }

    #[tokio::test]
    async fn test_request_validation_body_accept_and_normalize() {
        let p = body_plugin(serde_json::json!({
            "type": "object",
            "required": ["name"],
            "properties": { "name": { "type": "string" } }
        }));
        let ctx = test_context(r#"{"name":  "jack"}"#, Some("application/json"));
        let out = p.execute(ctx, &HashMap::new()).await.unwrap();
        // validated JSON is re-serialized (normalized) and content-length dropped
        assert_eq!(out.context.request.body, Bytes::from(r#"{"name":"jack"}"#));
        assert!(!out.context.request.headers.contains_key("content-length"));
    }

    #[tokio::test]
    async fn test_request_validation_body_reject() {
        let p = body_plugin(serde_json::json!({
            "type": "object",
            "required": ["name"]
        }));
        let ctx = test_context(r#"{"age": 3}"#, Some("application/json"));
        let err = p.execute(ctx, &HashMap::new()).await.unwrap_err();
        assert_eq!(err.error.code, "VALIDATION_FAILED");
        assert_eq!(err.context.response.status_code, 400);
        let body: serde_json::Value = serde_json::from_slice(&err.context.response.body).unwrap();
        assert_eq!(body["error"], "validation_failed");
    }

    #[tokio::test]
    async fn test_request_validation_non_json_body_rejected() {
        let p = body_plugin(serde_json::json!({ "type": "object" }));
        let ctx = test_context("this is not json", Some("application/json"));
        let err = p.execute(ctx, &HashMap::new()).await.unwrap_err();
        assert_eq!(err.error.code, "VALIDATION_FAILED");
    }

    #[tokio::test]
    async fn test_request_validation_missing_body_rejected() {
        let p = body_plugin(serde_json::json!({ "type": "object" }));
        let err = p
            .execute(test_context("", Some("application/json")), &HashMap::new())
            .await
            .unwrap_err();
        assert_eq!(err.error.code, "VALIDATION_FAILED");
    }

    #[tokio::test]
    async fn test_request_validation_urlencoded_body() {
        let p = body_plugin(serde_json::json!({
            "type": "object",
            "required": ["user"],
            "properties": { "user": { "type": "string", "minLength": 2 } }
        }));
        let ctx = test_context(
            "user=jack&note=hello%20world",
            Some("application/x-www-form-urlencoded"),
        );
        let out = p.execute(ctx, &HashMap::new()).await.unwrap();
        // urlencoded bodies are not rewritten
        assert_eq!(
            out.context.request.body,
            Bytes::from("user=jack&note=hello%20world")
        );

        let ctx = test_context("note=only", Some("application/x-www-form-urlencoded"));
        assert!(p.execute(ctx, &HashMap::new()).await.is_err());
    }

    #[tokio::test]
    async fn test_request_validation_header_schema() {
        let mut config = HashMap::new();
        config.insert(
            "header_schema".to_string(),
            serde_json::json!({
                "type": "object",
                "required": ["x-api-version"],
                "properties": { "x-api-version": { "type": "string", "enum": ["2"] } }
            }),
        );
        let p = RequestValidationPlugin::from_config(&config).unwrap();

        assert!(p
            .execute(test_context("", None), &HashMap::new())
            .await
            .is_ok());

        let mut ctx = test_context("", None);
        ctx.request.headers.remove("x-api-version");
        let err = p.execute(ctx, &HashMap::new()).await.unwrap_err();
        assert_eq!(err.error.code, "VALIDATION_FAILED");
    }

    #[tokio::test]
    async fn test_request_validation_rejected_code_and_msg() {
        let mut config = HashMap::new();
        config.insert(
            "body_schema".to_string(),
            serde_json::json!({ "type": "object" }),
        );
        config.insert("rejected_code".to_string(), serde_json::json!(422));
        config.insert("rejected_msg".to_string(), serde_json::json!("bad payload"));
        let p = RequestValidationPlugin::from_config(&config).unwrap();

        let ctx = test_context("[1,2,3]", Some("application/json"));
        let err = p.execute(ctx, &HashMap::new()).await.unwrap_err();
        assert_eq!(err.context.response.status_code, 422);
        assert_eq!(err.error.message, "bad payload");
    }

    #[test]
    fn test_request_validation_config_rejections() {
        // no schema at all
        assert!(RequestValidationPlugin::from_config(&HashMap::new()).is_err());

        // invalid JSON Schema fails fast at config load
        let mut config = HashMap::new();
        config.insert(
            "body_schema".to_string(),
            serde_json::json!({ "type": "definitely-not-a-type" }),
        );
        assert!(RequestValidationPlugin::from_config(&config).is_err());

        // schema must be an object
        let mut config = HashMap::new();
        config.insert("body_schema".to_string(), serde_json::json!("nope"));
        assert!(RequestValidationPlugin::from_config(&config).is_err());

        // rejected_code out of range
        let mut config = HashMap::new();
        config.insert(
            "body_schema".to_string(),
            serde_json::json!({ "type": "object" }),
        );
        config.insert("rejected_code".to_string(), serde_json::json!(199));
        assert!(RequestValidationPlugin::from_config(&config).is_err());
    }

    #[test]
    fn test_request_validation_decode_urlencoded_shape() {
        let v = decode_urlencoded("a=1&a=2&flag&note=hello+world");
        assert_eq!(v["a"], serde_json::json!(["1", "2"]));
        assert_eq!(v["flag"], serde_json::json!(true));
        assert_eq!(v["note"], serde_json::json!("hello world"));
    }
}
