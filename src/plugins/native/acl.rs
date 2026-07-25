//! Group-based access control plugin (`acl`).
//!
//! Restricts which already-authenticated consumers may reach a route, keyed on
//! the consumer's *group*. It does not authenticate — an upstream auth node
//! (e.g. `key-auth`) must have attached the consumer identity first, writing
//! `consumer.name` and (optionally) `consumer.group` into `context.message`.
//! This plugin then admits or blocks the request based on that group.
//!
//! APISIX 3.17's `acl` matches arbitrary consumer *labels* (`allow_labels` /
//! `deny_labels`, each a map of label→values). featherbit models a consumer's
//! membership as a single `consumer.group`, so this port implements the
//! classic group-allowlist form: `allowed_by` / `denied_by` are lists of group
//! names. See the deviation note in the docs.

use async_trait::async_trait;
use bytes::Bytes;
use std::collections::HashMap;

use crate::context::{Context, GatewayError};
use crate::plugins::{Plugin, PluginExecutionError, PluginOutput, PluginResult};

/// Admits or blocks requests based on the attached consumer's group.
///
/// Evaluation: with no consumer attached the request is rejected `401`. The
/// `denied_by` list is checked first (deny wins) — a consumer whose group is
/// listed is rejected. Then, when `allowed_by` is non-empty, a consumer whose
/// group is not listed (including a consumer with no group) is rejected. List
/// rejections use `rejected_code` (default `403`) and carry error code
/// `ACL_DENIED`.
pub struct AclPlugin {
    /// Groups permitted through; when non-empty, all other groups are rejected.
    allowed_by: Vec<String>,
    /// Groups blocked; a match rejects regardless of `allowed_by`.
    denied_by: Vec<String>,
    /// HTTP status used for list rejections.
    rejected_code: u16,
    /// Optional custom rejection message.
    rejected_msg: Option<String>,
}

impl AclPlugin {
    /// Builds the plugin from node config, failing fast when neither list is set.
    ///
    /// Accepted keys:
    /// - `allowed_by` (array of strings): consumer groups permitted through.
    ///   When non-empty, a consumer whose group is not listed is rejected.
    /// - `denied_by` (array of strings): consumer groups blocked. Checked
    ///   before `allowed_by` — deny wins.
    /// - At least one of `allowed_by` / `denied_by` is required.
    /// - `rejected_code` (integer, default `403`): status for list rejections.
    /// - `rejected_msg` (string, optional): custom rejection message.
    ///
    /// ```yaml
    /// type: acl
    /// config:
    ///   allowed_by: ["partners", "internal"]
    ///   denied_by: ["banned"]
    ///   rejected_code: 403
    /// ```
    pub fn from_config(config: &HashMap<String, serde_json::Value>) -> Result<Self, String> {
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

        let allowed_by = string_list("allowed_by");
        let denied_by = string_list("denied_by");

        if allowed_by.is_empty() && denied_by.is_empty() {
            return Err("acl: at least one of 'allowed_by' or 'denied_by' is required".to_string());
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
            allowed_by,
            denied_by,
            rejected_code,
            rejected_msg,
        })
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
                code: "ACL_DENIED".to_string(),
                message,
                metadata: HashMap::new(),
            },
        })
    }
}

#[async_trait]
impl Plugin for AclPlugin {
    fn plugin_type(&self) -> &str {
        "acl"
    }

    async fn execute(
        &self,
        ctx: Context,
        _named_inputs: &HashMap<String, serde_json::Value>,
    ) -> PluginResult {
        // No consumer attached at all -> authentication is missing.
        if ctx
            .message
            .get("consumer.name")
            .and_then(|v| v.as_str())
            .is_none()
        {
            return self.reject(ctx, 401, "Missing authentication.".to_string());
        }

        let group = ctx
            .message
            .get("consumer.group")
            .and_then(|v| v.as_str())
            .map(String::from);

        let reject_msg = || {
            self.rejected_msg
                .clone()
                .unwrap_or_else(|| "The consumer is forbidden.".to_string())
        };

        // Deny wins.
        if let Some(ref g) = group {
            if self.denied_by.contains(g) {
                return self.reject(ctx, self.rejected_code, reject_msg());
            }
        }

        // Allowlist: a consumer with no group, or a group not listed, is rejected.
        if !self.allowed_by.is_empty() {
            let allowed = group.as_ref().is_some_and(|g| self.allowed_by.contains(g));
            if !allowed {
                return self.reject(ctx, self.rejected_code, reject_msg());
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

    fn ctx(consumer_name: Option<&str>, group: Option<&str>) -> Context {
        let mut message = HashMap::new();
        if let Some(n) = consumer_name {
            message.insert("consumer.name".to_string(), serde_json::json!(n));
        }
        if let Some(g) = group {
            message.insert("consumer.group".to_string(), serde_json::json!(g));
        }
        Context {
            request: GatewayRequest {
                method: "GET".to_string(),
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

    fn plugin(config: serde_json::Value) -> AclPlugin {
        let map: HashMap<String, serde_json::Value> = serde_json::from_value(config).unwrap();
        AclPlugin::from_config(&map).unwrap()
    }

    #[tokio::test]
    async fn test_allowed_by() {
        let p = plugin(serde_json::json!({ "allowed_by": ["partners"] }));
        assert!(p
            .execute(ctx(Some("alice"), Some("partners")), &HashMap::new())
            .await
            .is_ok());
        // group not in allowlist
        let err = p
            .execute(ctx(Some("alice"), Some("randoms")), &HashMap::new())
            .await
            .unwrap_err();
        assert_eq!(err.error.code, "ACL_DENIED");
        assert_eq!(err.context.response.status_code, 403);
        // consumer with no group is rejected under an allowlist
        assert!(p
            .execute(ctx(Some("alice"), None), &HashMap::new())
            .await
            .is_err());
    }

    #[tokio::test]
    async fn test_denied_by_wins() {
        let p = plugin(serde_json::json!({
            "allowed_by": ["partners", "banned"],
            "denied_by": ["banned"]
        }));
        assert!(p
            .execute(ctx(Some("alice"), Some("partners")), &HashMap::new())
            .await
            .is_ok());
        // in allowlist but also denied -> deny wins
        assert!(p
            .execute(ctx(Some("bob"), Some("banned")), &HashMap::new())
            .await
            .is_err());
    }

    #[tokio::test]
    async fn test_denied_by_only() {
        let p = plugin(serde_json::json!({ "denied_by": ["banned"] }));
        // no allowlist: everything not denied passes, incl. groupless consumers
        assert!(p
            .execute(ctx(Some("alice"), None), &HashMap::new())
            .await
            .is_ok());
        assert!(p
            .execute(ctx(Some("bob"), Some("banned")), &HashMap::new())
            .await
            .is_err());
    }

    #[tokio::test]
    async fn test_no_consumer_attached_401() {
        let p = plugin(serde_json::json!({ "allowed_by": ["partners"] }));
        let err = p
            .execute(ctx(None, None), &HashMap::new())
            .await
            .unwrap_err();
        assert_eq!(err.context.response.status_code, 401);
        assert_eq!(err.error.code, "ACL_DENIED");
    }

    #[test]
    fn test_requires_a_list() {
        let map: HashMap<String, serde_json::Value> =
            serde_json::from_value(serde_json::json!({})).unwrap();
        assert!(AclPlugin::from_config(&map).is_err());
    }
}
