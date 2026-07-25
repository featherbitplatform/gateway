//! The `proxy-rewrite` node — rewrites the request path and adds/removes
//! headers on either the request or the response, depending on `phase`.

use async_trait::async_trait;
use std::collections::HashMap;

use crate::context::Context;
use crate::plugins::{Plugin, PluginOutput, PluginResult};

/// Mutates either `Context.request` or `Context.response` (selected by
/// `phase`): strips/adds a path prefix and adds/removes headers. Path
/// rewriting only applies in the request phase; header names are lowercased
/// before being applied. This plugin never fails at execution time.
pub struct ProxyRewritePlugin {
    strip_path_prefix: Option<String>,
    add_path_prefix: Option<String>,
    add_headers: HashMap<String, String>,
    remove_headers: Vec<String>,
    phase: RewritePhase,
}

/// Which side of the exchange the rewrite applies to.
#[derive(Debug, Clone, PartialEq)]
enum RewritePhase {
    Request,
    Response,
}

/// Stringifies a scalar header value (string, number, or bool); returns `None`
/// for non-scalar shapes so callers can reject them at config load.
fn header_value_to_string(v: &serde_json::Value) -> Option<String> {
    match v {
        serde_json::Value::String(s) => Some(s.clone()),
        serde_json::Value::Number(n) => Some(n.to_string()),
        serde_json::Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

/// Accepts both config shapes for `add_headers`:
/// - map form (YAML):        `add_headers: { x-foo: bar }`
/// - array form (UI editor): `add_headers: [{ name: x-foo, value: bar }]`
///
/// Scalar values (numbers, bools) are stringified; array entries with a blank
/// `name` are skipped; any other shape is an error at config load.
fn parse_add_headers(
    config: &HashMap<String, serde_json::Value>,
) -> Result<HashMap<String, String>, String> {
    let Some(raw) = config.get("add_headers") else {
        return Ok(HashMap::new());
    };

    match raw {
        serde_json::Value::Object(m) => m
            .iter()
            .map(|(k, v)| {
                header_value_to_string(v)
                    .map(|s| (k.clone(), s))
                    .ok_or_else(|| format!("add_headers['{}'] must be a scalar value", k))
            })
            .collect(),
        serde_json::Value::Array(items) => {
            let mut headers = HashMap::new();
            for item in items {
                let obj = item.as_object().ok_or_else(|| {
                    "add_headers entries must be objects with 'name' and 'value'".to_string()
                })?;
                let name = obj.get("name").and_then(|v| v.as_str()).unwrap_or("");
                if name.trim().is_empty() {
                    // blank row left in the UI editor
                    continue;
                }
                let value = match obj.get("value") {
                    None => String::new(),
                    Some(v) => header_value_to_string(v)
                        .ok_or_else(|| format!("add_headers['{}'] must be a scalar value", name))?,
                };
                headers.insert(name.to_string(), value);
            }
            Ok(headers)
        }
        _ => Err(
            "add_headers must be a map of name: value or a list of {name, value} objects"
                .to_string(),
        ),
    }
}

impl ProxyRewritePlugin {
    /// Builds the plugin from node config.
    ///
    /// Accepted keys (all optional):
    /// - `phase` (string, default `request`): `request` or `response`; any
    ///   value other than `response` falls back to `request`.
    /// - `strip_path_prefix` (string): prefix removed from the request path
    ///   when it matches; the result is normalized to start with `/`.
    /// - `add_path_prefix` (string): prefix prepended to the request path
    ///   (after stripping).
    /// - `add_headers` (map `{name: value}` **or** array `[{name, value}]`,
    ///   the UI editor's form): headers to append. Scalar values are
    ///   stringified; malformed shapes (e.g. a plain string, or nested
    ///   objects as values) error here at config load.
    /// - `remove_headers` (array of strings): header names to delete.
    ///
    /// ```yaml
    /// type: proxy-rewrite
    /// config:
    ///   phase: request
    ///   strip_path_prefix: /api/v1
    ///   add_headers:
    ///     x-forwarded-tier: edge
    ///   remove_headers: [x-internal]
    /// ```
    pub fn from_config(config: &HashMap<String, serde_json::Value>) -> Result<Self, String> {
        // The mirror of response-rewrite's mix-up: that plugin takes a single
        // `headers` object, but proxy-rewrite takes `add_headers`/`remove_headers`.
        // Reject the misplaced key rather than silently ignoring it.
        if config.contains_key("headers") {
            return Err(
                "proxy-rewrite has no 'headers' key (that is response-rewrite's schema); \
                 use add_headers: { name: value } and remove_headers: [name]"
                    .to_string(),
            );
        }

        let phase = match config.get("phase").and_then(|v| v.as_str()) {
            Some("response") => RewritePhase::Response,
            _ => RewritePhase::Request,
        };

        let strip_path_prefix = config
            .get("strip_path_prefix")
            .and_then(|v| v.as_str())
            .map(String::from);

        let add_path_prefix = config
            .get("add_path_prefix")
            .and_then(|v| v.as_str())
            .map(String::from);

        let add_headers = parse_add_headers(config)?;

        let remove_headers = config
            .get("remove_headers")
            .and_then(|v| v.as_array())
            .map(|seq| {
                seq.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();

        Ok(Self {
            strip_path_prefix,
            add_path_prefix,
            add_headers,
            remove_headers,
            phase,
        })
    }
}

#[async_trait]
impl Plugin for ProxyRewritePlugin {
    fn plugin_type(&self) -> &str {
        "proxy-rewrite"
    }

    async fn execute(
        &self,
        mut ctx: Context,
        _named_inputs: &HashMap<String, serde_json::Value>,
    ) -> PluginResult {
        match self.phase {
            RewritePhase::Request => {
                // Strip path prefix
                if let Some(ref prefix) = self.strip_path_prefix {
                    if ctx.request.path.starts_with(prefix.as_str()) {
                        let new_path = ctx.request.path[prefix.len()..].to_string();
                        ctx.request.path = if new_path.is_empty() || !new_path.starts_with('/') {
                            format!("/{}", new_path.trim_start_matches('/'))
                        } else {
                            new_path
                        };
                    }
                }

                // Add path prefix
                if let Some(ref prefix) = self.add_path_prefix {
                    ctx.request.path = format!("{}{}", prefix, ctx.request.path);
                }

                // Add headers
                for (key, value) in &self.add_headers {
                    ctx.request
                        .headers
                        .entry(key.to_lowercase())
                        .or_default()
                        .push(value.clone());
                }

                // Remove headers (case-insensitive: the stored key may not be lowercase)
                for key in &self.remove_headers {
                    crate::plugins::util::headers::remove_ci(&mut ctx.request.headers, key);
                }
            }
            RewritePhase::Response => {
                // Add headers to response
                for (key, value) in &self.add_headers {
                    ctx.response
                        .headers
                        .entry(key.to_lowercase())
                        .or_default()
                        .push(value.clone());
                }

                // Remove headers from response (case-insensitive)
                for key in &self.remove_headers {
                    crate::plugins::util::headers::remove_ci(&mut ctx.response.headers, key);
                }
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
    use bytes::Bytes;

    fn test_context(path: &str) -> Context {
        Context {
            request: GatewayRequest {
                method: "GET".to_string(),
                path: path.to_string(),
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
    async fn test_strip_path_prefix() {
        let mut config = HashMap::new();
        config.insert(
            "strip_path_prefix".to_string(),
            serde_json::Value::String("/api/v1".to_string()),
        );
        config.insert(
            "phase".to_string(),
            serde_json::Value::String("request".to_string()),
        );

        let plugin = ProxyRewritePlugin::from_config(&config).unwrap();
        let ctx = test_context("/api/v1/users");
        let result = plugin.execute(ctx, &HashMap::new()).await.unwrap();
        assert_eq!(result.context.request.path, "/users");
    }

    #[tokio::test]
    async fn test_strip_path_prefix_root() {
        let mut config = HashMap::new();
        config.insert(
            "strip_path_prefix".to_string(),
            serde_json::Value::String("/api/v1".to_string()),
        );

        let plugin = ProxyRewritePlugin::from_config(&config).unwrap();
        let ctx = test_context("/api/v1");
        let result = plugin.execute(ctx, &HashMap::new()).await.unwrap();
        assert_eq!(result.context.request.path, "/");
    }

    #[tokio::test]
    async fn test_add_request_header() {
        let mut config = HashMap::new();
        let mut headers = serde_json::Map::new();
        headers.insert(
            "x-custom".to_string(),
            serde_json::Value::String("value".to_string()),
        );
        config.insert(
            "add_headers".to_string(),
            serde_json::Value::Object(headers),
        );

        let plugin = ProxyRewritePlugin::from_config(&config).unwrap();
        let ctx = test_context("/test");
        let result = plugin.execute(ctx, &HashMap::new()).await.unwrap();
        assert_eq!(
            result.context.request.headers.get("x-custom"),
            Some(&vec!["value".to_string()])
        );
    }

    #[tokio::test]
    async fn test_add_request_header_array_shape() {
        // The UI editor serializes add_headers as [{name, value}]
        let mut config = HashMap::new();
        config.insert(
            "add_headers".to_string(),
            serde_json::json!([{ "name": "x-custom", "value": "value" }]),
        );

        let plugin = ProxyRewritePlugin::from_config(&config).unwrap();
        let ctx = test_context("/test");
        let result = plugin.execute(ctx, &HashMap::new()).await.unwrap();
        assert_eq!(
            result.context.request.headers.get("x-custom"),
            Some(&vec!["value".to_string()])
        );
    }

    #[tokio::test]
    async fn test_add_response_header_array_shape() {
        let mut config = HashMap::new();
        config.insert(
            "phase".to_string(),
            serde_json::Value::String("response".to_string()),
        );
        config.insert(
            "add_headers".to_string(),
            serde_json::json!([
                { "name": "x-custom", "value": "value" },
                { "name": "", "value": "ignored blank row" }
            ]),
        );

        let plugin = ProxyRewritePlugin::from_config(&config).unwrap();
        let ctx = test_context("/test");
        let result = plugin.execute(ctx, &HashMap::new()).await.unwrap();
        assert_eq!(
            result.context.response.headers.get("x-custom"),
            Some(&vec!["value".to_string()])
        );
        assert_eq!(result.context.response.headers.len(), 1);
    }

    #[tokio::test]
    async fn test_add_headers_numeric_value_map_shape() {
        let mut config = HashMap::new();
        config.insert(
            "add_headers".to_string(),
            serde_json::json!({ "x-version": 2 }),
        );

        let plugin = ProxyRewritePlugin::from_config(&config).unwrap();
        let ctx = test_context("/test");
        let result = plugin.execute(ctx, &HashMap::new()).await.unwrap();
        assert_eq!(
            result.context.request.headers.get("x-version"),
            Some(&vec!["2".to_string()])
        );
    }

    #[tokio::test]
    async fn test_add_headers_rejects_malformed() {
        let mut config = HashMap::new();
        config.insert(
            "add_headers".to_string(),
            serde_json::Value::String("x-custom: value".to_string()),
        );
        assert!(ProxyRewritePlugin::from_config(&config).is_err());

        let mut config = HashMap::new();
        config.insert(
            "add_headers".to_string(),
            serde_json::json!({ "x-nested": {"a": 1} }),
        );
        assert!(ProxyRewritePlugin::from_config(&config).is_err());
    }

    #[tokio::test]
    async fn test_remove_response_header() {
        let mut config = HashMap::new();
        config.insert(
            "phase".to_string(),
            serde_json::Value::String("response".to_string()),
        );
        config.insert(
            "remove_headers".to_string(),
            serde_json::Value::Array(vec![serde_json::Value::String("x-internal".to_string())]),
        );

        let plugin = ProxyRewritePlugin::from_config(&config).unwrap();
        let mut ctx = test_context("/test");
        ctx.response
            .headers
            .insert("x-internal".to_string(), vec!["secret".to_string()]);

        let result = plugin.execute(ctx, &HashMap::new()).await.unwrap();
        assert!(!result.context.response.headers.contains_key("x-internal"));
    }

    /// Header names are case-insensitive: a response header stored with a
    /// non-lowercase key (e.g. from a Lua script or another plugin) must still
    /// be removed by a lowercase `remove_headers` entry, and vice versa.
    #[tokio::test]
    async fn test_remove_response_header_is_case_insensitive() {
        let mut config = HashMap::new();
        config.insert(
            "phase".to_string(),
            serde_json::Value::String("response".to_string()),
        );
        config.insert(
            "remove_headers".to_string(),
            serde_json::Value::Array(vec![serde_json::Value::String("x-powered-by".to_string())]),
        );

        let plugin = ProxyRewritePlugin::from_config(&config).unwrap();
        let mut ctx = test_context("/test");
        // Stored with mixed case, removed with a lowercase config entry.
        ctx.response
            .headers
            .insert("X-Powered-By".to_string(), vec!["php".to_string()]);

        let result = plugin.execute(ctx, &HashMap::new()).await.unwrap();
        assert!(!result.context.response.headers.contains_key("X-Powered-By"));
    }
}
