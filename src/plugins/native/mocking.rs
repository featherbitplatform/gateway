//! Mocking plugin (`mocking`) — a faithful subset of Apache APISIX's
//! `mocking` plugin: responds with a configured mock instead of proxying
//! upstream. Useful for stubbing APIs during development and testing.
//!
//! **Terminal wiring**: a mocking node always answers the request itself, so
//! it returns `Ok` with the mock written onto `Context.response` — wire its
//! **success** port straight to `client.in` (never to an `upstream`). The
//! `error` port is never taken.
//!
//! Deviation from APISIX: `response_schema` (random body generation from a
//! JSON schema) is not implemented — configs that set it are rejected at
//! load, so `response_example` is required.

use async_trait::async_trait;
use bytes::Bytes;
use std::collections::HashMap;
use std::time::Duration;

use crate::context::Context;
use crate::plugins::{Plugin, PluginOutput, PluginResult};
use crate::vars::interpolate;

/// Content types the plugin accepts (matched on the part before `;`),
/// mirroring APISIX's `support_content_type`.
const SUPPORTED_CONTENT_TYPES: [&str; 5] = [
    "application/xml",
    "application/json",
    "text/plain",
    "text/html",
    "text/xml",
];

/// Builds a mock response: optional delay, then status + headers + body.
/// The body and string header values support `$var` interpolation.
pub struct MockingPlugin {
    delay: Duration,
    response_status: u16,
    content_type: String,
    /// Body template (`$var` interpolated per request).
    response_example: String,
    /// Lowercased header name → value template.
    response_headers: Vec<(String, String)>,
    with_mock_header: bool,
}

impl MockingPlugin {
    /// Builds the plugin from node config.
    ///
    /// Accepted keys:
    /// - `response_example` (string, **required**): response body; supports
    ///   `$var` interpolation (e.g. `$uri`, `$arg_name`).
    /// - `response_status` (integer >= 100, default `200`).
    /// - `content_type` (string, default `application/json;charset=utf8`):
    ///   base type must be one of `application/json`, `application/xml`,
    ///   `text/plain`, `text/html`, `text/xml`.
    /// - `response_headers` (map `{name: value}` or array `[{name, value}]`):
    ///   extra response headers; string values support `$var` interpolation.
    /// - `with_mock_header` (bool, default `true`): adds
    ///   `x-mock-by: featherbit-mocking`.
    /// - `delay` (number, seconds, may be fractional, default `0`): sleep
    ///   before responding.
    /// - `response_schema`: **not implemented** — rejected at config load
    ///   (deviation from APISIX).
    ///
    /// ```yaml
    /// type: mocking
    /// config:
    ///   response_status: 200
    ///   content_type: application/json;charset=utf8
    ///   response_example: '{"user": "$arg_name", "path": "$uri"}'
    ///   response_headers:
    ///     x-mock-env: staging
    ///   delay: 0.2
    /// ```
    pub fn from_config(config: &HashMap<String, serde_json::Value>) -> Result<Self, String> {
        if config.contains_key("response_schema") {
            return Err(
                "mocking: 'response_schema' (random body generation) is not implemented — \
                 use 'response_example' instead"
                    .to_string(),
            );
        }

        let response_example = config
            .get("response_example")
            .and_then(|v| v.as_str())
            .ok_or("mocking requires 'response_example' (string body)")?
            .to_string();

        let response_status = config
            .get("response_status")
            .map(|v| {
                v.as_u64()
                    .filter(|s| (100..=599).contains(s))
                    .ok_or("response_status must be an integer 100-599")
            })
            .transpose()?
            .unwrap_or(200) as u16;

        let content_type = config
            .get("content_type")
            .map(|v| {
                v.as_str()
                    .map(String::from)
                    .ok_or("content_type must be a string")
            })
            .transpose()?
            .unwrap_or_else(|| "application/json;charset=utf8".to_string());
        let base_type = content_type.split(';').next().unwrap_or("").trim();
        if !SUPPORTED_CONTENT_TYPES.contains(&base_type) {
            return Err(format!(
                "unsupported content type '{}' — supported: {}",
                content_type,
                SUPPORTED_CONTENT_TYPES.join(", ")
            ));
        }

        let response_headers = match config.get("response_headers") {
            None => Vec::new(),
            Some(v) => parse_headers(v)?,
        };

        let with_mock_header = config
            .get("with_mock_header")
            .map(|v| v.as_bool().ok_or("with_mock_header must be a boolean"))
            .transpose()?
            .unwrap_or(true);

        let delay = config
            .get("delay")
            .map(|v| {
                v.as_f64()
                    .filter(|d| *d >= 0.0 && d.is_finite())
                    .ok_or("delay must be a non-negative number of seconds")
            })
            .transpose()?
            .unwrap_or(0.0);

        Ok(Self {
            delay: Duration::from_secs_f64(delay),
            response_status,
            content_type,
            response_example,
            response_headers,
            with_mock_header,
        })
    }
}

/// Accepts `response_headers` as a map (`{name: value}`) or as the UI
/// editor's array form (`[{name, value}]`); scalar values are stringified,
/// other shapes are rejected. Header names are lowercased.
fn parse_headers(v: &serde_json::Value) -> Result<Vec<(String, String)>, String> {
    let scalar = |v: &serde_json::Value| -> Option<String> {
        match v {
            serde_json::Value::String(s) => Some(s.clone()),
            serde_json::Value::Number(n) => Some(n.to_string()),
            serde_json::Value::Bool(b) => Some(b.to_string()),
            _ => None,
        }
    };
    match v {
        serde_json::Value::Object(m) => m
            .iter()
            .map(|(k, v)| {
                scalar(v)
                    .map(|s| (k.to_lowercase(), s))
                    .ok_or_else(|| format!("response_headers['{k}'] must be a scalar value"))
            })
            .collect(),
        serde_json::Value::Array(items) => {
            let mut out = Vec::new();
            for item in items {
                let obj = item
                    .as_object()
                    .ok_or("response_headers entries must be objects with 'name' and 'value'")?;
                let name = obj.get("name").and_then(|v| v.as_str()).unwrap_or("");
                if name.trim().is_empty() {
                    continue; // blank row left in the UI editor
                }
                let value = match obj.get("value") {
                    None => String::new(),
                    Some(v) => scalar(v).ok_or_else(|| {
                        format!("response_headers['{name}'] must be a scalar value")
                    })?,
                };
                out.push((name.to_lowercase(), value));
            }
            Ok(out)
        }
        _ => Err(
            "response_headers must be a map of name: value or a list of {name, value} objects"
                .to_string(),
        ),
    }
}

#[async_trait]
impl Plugin for MockingPlugin {
    fn plugin_type(&self) -> &str {
        "mocking"
    }

    async fn execute(
        &self,
        mut ctx: Context,
        _named_inputs: &HashMap<String, serde_json::Value>,
    ) -> PluginResult {
        if !self.delay.is_zero() {
            tokio::time::sleep(self.delay).await;
        }

        let body = interpolate(&ctx, &self.response_example);
        let headers: Vec<(String, String)> = self
            .response_headers
            .iter()
            .map(|(name, tmpl)| (name.clone(), interpolate(&ctx, tmpl)))
            .collect();

        ctx.response.status_code = self.response_status;
        ctx.response.body = Bytes::from(body);
        ctx.response
            .headers
            .insert("content-type".to_string(), vec![self.content_type.clone()]);
        if self.with_mock_header {
            ctx.response.headers.insert(
                "x-mock-by".to_string(),
                vec!["featherbit-mocking".to_string()],
            );
        }
        for (name, value) in headers {
            ctx.response.headers.insert(name, vec![value]);
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
    use std::time::Instant;

    fn test_ctx() -> Context {
        let mut query = HashMap::new();
        query.insert("name".to_string(), vec!["jack".to_string()]);
        Context {
            request: GatewayRequest {
                method: "GET".to_string(),
                path: "/api/users".to_string(),
                host: "example.com".to_string(),
                scheme: "http".to_string(),
                headers: HashMap::new(),
                query_params: query,
                body: Bytes::new(),
                remote_addr: "10.1.2.3:44321".to_string(),
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

    fn plugin(config: serde_json::Value) -> Result<MockingPlugin, String> {
        let map: HashMap<String, serde_json::Value> = serde_json::from_value(config).unwrap();
        MockingPlugin::from_config(&map)
    }

    #[tokio::test]
    async fn test_mock_response_shape() {
        let p = plugin(serde_json::json!({
            "response_status": 201,
            "response_example": r#"{"user": "$arg_name", "path": "$uri"}"#,
            "response_headers": { "X-Mock-Env": "staging" }
        }))
        .unwrap();

        let out = p.execute(test_ctx(), &HashMap::new()).await.unwrap();
        let resp = out.context.response;
        assert_eq!(resp.status_code, 201);
        assert_eq!(
            resp.body,
            Bytes::from(r#"{"user": "jack", "path": "/api/users"}"#)
        );
        assert_eq!(
            resp.headers.get("content-type"),
            Some(&vec!["application/json;charset=utf8".to_string()])
        );
        assert_eq!(
            resp.headers.get("x-mock-by"),
            Some(&vec!["featherbit-mocking".to_string()])
        );
        assert_eq!(
            resp.headers.get("x-mock-env"),
            Some(&vec!["staging".to_string()])
        );
    }

    #[tokio::test]
    async fn test_defaults_and_mock_header_disabled() {
        let p = plugin(serde_json::json!({
            "response_example": "hello",
            "content_type": "text/plain",
            "with_mock_header": false
        }))
        .unwrap();
        let out = p.execute(test_ctx(), &HashMap::new()).await.unwrap();
        let resp = out.context.response;
        assert_eq!(resp.status_code, 200);
        assert_eq!(resp.body, Bytes::from("hello"));
        assert_eq!(
            resp.headers.get("content-type"),
            Some(&vec!["text/plain".to_string()])
        );
        assert!(!resp.headers.contains_key("x-mock-by"));
    }

    #[tokio::test]
    async fn test_delay_sleeps_before_responding() {
        let p = plugin(serde_json::json!({
            "response_example": "{}",
            "delay": 0.05
        }))
        .unwrap();
        let start = Instant::now();
        p.execute(test_ctx(), &HashMap::new()).await.unwrap();
        assert!(start.elapsed() >= Duration::from_millis(45));
    }

    #[test]
    fn test_config_errors() {
        // response_example is required.
        assert!(plugin(serde_json::json!({})).is_err());
        // response_schema is an explicit deviation: rejected at load.
        assert!(plugin(serde_json::json!({
            "response_schema": { "type": "object" }
        }))
        .is_err());
        assert!(plugin(serde_json::json!({
            "response_example": "{}",
            "response_schema": { "type": "object" }
        }))
        .is_err());
        // Unsupported content type.
        assert!(plugin(serde_json::json!({
            "response_example": "{}",
            "content_type": "application/octet-stream"
        }))
        .is_err());
        // Bad status.
        assert!(plugin(serde_json::json!({
            "response_example": "{}",
            "response_status": 42
        }))
        .is_err());
        // Negative delay.
        assert!(plugin(serde_json::json!({
            "response_example": "{}",
            "delay": -1
        }))
        .is_err());
    }
}
