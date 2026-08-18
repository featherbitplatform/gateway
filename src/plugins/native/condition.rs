//! The `condition` node — a pure branching waypoint. Evaluates a boolean
//! condition expression against the context and routes the request through
//! the `true` or `false` outcome port; a condition that cannot actually be
//! checked (absent variable under a comparison, JSONPath over a non-JSON
//! body) exits through `error` instead of guessing.
//!
//! The expression grammar is shared with `request-validation`
//! ([`crate::vars::Expr`]): top-level rules ANDed, nested `AND`/`OR`/`NOT`
//! groups, variable and JSONPath subjects. The node never mutates the
//! request or response.

use async_trait::async_trait;
use std::collections::HashMap;

use crate::context::{Context, GatewayError};
use crate::plugins::{Plugin, PluginExecutionError, PluginOutput, PluginResult};

/// Routes the context through `true` or `false` depending on a compiled
/// condition expression; an uncheckable condition becomes a
/// `CONDITION_UNCHECKABLE` execution error routed along the `error` edge.
#[derive(Debug)]
pub struct ConditionPlugin {
    conditions: crate::vars::Expr,
}

impl ConditionPlugin {
    /// Builds the plugin from node config.
    ///
    /// Accepted keys:
    /// - `conditions` (array, required, non-empty): a condition expression
    ///   (see [`crate::vars::Expr`]) — rules ANDed at top level, nested
    ///   `AND`/`OR`/`NOT` groups, variable and JSONPath body subjects.
    ///
    /// ```yaml
    /// type: condition
    /// config:
    ///   conditions:
    ///     - ["$.user.tier", "==", "premium"]
    /// ```
    pub fn from_config(config: &HashMap<String, serde_json::Value>) -> Result<Self, String> {
        let raw = config
            .get("conditions")
            .ok_or("condition: 'conditions' is required")?;
        if raw.as_array().is_some_and(|rules| rules.is_empty()) {
            return Err("condition: 'conditions' must not be empty".to_string());
        }
        let conditions = crate::vars::Expr::parse(raw)
            .map_err(|e| format!("condition: invalid 'conditions': {}", e))?;
        Ok(Self { conditions })
    }
}

#[async_trait]
impl Plugin for ConditionPlugin {
    fn plugin_type(&self) -> &str {
        "condition"
    }

    async fn execute(&self, ctx: Context) -> PluginResult {
        match self.conditions.try_eval(&ctx) {
            Ok(true) => Ok(PluginOutput::on_port(ctx, "true")),
            Ok(false) => Ok(PluginOutput::on_port(ctx, "false")),
            Err(reason) => Err(PluginExecutionError {
                context: ctx,
                error: GatewayError {
                    node_id: String::new(),
                    code: "CONDITION_UNCHECKABLE".to_string(),
                    message: format!("condition could not be checked: {}", reason),
                    metadata: HashMap::new(),
                },
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::context::{Context, GatewayRequest, GatewayResponse, Protocol};
    use crate::plugins::native::condition::ConditionPlugin;
    use crate::plugins::Plugin;
    use bytes::Bytes;
    use std::collections::HashMap;

    fn test_context(body: &str) -> Context {
        let mut headers = HashMap::new();
        headers.insert("x-tier".to_string(), vec!["premium".to_string()]);
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

    fn plugin(conditions: serde_json::Value) -> ConditionPlugin {
        let mut config = HashMap::new();
        config.insert("conditions".to_string(), conditions);
        ConditionPlugin::from_config(&config).unwrap()
    }

    #[tokio::test]
    async fn test_condition_true_branch() {
        let p = plugin(serde_json::json!([["http_x_tier", "==", "premium"]]));
        let out = p.execute(test_context("")).await.unwrap();
        assert_eq!(out.port, Some("true"));
    }

    #[tokio::test]
    async fn test_condition_false_branch() {
        let p = plugin(serde_json::json!([["http_x_tier", "==", "basic"]]));
        let out = p.execute(test_context("")).await.unwrap();
        assert_eq!(out.port, Some("false"));
    }

    #[tokio::test]
    async fn test_condition_jsonpath_branches() {
        let p = plugin(serde_json::json!([["$.user.tier", "==", "premium"]]));
        let out = p
            .execute(test_context(r#"{"user":{"tier":"premium"}}"#))
            .await
            .unwrap();
        assert_eq!(out.port, Some("true"));

        let out = p
            .execute(test_context(r#"{"user":{"tier":"basic"}}"#))
            .await
            .unwrap();
        assert_eq!(out.port, Some("false"));
    }

    #[tokio::test]
    async fn test_condition_uncheckable_var_is_error() {
        let p = plugin(serde_json::json!([["http_missing", "==", "x"]]));
        let err = p.execute(test_context("")).await.unwrap_err();
        assert_eq!(err.error.code, "CONDITION_UNCHECKABLE");
        assert!(
            err.error.message.contains("http_missing"),
            "{}",
            err.error.message
        );
        // the context comes back untouched for the error edge
        assert_eq!(err.context.request.path, "/api");
    }

    #[tokio::test]
    async fn test_condition_uncheckable_body_is_error() {
        let p = plugin(serde_json::json!([["$.user.tier", "==", "premium"]]));
        let err = p.execute(test_context("not json")).await.unwrap_err();
        assert_eq!(err.error.code, "CONDITION_UNCHECKABLE");
        assert!(
            err.error.message.contains("request body"),
            "{}",
            err.error.message
        );
    }

    #[tokio::test]
    async fn test_condition_existence_test_on_absent_var_is_checked() {
        // `absent`/`present` legitimately ask about absence: no error
        let p = plugin(serde_json::json!([["http_missing", "absent"]]));
        let out = p.execute(test_context("")).await.unwrap();
        assert_eq!(out.port, Some("true"));

        let p = plugin(serde_json::json!([["http_missing", "present"]]));
        let out = p.execute(test_context("")).await.unwrap();
        assert_eq!(out.port, Some("false"));
    }

    #[tokio::test]
    async fn test_condition_does_not_mutate_context() {
        let p = plugin(serde_json::json!([["$.user.tier", "==", "premium"]]));
        let body = r#"{"user":  {"tier": "premium"}}"#;
        let out = p.execute(test_context(body)).await.unwrap();
        // unlike request-validation, the body is not re-serialized
        assert_eq!(out.context.request.body, Bytes::from(body));
        assert_eq!(out.context.response.status_code, 0);
        assert!(out.context.response.body.is_empty());
    }

    #[test]
    fn test_condition_config_rejections() {
        // 'conditions' is required
        assert!(ConditionPlugin::from_config(&HashMap::new()).is_err());

        // malformed conditions fail at config load with a plugin-prefixed message
        let mut config = HashMap::new();
        config.insert(
            "conditions".to_string(),
            serde_json::json!([["$.a", "bogus_op", 1]]),
        );
        let err = ConditionPlugin::from_config(&config).unwrap_err();
        assert!(err.starts_with("condition: invalid 'conditions'"), "{err}");

        // an empty rule list would branch unconditionally — reject it
        let mut config = HashMap::new();
        config.insert("conditions".to_string(), serde_json::json!([]));
        assert!(ConditionPlugin::from_config(&config).is_err());
    }

    #[test]
    fn test_condition_registered() {
        assert!(crate::plugins::KNOWN_PLUGIN_TYPES.contains(&"condition"));

        let mut config = HashMap::new();
        config.insert(
            "conditions".to_string(),
            serde_json::json!([["http_x", "present"]]),
        );
        let p = crate::plugins::create_plugin(
            "condition",
            &config,
            &crate::plugins::resources::PluginResources::empty(),
        )
        .unwrap();
        assert_eq!(p.plugin_type(), "condition");

        let spec = crate::plugins::port_spec("condition").unwrap();
        let names: Vec<&str> = spec.outputs.iter().map(|p| p.name).collect();
        assert_eq!(names, vec!["true", "false", "error"]);
    }
}
