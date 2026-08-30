//! Read tools over the stored gateway config, plus standalone validation.

use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

use super::{parse_payload, ToolError};
use crate::config::{PolicyConfig, SupernodeConfig};
use crate::state::SharedState;

/// `{ "name": "<resource name>" }`
#[derive(Debug, Deserialize, JsonSchema)]
pub struct NameArgs {
    pub name: String,
}

/// `{ "policy": <object | YAML string> }`
#[derive(Debug, Deserialize, JsonSchema)]
pub struct ValidatePolicyArgs {
    /// The policy definition, as a JSON object or a YAML document string.
    pub policy: Value,
}

/// `{ "definition": <object | YAML string> }`
#[derive(Debug, Deserialize, JsonSchema)]
pub struct ValidateSupernodeArgs {
    /// The supernode definition, as a JSON object or a YAML document string.
    pub definition: Value,
}

fn json<T: serde::Serialize>(v: &T) -> Result<Value, ToolError> {
    serde_json::to_value(v).map_err(|e| ToolError::internal(e.to_string()))
}

pub async fn list_routes(state: &SharedState) -> Result<Value, ToolError> {
    let gw = state.gateway.read().await;
    Ok(serde_json::json!({ "routes": json(&gw.routes)? }))
}

pub async fn get_route(state: &SharedState, a: NameArgs) -> Result<Value, ToolError> {
    let gw = state.gateway.read().await;
    let r = gw
        .routes
        .iter()
        .find(|r| r.name == a.name)
        .ok_or_else(|| ToolError::not_found("route", &a.name))?;
    json(r)
}

pub async fn list_policies(state: &SharedState) -> Result<Value, ToolError> {
    let gw = state.gateway.read().await;
    Ok(serde_json::json!({ "policies": json(&gw.policies)? }))
}

pub async fn get_policy(state: &SharedState, a: NameArgs) -> Result<Value, ToolError> {
    let gw = state.gateway.read().await;
    let p = gw
        .policies
        .iter()
        .find(|p| p.name == a.name)
        .ok_or_else(|| ToolError::not_found("policy", &a.name))?;
    let referenced_by: Vec<&str> = gw
        .routes
        .iter()
        .filter(|r| r.policy == a.name)
        .map(|r| r.name.as_str())
        .collect();
    Ok(serde_json::json!({ "policy": json(p)?, "referenced_by_routes": referenced_by }))
}

pub async fn list_supernodes(state: &SharedState) -> Result<Value, ToolError> {
    let gw = state.gateway.read().await;
    Ok(serde_json::json!({ "supernodes": json(&gw.supernodes)? }))
}

pub async fn get_supernode(state: &SharedState, a: NameArgs) -> Result<Value, ToolError> {
    let gw = state.gateway.read().await;
    let s = gw
        .supernodes
        .iter()
        .find(|s| s.name == a.name)
        .ok_or_else(|| ToolError::not_found("supernode", &a.name))?;
    let used_by: Vec<&str> = gw
        .policies
        .iter()
        .filter(|p| {
            p.nodes.iter().any(|n| {
                n.node_type == "supernode"
                    && n.config.get("name").and_then(Value::as_str) == Some(a.name.as_str())
            })
        })
        .map(|p| p.name.as_str())
        .collect();
    Ok(serde_json::json!({ "supernode": json(s)?, "used_by_policies": used_by }))
}

pub async fn list_plugin_configs(state: &SharedState) -> Result<Value, ToolError> {
    let gw = state.gateway.read().await;
    Ok(serde_json::json!({ "plugin_configs": json(&gw.plugin_configs)? }))
}

pub async fn get_plugin_config(state: &SharedState, a: NameArgs) -> Result<Value, ToolError> {
    let gw = state.gateway.read().await;
    let pc = gw
        .plugin_configs
        .iter()
        .find(|p| p.name == a.name)
        .ok_or_else(|| ToolError::not_found("plugin config", &a.name))?;
    json(pc)
}

pub async fn list_stores(state: &SharedState) -> Result<Value, ToolError> {
    let gw = state.gateway.read().await;
    Ok(serde_json::json!({ "stores": json(&gw.stores)? }))
}

pub async fn list_consumers(state: &SharedState) -> Result<Value, ToolError> {
    let gw = state.gateway.read().await;
    let masked: Vec<_> = gw
        .consumers
        .iter()
        .map(crate::consumers::mask_credentials)
        .collect();
    Ok(serde_json::json!({ "consumers": json(&masked)?, "note": "credential secrets are masked" }))
}

pub async fn get_consumer(state: &SharedState, a: NameArgs) -> Result<Value, ToolError> {
    let gw = state.gateway.read().await;
    let c = gw
        .consumers
        .iter()
        .find(|c| c.name == a.name)
        .ok_or_else(|| ToolError::not_found("consumer", &a.name))?;
    json(&crate::consumers::mask_credentials(c))
}

/// Validates + compiles a policy against the live supernodes, plugin configs
/// and stores, without persisting. Mirrors what `put_policy(dry_run)` checks
/// for the policy itself (cross-references from routes are not included).
pub async fn validate_policy(
    state: &SharedState,
    a: ValidatePolicyArgs,
) -> Result<Value, ToolError> {
    let policy: PolicyConfig = parse_payload(a.policy, "policy")?;
    let (supernodes, plugin_configs) = {
        let gw = state.gateway.read().await;
        (gw.supernodes.clone(), gw.plugin_configs.clone())
    };
    let errors: Vec<String> =
        match crate::graph::prepare_policy(policy, &supernodes, &plugin_configs)
            .and_then(|p| crate::graph::compile_policy(&p, state.resources.clone()).map(|_| ()))
        {
            Ok(()) => Vec::new(),
            Err(e) => e.split("; ").map(str::to_string).collect(),
        };
    Ok(serde_json::json!({ "valid": errors.is_empty(), "errors": errors }))
}

/// Structural validation of a supernode definition.
pub async fn validate_supernode(a: ValidateSupernodeArgs) -> Result<Value, ToolError> {
    let def: SupernodeConfig = parse_payload(a.definition, "definition")?;
    let errors = crate::graph::validate_supernode(&def)
        .err()
        .unwrap_or_default();
    Ok(serde_json::json!({ "valid": errors.is_empty(), "errors": errors }))
}

#[cfg(test)]
mod tests {
    use crate::mcp::tools::call;
    use crate::mcp::tools::test_support::{obj, state, ECHO_GATEWAY};

    #[tokio::test]
    async fn reads_mirror_config_and_report_not_found() {
        let s = state("{}", ECHO_GATEWAY);
        let v = call(&s, "list_routes", obj(serde_json::json!({})))
            .await
            .unwrap();
        assert_eq!(v["routes"][0]["name"], "hello");
        assert_eq!(v["routes"][0]["match"]["path"], "/hello");
        let v = call(
            &s,
            "get_policy",
            obj(serde_json::json!({"name": "echo-policy"})),
        )
        .await
        .unwrap();
        assert_eq!(v["referenced_by_routes"][0], "hello");
        assert_eq!(v["policy"]["nodes"].as_array().unwrap().len(), 3);
        let err = call(&s, "get_route", obj(serde_json::json!({"name": "nope"})))
            .await
            .unwrap_err();
        assert_eq!(err.code, "not_found");
        assert!(err.message.contains("route 'nope'"));
    }

    #[tokio::test]
    async fn consumers_are_masked() {
        let gw = format!(
            "{ECHO_GATEWAY}\nconsumers:\n  - name: alice\n    credentials:\n      key-auth: {{ key: topsecret }}\n"
        );
        let s = state("{}", &gw);
        let v = call(&s, "list_consumers", obj(serde_json::json!({})))
            .await
            .unwrap();
        assert_eq!(
            v["consumers"][0]["credentials"]["key-auth"]["key"],
            "<masked>"
        );
        let v = call(
            &s,
            "get_consumer",
            obj(serde_json::json!({"name": "alice"})),
        )
        .await
        .unwrap();
        assert_eq!(v["credentials"]["key-auth"]["key"], "<masked>");
    }

    #[tokio::test]
    async fn validate_policy_reports_unwired_port_and_accepts_yaml() {
        let s = state("{}", ECHO_GATEWAY);
        let bad = "name: p\nnodes:\n  - {id: l, type: listener}\n  - {id: k, type: key-auth, config: {keys: [k1]}}\n  - {id: c, type: client}\nedges:\n  - {from: l.out, to: k.in}\n  - {from: k.out, to: c.in}\n";
        let v = call(
            &s,
            "validate_policy",
            obj(serde_json::json!({"policy": bad})),
        )
        .await
        .unwrap();
        assert_eq!(v["valid"], false);
        let errors = v["errors"].as_array().unwrap();
        assert!(
            errors
                .iter()
                .any(|e| e.as_str().unwrap().contains("denied")),
            "{errors:?}"
        );

        let good = serde_json::json!({"name": "p", "nodes": [
            {"id": "l", "type": "listener"}, {"id": "e", "type": "echo", "config": {"body": "hi"}}, {"id": "c", "type": "client"}],
            "edges": [{"from": "l.out", "to": "e.in"}, {"from": "e.out", "to": "c.in"}]});
        let v = call(
            &s,
            "validate_policy",
            obj(serde_json::json!({"policy": good})),
        )
        .await
        .unwrap();
        assert_eq!(v["valid"], true);
    }

    #[tokio::test]
    async fn validate_supernode_structural() {
        let s = state("{}", "{}");
        let def = "name: sn\nnodes:\n  - {id: input, type: input}\n  - {id: e, type: echo, config: {body: hi}}\nedges:\n  - {from: input.out, to: e.in}\n";
        let v = call(
            &s,
            "validate_supernode",
            obj(serde_json::json!({"definition": def})),
        )
        .await
        .unwrap();
        assert_eq!(v["valid"], false, "{v}");
        assert!(!v["errors"].as_array().unwrap().is_empty());
    }
}
