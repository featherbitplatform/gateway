//! Read tools over static knowledge: node types, vars, status, config export.

use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

use super::ToolError;
use crate::state::SharedState;

/// Tools that take no arguments.
#[derive(Debug, Default, Deserialize, JsonSchema)]
pub struct NoArgs {}

/// `{ "type": "<node type>" }`
#[derive(Debug, Deserialize, JsonSchema)]
pub struct TypeArgs {
    /// Node type name as it appears in YAML `type:` (e.g. `limit-count`).
    #[serde(rename = "type")]
    pub node_type: String,
}

pub async fn list_node_types() -> Result<Value, ToolError> {
    Ok(serde_json::json!({ "node_types": crate::admin::policies::plugin_catalog() }))
}

pub async fn get_node_type(a: TypeArgs) -> Result<Value, ToolError> {
    let entry = crate::admin::policies::plugin_catalog()
        .into_iter()
        .find(|e| e["type"] == a.node_type)
        .ok_or_else(|| ToolError::not_found("node type", &a.node_type))?;
    let mut out = entry;
    out["docs"] = match crate::mcp::docs::plugin_page(&a.node_type) {
        Some(md) => Value::String(md),
        None => Value::Null,
    };
    Ok(out)
}

pub async fn list_vars() -> Result<Value, ToolError> {
    Ok(serde_json::json!({ "vars": crate::vars::catalog::var_catalog() }))
}

pub async fn get_status(state: &SharedState) -> Result<Value, ToolError> {
    let routes = state.routes.read().await.len();
    let gw = state.gateway.read().await;
    Ok(serde_json::json!({
        "version": env!("CARGO_PKG_VERSION"),
        "routes": routes,
        "policies": gw.policies.len(),
        "supernodes": gw.supernodes.len(),
        "stores": gw.stores.len(),
        "debug": { "enabled": state.debug.enabled, "sandbox": state.debug.sandbox_enabled },
    }))
}

pub async fn export_config(state: &SharedState) -> Result<Value, ToolError> {
    let gw = state.gateway.read().await;
    let yaml = serde_yaml::to_string(&*gw).map_err(|e| ToolError::internal(e.to_string()))?;
    Ok(serde_json::json!({ "format": "yaml", "content": yaml }))
}

#[cfg(test)]
mod tests {
    use crate::mcp::tools::call;
    use crate::mcp::tools::test_support::{obj, state, ECHO_GATEWAY};

    #[tokio::test]
    async fn node_types_and_lookup() {
        let s = state("{}", "{}");
        let v = call(&s, "list_node_types", obj(serde_json::json!({})))
            .await
            .unwrap();
        let types = v["node_types"].as_array().unwrap();
        assert!(types.iter().any(|t| t["type"] == "limit-count"));
        assert!(types[0]["ports"].is_object());

        let v = call(
            &s,
            "get_node_type",
            obj(serde_json::json!({"type": "condition"})),
        )
        .await
        .unwrap();
        let outs = v["ports"]["outputs"].as_array().unwrap();
        assert!(
            outs.iter().any(|p| p["name"] == "true") && outs.iter().any(|p| p["name"] == "false")
        );

        let err = call(
            &s,
            "get_node_type",
            obj(serde_json::json!({"type": "nope"})),
        )
        .await
        .unwrap_err();
        assert_eq!(err.code, "not_found");
        let err = call(&s, "get_node_type", obj(serde_json::json!({})))
            .await
            .unwrap_err();
        assert_eq!(err.code, "invalid_input");
    }

    #[tokio::test]
    async fn vars_status_export() {
        let s = state("debug:\n  enabled: true\n", ECHO_GATEWAY);
        let v = call(&s, "list_vars", obj(serde_json::json!({})))
            .await
            .unwrap();
        assert!(v["vars"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["name"] == "remote_addr"));
        let v = call(&s, "get_status", obj(serde_json::json!({})))
            .await
            .unwrap();
        assert_eq!(v["routes"], 1);
        assert_eq!(v["debug"]["enabled"], true);
        let v = call(&s, "export_config", obj(serde_json::json!({})))
            .await
            .unwrap();
        assert!(v["content"].as_str().unwrap().contains("echo-policy"));
    }
}
