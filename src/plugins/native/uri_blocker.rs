//! URI block-rule plugin (`uri-blocker`).
//!
//! Port of APISIX's `uri-blocker`: matches the request URI (path plus query
//! string) against a list of regexes and rejects matching requests with a
//! configurable status code. Rejections are routed through the node's error
//! port with error code `URI_BLOCKED`.

use async_trait::async_trait;
use bytes::Bytes;
use regex::Regex;
use std::collections::HashMap;

use crate::context::{Context, GatewayError};
use crate::plugins::{Plugin, PluginExecutionError, PluginOutput, PluginResult};

/// Blocks requests whose URI matches any configured `block_rules` regex.
///
/// The matched subject is `crate::vars`' `request_uri` — the path plus
/// `?query` when query parameters exist (APISIX matches `ctx.var.request_uri`).
/// Regexes are compiled once at config load; `case_insensitive` prefixes each
/// with `(?i)` like APISIX does with its concatenated rule string.
pub struct UriBlockerPlugin {
    /// Compiled block rules; a request matching any of them is rejected.
    block_rules: Vec<Regex>,
    /// HTTP status for rejections (default 403).
    rejected_code: u16,
    /// Optional rejection body message; when unset the response body is empty.
    rejected_msg: Option<String>,
}

impl UriBlockerPlugin {
    /// Builds the plugin from node config.
    ///
    /// Accepted keys:
    /// - `block_rules` (array of regex strings, **required**, non-empty):
    ///   rules tested against the request URI (path + query string). Invalid
    ///   regexes and empty lists are config errors.
    /// - `rejected_code` (integer 200–599, default `403`): rejection status.
    /// - `rejected_msg` (string, optional): when set, rejections carry a JSON
    ///   body `{"error_msg": ...}`; when unset the body is empty (APISIX
    ///   parity).
    /// - `case_insensitive` (bool, default `false`): match rules
    ///   case-insensitively.
    ///
    /// ```yaml
    /// type: uri-blocker
    /// config:
    ///   block_rules: ["root.exe", "root.m+", "^/admin/"]
    ///   rejected_code: 404
    ///   case_insensitive: true
    /// ```
    pub fn from_config(config: &HashMap<String, serde_json::Value>) -> Result<Self, String> {
        let case_insensitive = config
            .get("case_insensitive")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        let rules = config
            .get("block_rules")
            .and_then(|v| v.as_array())
            .ok_or_else(|| {
                "uri-blocker: block_rules is required and must be an array".to_string()
            })?;
        if rules.is_empty() {
            return Err("uri-blocker: block_rules must not be empty".to_string());
        }

        let block_rules = rules
            .iter()
            .map(|item| {
                let s = item
                    .as_str()
                    .ok_or_else(|| "block_rules entries must be strings".to_string())?;
                if s.is_empty() {
                    return Err("block_rules entries must be non-empty".to_string());
                }
                let pattern = if case_insensitive {
                    format!("(?i){}", s)
                } else {
                    s.to_string()
                };
                Regex::new(&pattern)
                    .map_err(|e| format!("invalid regex '{}' in block_rules: {}", s, e))
            })
            .collect::<Result<Vec<_>, _>>()?;

        let rejected_code = match config.get("rejected_code") {
            None => 403,
            Some(v) => {
                let code = v
                    .as_u64()
                    .ok_or_else(|| "rejected_code must be an integer".to_string())?;
                if !(200..=599).contains(&code) {
                    return Err("rejected_code must be between 200 and 599".to_string());
                }
                code as u16
            }
        };

        Ok(Self {
            block_rules,
            rejected_code,
            rejected_msg: config
                .get("rejected_msg")
                .and_then(|v| v.as_str())
                .map(String::from),
        })
    }
}

#[async_trait]
impl Plugin for UriBlockerPlugin {
    fn plugin_type(&self) -> &str {
        "uri-blocker"
    }

    async fn execute(
        &self,
        ctx: Context,
        _named_inputs: &HashMap<String, serde_json::Value>,
    ) -> PluginResult {
        let request_uri = crate::vars::resolve(&ctx, "request_uri")
            .map(|v| v.into_owned())
            .unwrap_or_else(|| ctx.request.path.clone());

        if self.block_rules.iter().any(|re| re.is_match(&request_uri)) {
            let mut ctx = ctx;
            ctx.response.status_code = self.rejected_code;
            if let Some(ref msg) = self.rejected_msg {
                ctx.response.body =
                    Bytes::from(serde_json::json!({ "error_msg": msg }).to_string());
                ctx.response.headers.insert(
                    "content-type".to_string(),
                    vec!["application/json".to_string()],
                );
            } else {
                ctx.response.body = Bytes::new();
            }
            return Err(PluginExecutionError {
                context: ctx,
                error: GatewayError {
                    node_id: String::new(),
                    code: "URI_BLOCKED".to_string(),
                    message: self
                        .rejected_msg
                        .clone()
                        .unwrap_or_else(|| "request URI is blocked".to_string()),
                    metadata: HashMap::new(),
                },
            });
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

    fn test_context(path: &str, query: &[(&str, &str)]) -> Context {
        let mut query_params: HashMap<String, Vec<String>> = HashMap::new();
        for (k, v) in query {
            query_params
                .entry(k.to_string())
                .or_default()
                .push(v.to_string());
        }
        Context {
            request: GatewayRequest {
                method: "GET".to_string(),
                path: path.to_string(),
                host: "localhost".to_string(),
                scheme: "http".to_string(),
                headers: HashMap::new(),
                query_params,
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

    fn config(json: serde_json::Value) -> HashMap<String, serde_json::Value> {
        serde_json::from_value(json).unwrap()
    }

    #[test]
    fn test_config_requires_block_rules() {
        assert!(UriBlockerPlugin::from_config(&config(serde_json::json!({}))).is_err());
        assert!(UriBlockerPlugin::from_config(&config(serde_json::json!({
            "block_rules": []
        })))
        .is_err());
        assert!(UriBlockerPlugin::from_config(&config(serde_json::json!({
            "block_rules": ["("]
        })))
        .is_err());
        assert!(UriBlockerPlugin::from_config(&config(serde_json::json!({
            "block_rules": [42]
        })))
        .is_err());
        assert!(UriBlockerPlugin::from_config(&config(serde_json::json!({
            "block_rules": ["^/admin"], "rejected_code": 99
        })))
        .is_err());
        assert!(UriBlockerPlugin::from_config(&config(serde_json::json!({
            "block_rules": ["^/admin"]
        })))
        .is_ok());
    }

    #[tokio::test]
    async fn test_blocks_matching_path() {
        let plugin = UriBlockerPlugin::from_config(&config(serde_json::json!({
            "block_rules": ["root.exe", "^/admin/"]
        })))
        .unwrap();

        let err = plugin
            .execute(test_context("/admin/users", &[]), &HashMap::new())
            .await
            .unwrap_err();
        assert_eq!(err.error.code, "URI_BLOCKED");
        assert_eq!(err.context.response.status_code, 403);
        // no rejected_msg -> empty body (APISIX parity)
        assert!(err.context.response.body.is_empty());

        assert!(plugin
            .execute(test_context("/public", &[]), &HashMap::new())
            .await
            .is_ok());
    }

    #[tokio::test]
    async fn test_matches_query_string() {
        let plugin = UriBlockerPlugin::from_config(&config(serde_json::json!({
            "block_rules": ["root.exe"]
        })))
        .unwrap();

        // rule matches inside the query string, like APISIX's request_uri
        assert!(plugin
            .execute(
                test_context("/download", &[("file", "root.exe")]),
                &HashMap::new()
            )
            .await
            .is_err());
        assert!(plugin
            .execute(
                test_context("/download", &[("file", "notes.txt")]),
                &HashMap::new()
            )
            .await
            .is_ok());
    }

    #[tokio::test]
    async fn test_case_insensitive() {
        let sensitive = UriBlockerPlugin::from_config(&config(serde_json::json!({
            "block_rules": ["/admin"]
        })))
        .unwrap();
        assert!(sensitive
            .execute(test_context("/ADMIN/panel", &[]), &HashMap::new())
            .await
            .is_ok());

        let insensitive = UriBlockerPlugin::from_config(&config(serde_json::json!({
            "block_rules": ["/admin"], "case_insensitive": true
        })))
        .unwrap();
        assert!(insensitive
            .execute(test_context("/ADMIN/panel", &[]), &HashMap::new())
            .await
            .is_err());
    }

    #[tokio::test]
    async fn test_custom_code_and_message() {
        let plugin = UriBlockerPlugin::from_config(&config(serde_json::json!({
            "block_rules": ["^/admin"], "rejected_code": 404, "rejected_msg": "not found"
        })))
        .unwrap();

        let err = plugin
            .execute(test_context("/admin", &[]), &HashMap::new())
            .await
            .unwrap_err();
        assert_eq!(err.context.response.status_code, 404);
        let body: serde_json::Value = serde_json::from_slice(&err.context.response.body).unwrap();
        assert_eq!(body["error_msg"], "not found");
    }
}
