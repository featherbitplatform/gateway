//! Consumer allow/deny list plugin (`consumer-restriction`).
//!
//! Restricts which already-authenticated consumers may reach a route. It does
//! not authenticate — an upstream auth node (e.g. `key-auth`) must have
//! attached the consumer identity first (the `consumer.*` keys in
//! `context.message`). This plugin then matches the consumer's name or group
//! against a configured whitelist/blacklist and, optionally, restricts a named
//! consumer to specific HTTP methods.

use async_trait::async_trait;
use bytes::Bytes;
use std::collections::HashMap;

use crate::context::{Context, GatewayError};
use crate::plugins::{Plugin, PluginExecutionError, PluginOutput, PluginResult};

/// What consumer attribute the lists match against.
#[derive(Debug, Clone, Copy, PartialEq)]
enum RestrictionType {
    /// Match `consumer.name`.
    ConsumerName,
    /// Match `consumer.group`.
    ConsumerGroupId,
}

/// A per-consumer HTTP-method allowlist entry.
struct AllowedByMethod {
    /// Consumer name this restriction applies to.
    user: String,
    /// Uppercased HTTP methods the consumer may use.
    methods: Vec<String>,
}

/// Restricts access based on the attached consumer's name or group.
///
/// Evaluation mirrors APISIX: the blacklist is checked first (a match
/// rejects), then the whitelist (a non-match rejects), then — only for
/// consumers not already cleared by the whitelist — `allowed_by_methods`. When
/// no consumer is attached the request is rejected `401`; list rejections use
/// `rejected_code` (default `403`). All rejections carry error code
/// `CONSUMER_RESTRICTED`.
pub struct ConsumerRestrictionPlugin {
    /// Which consumer attribute the lists match (`consumer_name` / `consumer_group_id`).
    restriction_type: RestrictionType,
    /// When non-empty, the value must be present or the request is rejected.
    whitelist: Vec<String>,
    /// When non-empty, a matching value is rejected.
    blacklist: Vec<String>,
    /// Per-consumer HTTP-method allowlists.
    allowed_by_methods: Vec<AllowedByMethod>,
    /// HTTP status used for list rejections.
    rejected_code: u16,
    /// Optional custom rejection message.
    rejected_msg: Option<String>,
}

impl ConsumerRestrictionPlugin {
    /// Builds the plugin from node config, failing fast on invalid combinations.
    ///
    /// Accepted keys:
    /// - `type` (string, default `"consumer_name"`): the consumer attribute the
    ///   lists match. Supported: `consumer_name` (matches `consumer.name`) and
    ///   `consumer_group_id` (matches `consumer.group`). `service_id` and
    ///   `route_id` exist in APISIX but have no featherbit analogue and are
    ///   rejected at config load.
    /// - `whitelist` (array of strings): values allowed through; a consumer
    ///   whose value is absent is rejected. Mutually exclusive with `blacklist`.
    /// - `blacklist` (array of strings): values rejected on match.
    /// - `allowed_by_methods` (array of `{ user, methods }`): restricts the
    ///   named consumer to the listed HTTP methods.
    /// - At least one of `whitelist` / `blacklist` / `allowed_by_methods` is
    ///   required.
    /// - `rejected_code` (integer, default `403`): status for list rejections.
    /// - `rejected_msg` (string, optional): custom rejection message.
    ///
    /// ```yaml
    /// type: consumer-restriction
    /// config:
    ///   type: consumer_name
    ///   whitelist: ["alice", "bob"]
    ///   allowed_by_methods:
    ///     - user: alice
    ///       methods: ["GET", "POST"]
    ///   rejected_code: 403
    ///   rejected_msg: "You shall not pass"
    /// ```
    pub fn from_config(config: &HashMap<String, serde_json::Value>) -> Result<Self, String> {
        let restriction_type = match config
            .get("type")
            .and_then(|v| v.as_str())
            .unwrap_or("consumer_name")
        {
            "consumer_name" => RestrictionType::ConsumerName,
            "consumer_group_id" => RestrictionType::ConsumerGroupId,
            other @ ("service_id" | "route_id") => {
                return Err(format!(
                    "consumer-restriction: type '{other}' is not supported in featherbit; \
                     use 'consumer_name' or 'consumer_group_id'"
                ));
            }
            other => {
                return Err(format!(
                    "consumer-restriction: unknown type '{other}'; \
                     expected 'consumer_name' or 'consumer_group_id'"
                ));
            }
        };

        let string_list = |key: &str| -> Vec<String> {
            config
                .get(key)
                .and_then(|v| v.as_array())
                .map(|seq| {
                    seq.iter()
                        .filter_map(|v| v.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default()
        };

        let whitelist = string_list("whitelist");
        let blacklist = string_list("blacklist");

        if !whitelist.is_empty() && !blacklist.is_empty() {
            return Err(
                "consumer-restriction: set only one of 'whitelist' or 'blacklist'".to_string(),
            );
        }

        let allowed_by_methods = config
            .get("allowed_by_methods")
            .and_then(|v| v.as_array())
            .map(|seq| {
                seq.iter()
                    .filter_map(|entry| {
                        let user = entry.get("user")?.as_str()?.to_string();
                        let methods = entry
                            .get("methods")?
                            .as_array()?
                            .iter()
                            .filter_map(|m| m.as_str().map(|s| s.to_uppercase()))
                            .collect();
                        Some(AllowedByMethod { user, methods })
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();

        if whitelist.is_empty() && blacklist.is_empty() && allowed_by_methods.is_empty() {
            return Err(
                "consumer-restriction: at least one of 'whitelist', 'blacklist' or \
                 'allowed_by_methods' is required"
                    .to_string(),
            );
        }

        let rejected_code = config
            .get("rejected_code")
            .and_then(|v| v.as_u64())
            .map(|c| c as u16)
            .unwrap_or(403);

        let rejected_msg = config
            .get("rejected_msg")
            .and_then(|v| v.as_str())
            .map(String::from);

        Ok(Self {
            restriction_type,
            whitelist,
            blacklist,
            allowed_by_methods,
            rejected_code,
            rejected_msg,
        })
    }

    /// The `context.message` key holding the value this instance matches on.
    fn value_key(&self) -> &'static str {
        match self.restriction_type {
            RestrictionType::ConsumerName => "consumer.name",
            RestrictionType::ConsumerGroupId => "consumer.group",
        }
    }

    /// Human label for the configured type, used in default messages.
    fn type_label(&self) -> &'static str {
        match self.restriction_type {
            RestrictionType::ConsumerName => "consumer_name",
            RestrictionType::ConsumerGroupId => "consumer_group_id",
        }
    }

    /// Builds a rejection carrying the context so the graph routes the error port.
    fn reject(&self, mut ctx: Context, status: u16, message: String) -> PluginResult {
        ctx.response.status_code = status;
        ctx.response.body = Bytes::from(serde_json::json!({ "message": message }).to_string());
        ctx.response.headers.insert(
            "content-type".to_string(),
            vec!["application/json".to_string()],
        );
        Err(PluginExecutionError {
            context: ctx,
            error: GatewayError {
                node_id: String::new(),
                code: "CONSUMER_RESTRICTED".to_string(),
                message,
                metadata: HashMap::new(),
            },
        })
    }
}

#[async_trait]
impl Plugin for ConsumerRestrictionPlugin {
    fn plugin_type(&self) -> &str {
        "consumer-restriction"
    }

    async fn execute(
        &self,
        ctx: Context,
        _named_inputs: &HashMap<String, serde_json::Value>,
    ) -> PluginResult {
        let value = ctx
            .message
            .get(self.value_key())
            .and_then(|v| v.as_str())
            .map(String::from);

        let value = match value {
            Some(v) => v,
            None => {
                let msg = format!(
                    "The request is rejected, please check the {} for this request",
                    self.type_label()
                );
                return self.reject(ctx, 401, msg);
            }
        };

        let default_reject_msg = || {
            self.rejected_msg
                .clone()
                .unwrap_or_else(|| format!("The {} is forbidden.", self.type_label()))
        };

        // Blacklist first.
        if !self.blacklist.is_empty() && self.blacklist.contains(&value) {
            return self.reject(ctx, self.rejected_code, default_reject_msg());
        }

        // Whitelist.
        let mut whitelisted = false;
        if !self.whitelist.is_empty() {
            whitelisted = self.whitelist.contains(&value);
            if !whitelisted {
                return self.reject(ctx, self.rejected_code, default_reject_msg());
            }
        }

        // Per-consumer method restriction (skipped when already whitelisted).
        if !self.allowed_by_methods.is_empty() && !whitelisted {
            let method = ctx.request.method.to_uppercase();
            let entry = self.allowed_by_methods.iter().find(|e| e.user == value);
            if let Some(entry) = entry {
                if !entry.methods.contains(&method) {
                    return self.reject(ctx, self.rejected_code, default_reject_msg());
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

    fn ctx(method: &str, consumer_name: Option<&str>, group: Option<&str>) -> Context {
        let mut message = HashMap::new();
        if let Some(n) = consumer_name {
            message.insert("consumer.name".to_string(), serde_json::json!(n));
        }
        if let Some(g) = group {
            message.insert("consumer.group".to_string(), serde_json::json!(g));
        }
        Context {
            request: GatewayRequest {
                method: method.to_string(),
                path: "/".to_string(),
                host: "h".to_string(),
                scheme: "http".to_string(),
                headers: HashMap::new(),
                query_params: HashMap::new(),
                body: Bytes::new(),
                remote_addr: "1.2.3.4:5".to_string(),
                protocol: Protocol::Http1,
            },
            response: GatewayResponse {
                status_code: 0,
                headers: HashMap::new(),
                body: Bytes::new(),
            },
            message,
            errors: Vec::new(),
        }
    }

    fn plugin(config: serde_json::Value) -> ConsumerRestrictionPlugin {
        let map: HashMap<String, serde_json::Value> = serde_json::from_value(config).unwrap();
        ConsumerRestrictionPlugin::from_config(&map).unwrap()
    }

    #[tokio::test]
    async fn test_consumer_restriction_whitelist() {
        let p = plugin(serde_json::json!({ "whitelist": ["alice", "bob"] }));
        assert!(p
            .execute(ctx("GET", Some("alice"), None), &HashMap::new())
            .await
            .is_ok());
        let err = p
            .execute(ctx("GET", Some("mallory"), None), &HashMap::new())
            .await
            .unwrap_err();
        assert_eq!(err.error.code, "CONSUMER_RESTRICTED");
        assert_eq!(err.context.response.status_code, 403);
    }

    #[tokio::test]
    async fn test_consumer_restriction_blacklist() {
        let p = plugin(serde_json::json!({ "blacklist": ["mallory"] }));
        assert!(p
            .execute(ctx("GET", Some("alice"), None), &HashMap::new())
            .await
            .is_ok());
        assert!(p
            .execute(ctx("GET", Some("mallory"), None), &HashMap::new())
            .await
            .is_err());
    }

    #[tokio::test]
    async fn test_consumer_restriction_group() {
        let p =
            plugin(serde_json::json!({ "type": "consumer_group_id", "whitelist": ["partners"] }));
        assert!(p
            .execute(ctx("GET", Some("alice"), Some("partners")), &HashMap::new())
            .await
            .is_ok());
        assert!(p
            .execute(ctx("GET", Some("alice"), Some("randoms")), &HashMap::new())
            .await
            .is_err());
    }

    #[tokio::test]
    async fn test_no_consumer_attached_401() {
        let p = plugin(serde_json::json!({ "whitelist": ["alice"] }));
        let err = p
            .execute(ctx("GET", None, None), &HashMap::new())
            .await
            .unwrap_err();
        assert_eq!(err.context.response.status_code, 401);
        assert_eq!(err.error.code, "CONSUMER_RESTRICTED");
    }

    #[tokio::test]
    async fn test_allowed_by_methods() {
        let p = plugin(serde_json::json!({
            "allowed_by_methods": [{ "user": "alice", "methods": ["GET"] }]
        }));
        // alice restricted to GET
        assert!(p
            .execute(ctx("GET", Some("alice"), None), &HashMap::new())
            .await
            .is_ok());
        assert!(p
            .execute(ctx("POST", Some("alice"), None), &HashMap::new())
            .await
            .is_err());
        // bob has no entry -> unrestricted
        assert!(p
            .execute(ctx("DELETE", Some("bob"), None), &HashMap::new())
            .await
            .is_ok());
    }

    #[tokio::test]
    async fn test_whitelist_bypasses_method_check() {
        let p = plugin(serde_json::json!({
            "whitelist": ["alice"],
            "allowed_by_methods": [{ "user": "alice", "methods": ["GET"] }]
        }));
        // whitelisted -> method restriction skipped
        assert!(p
            .execute(ctx("POST", Some("alice"), None), &HashMap::new())
            .await
            .is_ok());
    }

    #[test]
    fn test_config_validation() {
        // service_id rejected
        let map: HashMap<String, serde_json::Value> =
            serde_json::from_value(serde_json::json!({ "type": "service_id", "whitelist": ["x"] }))
                .unwrap();
        assert!(ConsumerRestrictionPlugin::from_config(&map).is_err());

        // both lists rejected
        let map: HashMap<String, serde_json::Value> =
            serde_json::from_value(serde_json::json!({ "whitelist": ["a"], "blacklist": ["b"] }))
                .unwrap();
        assert!(ConsumerRestrictionPlugin::from_config(&map).is_err());

        // nothing configured rejected
        let map: HashMap<String, serde_json::Value> =
            serde_json::from_value(serde_json::json!({})).unwrap();
        assert!(ConsumerRestrictionPlugin::from_config(&map).is_err());
    }
}
