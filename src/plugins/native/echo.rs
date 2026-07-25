//! The `echo` node — replaces or wraps the response body and adds response
//! headers. A response-phase node: place it after `upstream` (between
//! `upstream` and `client`) so it sees the upstream response.
//!
//! Port of APISIX's `echo` plugin (which APISIX itself documents as a
//! demo/util plugin). `before_body`/`body`/`after_body` map to APISIX's
//! body_filter behavior and `headers` to its header_filter behavior.

use async_trait::async_trait;
use bytes::{Bytes, BytesMut};
use std::collections::HashMap;

use crate::context::Context;
use crate::plugins::util::content_codec::{decode, ContentEncoding};
use crate::plugins::{Plugin, PluginOutput, PluginResult};

/// Rewrites the response: `body` replaces the upstream body, then
/// `before_body` / `after_body` are concatenated around it, and `headers`
/// are set on the response (replacing any existing values, like
/// `ngx.header[...]`).
///
/// Because the body is mutated, the plugin follows featherbit's body-mutation
/// convention: `content-length` is removed (the server recomputes it) and a
/// compressed upstream body is decoded first, with `content-encoding`
/// removed. This plugin never fails at execution time.
pub struct EchoPlugin {
    /// Replacement for the upstream response body.
    body: Option<String>,
    /// Prepended to the (possibly replaced) body.
    before_body: Option<String>,
    /// Appended to the (possibly replaced) body.
    after_body: Option<String>,
    /// Response headers to set (names lowercased, values replace existing).
    headers: HashMap<String, String>,
}

/// Reads an optional string key, rejecting non-string values.
fn optional_string(
    config: &HashMap<String, serde_json::Value>,
    key: &str,
) -> Result<Option<String>, String> {
    match config.get(key) {
        None => Ok(None),
        Some(serde_json::Value::String(s)) => Ok(Some(s.clone())),
        Some(_) => Err(format!("{} must be a string", key)),
    }
}

impl EchoPlugin {
    /// Builds the plugin from node config.
    ///
    /// Accepted keys — at least one of `body` / `before_body` / `after_body`
    /// is required (APISIX parity):
    /// - `body` (string): replaces the upstream response body.
    /// - `before_body` (string): prepended to the body (after any `body`
    ///   replacement).
    /// - `after_body` (string): appended to the body.
    /// - `headers` (map `{name: value}` **or** array `[{name, value}]`, the
    ///   UI editor's form): response headers to set. Values must be scalars
    ///   (strings, numbers, bools — stringified); names are lowercased;
    ///   array entries with a blank `name` are skipped. Existing values for
    ///   the same header are replaced.
    ///
    /// ```yaml
    /// type: echo
    /// config:
    ///   before_body: "before the body modification "
    ///   after_body: " after the body modification"
    ///   headers:
    ///     x-served-by: featherbit
    /// ```
    pub fn from_config(config: &HashMap<String, serde_json::Value>) -> Result<Self, String> {
        let body = optional_string(config, "body")?;
        let before_body = optional_string(config, "before_body")?;
        let after_body = optional_string(config, "after_body")?;

        if body.is_none() && before_body.is_none() && after_body.is_none() {
            return Err(
                "echo plugin requires at least one of 'body', 'before_body' or 'after_body'"
                    .to_string(),
            );
        }

        let scalar = |v: &serde_json::Value, name: &str| -> Result<String, String> {
            match v {
                serde_json::Value::String(s) => Ok(s.clone()),
                serde_json::Value::Number(n) => Ok(n.to_string()),
                serde_json::Value::Bool(b) => Ok(b.to_string()),
                _ => Err(format!("headers['{}'] must be a scalar value", name)),
            }
        };

        let headers = match config.get("headers") {
            None => HashMap::new(),
            // Map form (YAML): headers: { x-foo: bar }
            Some(serde_json::Value::Object(m)) => m
                .iter()
                .map(|(k, v)| Ok((k.to_lowercase(), scalar(v, k)?)))
                .collect::<Result<HashMap<_, _>, String>>()?,
            // Array form (UI editor): headers: [{ name: x-foo, value: bar }]
            Some(serde_json::Value::Array(items)) => {
                let mut headers = HashMap::new();
                for item in items {
                    let obj = item.as_object().ok_or_else(|| {
                        "headers entries must be objects with 'name' and 'value'".to_string()
                    })?;
                    let name = obj.get("name").and_then(|v| v.as_str()).unwrap_or("");
                    if name.trim().is_empty() {
                        // blank row left in the UI editor
                        continue;
                    }
                    let value = match obj.get("value") {
                        None => String::new(),
                        Some(v) => scalar(v, name)?,
                    };
                    headers.insert(name.to_lowercase(), value);
                }
                headers
            }
            Some(_) => {
                return Err(
                    "headers must be a map of name: value or a list of {name, value} objects"
                        .to_string(),
                )
            }
        };

        Ok(Self {
            body,
            before_body,
            after_body,
            headers,
        })
    }
}

#[async_trait]
impl Plugin for EchoPlugin {
    fn plugin_type(&self) -> &str {
        "echo"
    }

    async fn execute(
        &self,
        mut ctx: Context,
        _named_inputs: &HashMap<String, serde_json::Value>,
    ) -> PluginResult {
        // Body mutation (always: from_config requires at least one body key).
        let current: Bytes = match &self.body {
            // Full replacement ignores the upstream body entirely.
            Some(body) => Bytes::from(body.clone()),
            // before/after wrap the upstream body; a compressed body is
            // decoded first so text concatenates onto text. If the encoding
            // is unsupported or the stream corrupt, the raw bytes are used.
            None => {
                let encoding = ctx
                    .response
                    .headers
                    .get("content-encoding")
                    .and_then(|v| v.first())
                    .and_then(|v| ContentEncoding::parse(v).ok())
                    .flatten();
                match encoding {
                    Some(enc) => decode(&enc, &ctx.response.body)
                        .unwrap_or_else(|_| ctx.response.body.clone()),
                    None => ctx.response.body.clone(),
                }
            }
        };

        let mut buf = BytesMut::new();
        if let Some(before) = &self.before_body {
            buf.extend_from_slice(before.as_bytes());
        }
        buf.extend_from_slice(&current);
        if let Some(after) = &self.after_body {
            buf.extend_from_slice(after.as_bytes());
        }
        ctx.response.body = buf.freeze();

        // Body-mutation convention: the body is now plain (decoded) and has
        // a new length — drop the stale metadata headers.
        ctx.response.headers.remove("content-length");
        ctx.response.headers.remove("content-encoding");

        // Header rewrites (set semantics, like ngx.header[name] = value).
        for (name, value) in &self.headers {
            ctx.response
                .headers
                .insert(name.clone(), vec![value.clone()]);
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
    use crate::plugins::util::content_codec::encode;

    fn test_context(body: &str) -> Context {
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
                status_code: 200,
                headers: HashMap::new(),
                body: Bytes::from(body.to_string()),
            },
            message: HashMap::new(),
            errors: Vec::new(),
        }
    }

    fn config(json: serde_json::Value) -> HashMap<String, serde_json::Value> {
        serde_json::from_value(json).unwrap()
    }

    #[test]
    fn test_echo_config_validation() {
        // At least one body key is required (APISIX anyOf).
        assert!(EchoPlugin::from_config(&HashMap::new()).is_err());
        assert!(EchoPlugin::from_config(&config(serde_json::json!({
            "headers": {"x-a": "b"}
        })))
        .is_err());

        // Non-string body keys rejected.
        assert!(EchoPlugin::from_config(&config(serde_json::json!({"body": 5}))).is_err());

        // Non-scalar header values and non-map headers rejected.
        assert!(EchoPlugin::from_config(&config(serde_json::json!({
            "body": "x",
            "headers": {"x-a": {"nested": true}}
        })))
        .is_err());
        assert!(EchoPlugin::from_config(&config(serde_json::json!({
            "body": "x",
            "headers": ["x-a"]
        })))
        .is_err());

        // Valid shapes accepted.
        assert!(EchoPlugin::from_config(&config(serde_json::json!({"body": "x"}))).is_ok());
        assert!(EchoPlugin::from_config(&config(serde_json::json!({
            "before_body": "pre",
            "headers": {"x-version": 2}
        })))
        .is_ok());
    }

    #[tokio::test]
    async fn test_echo_body_replacement() {
        let plugin = EchoPlugin::from_config(&config(serde_json::json!({
            "body": "replaced"
        })))
        .unwrap();

        let mut ctx = test_context("upstream body");
        ctx.response
            .headers
            .insert("content-length".to_string(), vec!["13".to_string()]);

        let result = plugin.execute(ctx, &HashMap::new()).await.unwrap();
        assert_eq!(result.context.response.body, Bytes::from("replaced"));
        // Stale content-length dropped per the body-mutation convention.
        assert!(!result
            .context
            .response
            .headers
            .contains_key("content-length"));
    }

    #[tokio::test]
    async fn test_echo_before_and_after_body_wrap_upstream() {
        let plugin = EchoPlugin::from_config(&config(serde_json::json!({
            "before_body": "pre|",
            "after_body": "|post"
        })))
        .unwrap();

        let result = plugin
            .execute(test_context("upstream"), &HashMap::new())
            .await
            .unwrap();
        assert_eq!(
            result.context.response.body,
            Bytes::from("pre|upstream|post")
        );
    }

    #[tokio::test]
    async fn test_echo_wraps_replaced_body() {
        let plugin = EchoPlugin::from_config(&config(serde_json::json!({
            "body": "mid",
            "before_body": "a|",
            "after_body": "|z"
        })))
        .unwrap();

        let result = plugin
            .execute(test_context("ignored"), &HashMap::new())
            .await
            .unwrap();
        assert_eq!(result.context.response.body, Bytes::from("a|mid|z"));
    }

    #[tokio::test]
    async fn test_echo_decodes_compressed_upstream_body() {
        let plugin = EchoPlugin::from_config(&config(serde_json::json!({
            "before_body": "pre|"
        })))
        .unwrap();

        let mut ctx = test_context("");
        ctx.response.body = encode(&ContentEncoding::Gzip, &Bytes::from("upstream"), 6).unwrap();
        ctx.response
            .headers
            .insert("content-encoding".to_string(), vec!["gzip".to_string()]);
        ctx.response
            .headers
            .insert("content-length".to_string(), vec!["28".to_string()]);

        let result = plugin.execute(ctx, &HashMap::new()).await.unwrap();
        let ctx = result.context;
        assert_eq!(ctx.response.body, Bytes::from("pre|upstream"));
        assert!(!ctx.response.headers.contains_key("content-encoding"));
        assert!(!ctx.response.headers.contains_key("content-length"));
    }

    #[tokio::test]
    async fn test_echo_headers_array_shape() {
        // The UI editor serializes headers as [{name, value}]
        let plugin = EchoPlugin::from_config(&config(serde_json::json!({
            "body": "x",
            "headers": [
                { "name": "X-Served-By", "value": "featherbit" },
                { "name": "", "value": "ignored blank row" }
            ]
        })))
        .unwrap();

        let result = plugin
            .execute(test_context("upstream"), &HashMap::new())
            .await
            .unwrap();
        let headers = &result.context.response.headers;
        assert_eq!(
            headers.get("x-served-by"),
            Some(&vec!["featherbit".to_string()])
        );
        assert!(!headers
            .values()
            .any(|v| v.contains(&"ignored blank row".to_string())));
    }

    #[tokio::test]
    async fn test_echo_sets_headers_with_replace_semantics() {
        let plugin = EchoPlugin::from_config(&config(serde_json::json!({
            "body": "x",
            "headers": {"X-Served-By": "featherbit", "x-version": 2}
        })))
        .unwrap();

        let mut ctx = test_context("upstream");
        ctx.response
            .headers
            .insert("x-served-by".to_string(), vec!["upstream-host".to_string()]);

        let result = plugin.execute(ctx, &HashMap::new()).await.unwrap();
        let headers = &result.context.response.headers;
        // Name lowercased, existing value replaced (not appended).
        assert_eq!(
            headers.get("x-served-by"),
            Some(&vec!["featherbit".to_string()])
        );
        assert_eq!(headers.get("x-version"), Some(&vec!["2".to_string()]));
    }
}
