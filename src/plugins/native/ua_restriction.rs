//! User-Agent allow/deny list plugin (`ua-restriction`).
//!
//! Port of APISIX's `ua-restriction`: matches the request's `User-Agent`
//! header against a list of regexes — either an allowlist (only matching
//! agents pass) or a denylist (matching agents are rejected). Rejections are
//! routed through the node's error port with error code `UA_RESTRICTED`.

use async_trait::async_trait;
use bytes::Bytes;
use regex::Regex;
use std::collections::HashMap;

use crate::context::{Context, GatewayError};
use crate::plugins::{Plugin, PluginExecutionError, PluginOutput, PluginResult};

/// Restricts access based on the `User-Agent` request header.
///
/// Exactly one of `allowlist` / `denylist` is configured (APISIX `oneOf`
/// parity). Each User-Agent value is trimmed and tested against the regexes;
/// in allowlist mode a request passes when any value matches any rule, in
/// denylist mode a request is rejected when any value matches any rule.
/// A missing User-Agent is rejected unless `bypass_missing` is set.
pub struct UaRestrictionPlugin {
    /// Compiled allowlist regexes; non-empty means allowlist mode.
    allowlist: Vec<Regex>,
    /// Compiled denylist regexes; non-empty means denylist mode.
    denylist: Vec<Regex>,
    /// Pass requests that carry no User-Agent header (default `false`).
    bypass_missing: bool,
    /// HTTP status for rejections (default 403).
    rejected_code: u16,
    /// Body message for rejections.
    rejected_msg: String,
}

/// Parses a config key as an array of non-empty regex strings, compiling
/// each at config load. An absent key yields an empty list.
fn parse_regex_list(
    config: &HashMap<String, serde_json::Value>,
    key: &str,
) -> Result<Vec<Regex>, String> {
    let Some(raw) = config.get(key) else {
        return Ok(Vec::new());
    };
    let arr = raw
        .as_array()
        .ok_or_else(|| format!("{} must be an array of regex strings", key))?;
    arr.iter()
        .map(|item| {
            let s = item
                .as_str()
                .ok_or_else(|| format!("{} entries must be strings", key))?;
            if s.is_empty() {
                return Err(format!("{} entries must be non-empty", key));
            }
            Regex::new(s).map_err(|e| format!("invalid regex '{}' in {}: {}", s, key, e))
        })
        .collect()
}

impl UaRestrictionPlugin {
    /// Builds the plugin from node config.
    ///
    /// Accepted keys:
    /// - `allowlist` (array of regex strings): only matching User-Agents pass.
    /// - `denylist` (array of regex strings): matching User-Agents are rejected.
    ///   Exactly **one** of `allowlist` / `denylist` must be non-empty
    ///   (APISIX rejects both-set and neither-set configs); regexes are
    ///   compiled here, so an invalid pattern is a config error.
    /// - `bypass_missing` (bool, default `false`): pass requests without a
    ///   User-Agent header instead of rejecting them.
    /// - `rejected_code` (integer 200–599, default `403`): rejection status.
    /// - `rejected_msg` (string, default `"Not allowed"`): rejection message,
    ///   returned as `{"message": ...}`.
    ///
    /// ```yaml
    /// type: ua-restriction
    /// config:
    ///   denylist: ["curl/.*", "(?i)spider"]
    ///   bypass_missing: false
    ///   rejected_msg: Not allowed
    /// ```
    pub fn from_config(config: &HashMap<String, serde_json::Value>) -> Result<Self, String> {
        let allowlist = parse_regex_list(config, "allowlist")?;
        let denylist = parse_regex_list(config, "denylist")?;

        if !allowlist.is_empty() && !denylist.is_empty() {
            return Err("ua-restriction: allowlist and denylist cannot both be set".to_string());
        }
        if allowlist.is_empty() && denylist.is_empty() {
            return Err(
                "ua-restriction: exactly one of allowlist or denylist must be non-empty"
                    .to_string(),
            );
        }

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

        let rejected_msg = config
            .get("rejected_msg")
            .and_then(|v| v.as_str())
            .unwrap_or("Not allowed")
            .to_string();

        Ok(Self {
            allowlist,
            denylist,
            bypass_missing: config
                .get("bypass_missing")
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
            rejected_code,
            rejected_msg,
        })
    }

    /// Builds the 403-style rejection: JSON body on the response, error routed
    /// through the error port with code `UA_RESTRICTED`.
    fn reject(&self, mut ctx: Context) -> PluginResult {
        ctx.response.status_code = self.rejected_code;
        ctx.response.body =
            Bytes::from(serde_json::json!({ "message": self.rejected_msg }).to_string());
        ctx.response.headers.insert(
            "content-type".to_string(),
            vec!["application/json".to_string()],
        );
        Err(PluginExecutionError {
            context: ctx,
            error: GatewayError {
                node_id: String::new(),
                code: "UA_RESTRICTED".to_string(),
                message: self.rejected_msg.clone(),
                metadata: HashMap::new(),
            },
        })
    }
}

#[async_trait]
impl Plugin for UaRestrictionPlugin {
    fn plugin_type(&self) -> &str {
        "ua-restriction"
    }

    async fn execute(
        &self,
        ctx: Context,
        _named_inputs: &HashMap<String, serde_json::Value>,
    ) -> PluginResult {
        let user_agents: Vec<&str> = ctx
            .request
            .headers
            .get("user-agent")
            .map(|values| values.iter().map(|v| v.trim()).collect())
            .unwrap_or_default();

        if user_agents.is_empty() {
            if self.bypass_missing {
                return Ok(PluginOutput {
                    context: ctx,
                    named_outputs: HashMap::new(),
                });
            }
            return self.reject(ctx);
        }

        let passed = if !self.allowlist.is_empty() {
            // Allowlist mode: any UA value matching any rule passes.
            user_agents
                .iter()
                .any(|ua| self.allowlist.iter().any(|re| re.is_match(ua)))
        } else {
            // Denylist mode: any UA value matching any rule rejects.
            !user_agents
                .iter()
                .any(|ua| self.denylist.iter().any(|re| re.is_match(ua)))
        };

        if !passed {
            return self.reject(ctx);
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

    fn test_context(user_agent: Option<&str>) -> Context {
        let mut headers = HashMap::new();
        if let Some(ua) = user_agent {
            headers.insert("user-agent".to_string(), vec![ua.to_string()]);
        }
        Context {
            request: GatewayRequest {
                method: "GET".to_string(),
                path: "/test".to_string(),
                host: "localhost".to_string(),
                scheme: "http".to_string(),
                headers,
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

    fn config(json: serde_json::Value) -> HashMap<String, serde_json::Value> {
        serde_json::from_value(json).unwrap()
    }

    #[test]
    fn test_config_requires_exactly_one_list() {
        // neither list
        assert!(UaRestrictionPlugin::from_config(&config(serde_json::json!({}))).is_err());
        // both lists
        assert!(UaRestrictionPlugin::from_config(&config(serde_json::json!({
            "allowlist": ["a"], "denylist": ["b"]
        })))
        .is_err());
        // one list is fine
        assert!(UaRestrictionPlugin::from_config(&config(serde_json::json!({
            "denylist": ["curl"]
        })))
        .is_ok());
    }

    #[test]
    fn test_config_rejects_invalid_regex_and_bad_shapes() {
        assert!(UaRestrictionPlugin::from_config(&config(serde_json::json!({
            "denylist": ["("]
        })))
        .is_err());
        assert!(UaRestrictionPlugin::from_config(&config(serde_json::json!({
            "denylist": "curl"
        })))
        .is_err());
        assert!(UaRestrictionPlugin::from_config(&config(serde_json::json!({
            "denylist": [""]
        })))
        .is_err());
        assert!(UaRestrictionPlugin::from_config(&config(serde_json::json!({
            "denylist": ["curl"], "rejected_code": 100
        })))
        .is_err());
    }

    #[tokio::test]
    async fn test_denylist_blocks_matching_ua() {
        let plugin = UaRestrictionPlugin::from_config(&config(serde_json::json!({
            "denylist": ["curl/.*", "(?i)spider"]
        })))
        .unwrap();

        let err = plugin
            .execute(test_context(Some("curl/8.1.2")), &HashMap::new())
            .await
            .unwrap_err();
        assert_eq!(err.error.code, "UA_RESTRICTED");
        assert_eq!(err.context.response.status_code, 403);

        // non-matching UA passes
        assert!(plugin
            .execute(test_context(Some("Mozilla/5.0")), &HashMap::new())
            .await
            .is_ok());
    }

    #[tokio::test]
    async fn test_allowlist_only_matching_ua_passes() {
        let plugin = UaRestrictionPlugin::from_config(&config(serde_json::json!({
            "allowlist": ["Mozilla.*"]
        })))
        .unwrap();

        assert!(plugin
            .execute(test_context(Some("Mozilla/5.0")), &HashMap::new())
            .await
            .is_ok());
        // trimmed before matching (APISIX str_strip parity)
        assert!(plugin
            .execute(test_context(Some("  Mozilla/5.0  ")), &HashMap::new())
            .await
            .is_ok());
        assert!(plugin
            .execute(test_context(Some("curl/8.1.2")), &HashMap::new())
            .await
            .is_err());
    }

    #[tokio::test]
    async fn test_missing_ua_bypass() {
        let deny = UaRestrictionPlugin::from_config(&config(serde_json::json!({
            "denylist": ["curl"]
        })))
        .unwrap();
        // default: missing UA is rejected
        assert!(deny
            .execute(test_context(None), &HashMap::new())
            .await
            .is_err());

        let bypass = UaRestrictionPlugin::from_config(&config(serde_json::json!({
            "denylist": ["curl"], "bypass_missing": true
        })))
        .unwrap();
        assert!(bypass
            .execute(test_context(None), &HashMap::new())
            .await
            .is_ok());
    }

    #[tokio::test]
    async fn test_custom_code_and_message() {
        let plugin = UaRestrictionPlugin::from_config(&config(serde_json::json!({
            "denylist": ["curl"], "rejected_code": 405, "rejected_msg": "go away"
        })))
        .unwrap();
        let err = plugin
            .execute(test_context(Some("curl/8.1.2")), &HashMap::new())
            .await
            .unwrap_err();
        assert_eq!(err.context.response.status_code, 405);
        let body: serde_json::Value = serde_json::from_slice(&err.context.response.body).unwrap();
        assert_eq!(body["message"], "go away");
    }
}
