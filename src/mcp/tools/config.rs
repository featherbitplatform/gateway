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
    /// The policy definition `{nodes: [...], edges: [...], error_handler?}`, as a
    /// JSON object or a YAML document string. `name` inside it is optional.
    #[serde(default, alias = "definition")]
    pub policy: Option<Value>,
    /// Optional policy name (only used in messages; the definition is not saved).
    #[serde(default)]
    pub name: Option<String>,
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
///
/// The result's `buffering` array names every upstream the policy forces to
/// buffer instead of stream, and the node responsible for each — the same
/// information the Admin API's `POST /api/policies/validate` reports to a
/// human, so an agent driving the gateway sees it too. A policy that forces
/// buffering is still `valid`; `buffering` is informational, not an error.
pub async fn validate_policy(
    state: &SharedState,
    a: ValidatePolicyArgs,
) -> Result<Value, ToolError> {
    // Accept the definition under `policy` or `definition`, as an object or a
    // YAML string, with or without a `name` — validation does not save it, so
    // forcing a name only made agents trip on "missing field `name`".
    let mut raw = a.policy.ok_or_else(|| {
        ToolError::invalid_payload("validate_policy needs `policy` (or `definition`): the policy as a JSON object or YAML string")
    })?;
    if let Value::String(yaml) = &raw {
        raw = serde_yaml::from_str(yaml)
            .map_err(|e| ToolError::invalid_payload(format!("policy: YAML did not parse: {e}")))?;
    }
    if let Some(obj) = raw.as_object_mut() {
        if !obj.contains_key("name") {
            let name = a
                .name
                .clone()
                .unwrap_or_else(|| "unsaved-policy".to_string());
            obj.insert("name".to_string(), Value::String(name));
        }
    }
    let policy: PolicyConfig = parse_payload(raw, "policy")?;
    let (supernodes, plugin_configs) = {
        let gw = state.gateway.read().await;
        (gw.supernodes.clone(), gw.plugin_configs.clone())
    };
    let compiled = crate::graph::prepare_policy(policy, &supernodes, &plugin_configs)
        .and_then(|p| crate::graph::compile_policy(&p, state.resources.clone()));
    let cache_pairs: Value = match &compiled {
        Ok(graph) => serde_json::to_value(graph.cache_pair_warnings())
            .expect("CachePairWarning always serializes"),
        Err(_) => serde_json::json!([]),
    };
    let (errors, buffering): (Vec<String>, Value) = match compiled {
        Ok(graph) => (
            Vec::new(),
            serde_json::to_value(graph.buffering_reasons())
                .expect("BufferingReason always serializes"),
        ),
        Err(e) => (
            e.split("; ").map(str::to_string).collect(),
            serde_json::json!([]),
        ),
    };
    Ok(serde_json::json!({
        "valid": errors.is_empty(),
        "errors": errors,
        "buffering": buffering,
        "cache_pairs": cache_pairs
    }))
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
    async fn validate_policy_accepts_agent_shapes() {
        let s = state("{}", ECHO_GATEWAY);
        let nameless = serde_json::json!({
            "nodes": [{"id": "l", "type": "listener"}, {"id": "e", "type": "echo", "config": {"body": "hi"}}, {"id": "c", "type": "client"}],
            "edges": [{"from": "l.out", "to": "e.in"}, {"from": "e.out", "to": "c.in"}]
        });
        // No `name` inside the definition.
        let v = call(
            &s,
            "validate_policy",
            obj(serde_json::json!({"policy": nameless})),
        )
        .await
        .unwrap();
        assert_eq!(v["valid"], true, "{v}");
        // `definition` as the argument key, YAML string payload.
        let yaml = serde_yaml::to_string(&nameless).unwrap();
        let v = call(
            &s,
            "validate_policy",
            obj(serde_json::json!({"definition": yaml})),
        )
        .await
        .unwrap();
        assert_eq!(v["valid"], true, "{v}");
        // Neither key: an invalid_input with the shape hint.
        let err = call(&s, "validate_policy", obj(serde_json::json!({"nodes": []})))
            .await
            .unwrap_err();
        assert_eq!(err.code, "invalid_input");
        assert!(
            err.hint.as_deref().unwrap_or("").contains("definition"),
            "{err:?}"
        );
    }

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

    /// Same shape the Admin API's `POST /api/policies/validate` reports:
    /// an agent driving the gateway must see which node forces an upstream
    /// to buffer, not just a bare `valid: true`.
    #[tokio::test]
    async fn validate_policy_reports_forced_buffering() {
        let s = state("{}", ECHO_GATEWAY);
        let policy = serde_json::json!({
            "nodes": [
                { "id": "listener", "type": "listener", "config": {} },
                { "id": "up", "type": "upstream",
                  "config": { "targets": [{ "host": "h", "port": 80 }] } },
                { "id": "rw", "type": "response-rewrite",
                  "config": { "filters": [{ "regex": "a", "replace": "b" }] } },
                { "id": "client", "type": "client", "config": {} }
            ],
            "edges": [
                { "from": "listener.out", "to": "up.in" },
                { "from": "up.success", "to": "rw.in" },
                { "from": "rw.success", "to": "client.in" }
            ]
        });
        let v = call(
            &s,
            "validate_policy",
            obj(serde_json::json!({"policy": policy})),
        )
        .await
        .unwrap();
        assert_eq!(v["valid"], true, "{v}");
        assert_eq!(v["buffering"][0]["upstream"], "up");
        assert_eq!(v["buffering"][0]["blocked_by"], "rw");
        assert_eq!(v["buffering"][0]["node_type"], "response-rewrite");
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
