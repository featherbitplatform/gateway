//! Compile-time resolution of shared plugin configs (`config_ref`).
//!
//! [`resolve_plugin_configs`] validates the `plugin_configs` definitions and
//! returns an in-memory copy of the gateway config where every node's
//! `config_ref` — in policies and supernode definitions alike — has been
//! materialized: effective config = shared config with the node's local keys
//! written over it (shallow, top-level, local wins), `config_ref` cleared.
//! Runs at the top of `SharedState::compile_routes` (before supernode
//! expansion, so instances inherit resolved inner configs) and in the debug
//! sandbox. The resolved copy is never stored: gateway.yaml, the Admin API,
//! etcd, and the export endpoint always keep the reference form.

use std::collections::HashMap;

use serde_json::Value;

use super::gateway::{GatewayConfig, NodeConfig, PluginConfigDef};

/// Types that may not be used as a shared config's `type`: pipeline endpoints,
/// supernode instances, and supernode boundary pseudo-nodes.
const RESERVED_TYPES: [&str; 6] = [
    "listener",
    "client",
    "supernode",
    "input",
    "output",
    "error",
];

/// Validates shared plugin configs and materializes every `config_ref`.
///
/// Definition rules: unique names; `type` must be a factory-known plugin type
/// and not a reserved type. Reference rules: the named def must exist and its
/// type must equal the node's type; `supernode` instance nodes cannot carry a
/// ref. All errors name the offending policy/supernode and node.
pub fn resolve_plugin_configs(gw: &GatewayConfig) -> Result<GatewayConfig, String> {
    // consumed by SharedState::compile_routes in Task 4
    let mut seen = std::collections::HashSet::new();
    for def in &gw.plugin_configs {
        if !seen.insert(def.name.as_str()) {
            return Err(format!("Duplicate plugin config name '{}'", def.name));
        }
        if RESERVED_TYPES.contains(&def.plugin_type.as_str()) {
            return Err(format!(
                "Plugin config '{}' uses reserved type '{}'",
                def.name, def.plugin_type
            ));
        }
        if !crate::plugins::KNOWN_PLUGIN_TYPES.contains(&def.plugin_type.as_str()) {
            return Err(format!(
                "Plugin config '{}' references unknown plugin type '{}'",
                def.name, def.plugin_type
            ));
        }
    }

    let by_name: HashMap<&str, &PluginConfigDef> = gw
        .plugin_configs
        .iter()
        .map(|d| (d.name.as_str(), d))
        .collect();

    let mut out = gw.clone();
    for policy in &mut out.policies {
        let ctx = format!("policy '{}'", policy.name);
        resolve_nodes(&mut policy.nodes, &by_name, &ctx)?;
    }
    for sn in &mut out.supernodes {
        let ctx = format!("supernode '{}'", sn.name);
        resolve_nodes(&mut sn.nodes, &by_name, &ctx)?;
    }
    Ok(out)
}

/// Materializes `config_ref` on each node in place: shared config first, then
/// the node's own keys written over it (local wins), ref cleared.
fn resolve_nodes(
    nodes: &mut [NodeConfig],
    by_name: &HashMap<&str, &PluginConfigDef>,
    ctx: &str,
) -> Result<(), String> {
    for node in nodes {
        let Some(ref_name) = node.config_ref.take() else {
            continue;
        };
        if node.node_type == "supernode" {
            return Err(format!(
                "{ctx}: supernode instance node '{}' cannot use config_ref",
                node.id
            ));
        }
        let def = by_name.get(ref_name.as_str()).ok_or_else(|| {
            format!(
                "{ctx}: node '{}' references unknown plugin config '{}'",
                node.id, ref_name
            )
        })?;
        if def.plugin_type != node.node_type {
            return Err(format!(
                "{ctx}: node '{}' ({}) references plugin config '{}' of type '{}'",
                node.id, node.node_type, ref_name, def.plugin_type
            ));
        }
        let mut merged: HashMap<String, Value> = def.config.clone();
        merged.extend(node.config.drain());
        node.config = merged;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{EdgeConfig, PolicyConfig, SupernodeConfig};
    use serde_json::json;

    fn def(name: &str, plugin_type: &str, config: serde_json::Value) -> PluginConfigDef {
        PluginConfigDef {
            name: name.into(),
            plugin_type: plugin_type.into(),
            description: None,
            config: serde_json::from_value(config).unwrap(),
        }
    }

    fn node(id: &str, ty: &str, config_ref: Option<&str>, config: serde_json::Value) -> NodeConfig {
        NodeConfig {
            id: id.into(),
            node_type: ty.into(),
            config: serde_json::from_value(config).unwrap(),
            config_ref: config_ref.map(String::from),
            position: None,
        }
    }

    fn gw_with(policy_nodes: Vec<NodeConfig>, defs: Vec<PluginConfigDef>) -> GatewayConfig {
        let mut gw: GatewayConfig = serde_yaml::from_str("{}").unwrap();
        gw.plugin_configs = defs;
        gw.policies = vec![PolicyConfig {
            name: "p".into(),
            error_handler: None,
            nodes: policy_nodes,
            edges: Vec::new(),
        }];
        gw
    }

    #[test]
    fn test_merge_local_wins_and_ref_cleared() {
        let gw = gw_with(
            vec![node(
                "auth",
                "openid-connect",
                Some("corp"),
                json!({"scope": "openid profile"}),
            )],
            vec![def(
                "corp",
                "openid-connect",
                json!({"client_id": "gw", "scope": "openid"}),
            )],
        );
        let out = resolve_plugin_configs(&gw).unwrap();
        let n = &out.policies[0].nodes[0];
        assert_eq!(n.config["client_id"], json!("gw")); // inherited
        assert_eq!(n.config["scope"], json!("openid profile")); // local wins
        assert!(
            n.config_ref.is_none(),
            "ref must be cleared in resolved copy"
        );
        // Source is untouched (pure function).
        assert_eq!(gw.policies[0].nodes[0].config_ref.as_deref(), Some("corp"));
    }

    #[test]
    fn test_no_ref_passthrough_unchanged() {
        let gw = gw_with(vec![node("a", "cors", None, json!({"x": 1}))], vec![]);
        let out = resolve_plugin_configs(&gw).unwrap();
        assert_eq!(out.policies[0].nodes[0].config["x"], json!(1));
    }

    #[test]
    fn test_unknown_ref_is_error() {
        let gw = gw_with(vec![node("a", "cors", Some("nope"), json!({}))], vec![]);
        let err = resolve_plugin_configs(&gw).unwrap_err();
        assert!(
            err.contains("unknown plugin config 'nope'") && err.contains("'a'"),
            "{err}"
        );
    }

    #[test]
    fn test_type_mismatch_is_error() {
        let gw = gw_with(
            vec![node("a", "cors", Some("corp"), json!({}))],
            vec![def("corp", "openid-connect", json!({}))],
        );
        let err = resolve_plugin_configs(&gw).unwrap_err();
        assert!(
            err.contains("'corp'") && err.contains("openid-connect") && err.contains("cors"),
            "{err}"
        );
    }

    #[test]
    fn test_supernode_definition_nodes_resolve() {
        let mut gw = gw_with(
            vec![],
            vec![def("m", "mocking", json!({"response_status": 200}))],
        );
        gw.supernodes = vec![SupernodeConfig {
            name: "sn".into(),
            description: None,
            nodes: vec![
                node("input", "input", None, json!({})),
                node("output", "output", None, json!({})),
                node("error", "error", None, json!({})),
                node(
                    "mock",
                    "mocking",
                    Some("m"),
                    json!({"content_type": "text/plain"}),
                ),
            ],
            edges: vec![
                EdgeConfig {
                    from: "input.out".into(),
                    to: "mock.in".into(),
                },
                EdgeConfig {
                    from: "mock.success".into(),
                    to: "output.in".into(),
                },
            ],
        }];
        let out = resolve_plugin_configs(&gw).unwrap();
        let inner = &out.supernodes[0].nodes[3];
        assert_eq!(inner.config["response_status"], json!(200));
        assert_eq!(inner.config["content_type"], json!("text/plain"));
        assert!(inner.config_ref.is_none());
    }

    #[test]
    fn test_supernode_instance_node_cannot_carry_ref() {
        let gw = gw_with(
            vec![node("sn", "supernode", Some("m"), json!({"name": "x"}))],
            vec![def("m", "mocking", json!({}))],
        );
        let err = resolve_plugin_configs(&gw).unwrap_err();
        assert!(
            err.contains("supernode") && err.contains("config_ref"),
            "{err}"
        );
    }

    #[test]
    fn test_duplicate_def_names_rejected() {
        let gw = gw_with(
            vec![],
            vec![def("d", "cors", json!({})), def("d", "cors", json!({}))],
        );
        let err = resolve_plugin_configs(&gw).unwrap_err();
        assert!(err.contains("Duplicate plugin config name 'd'"), "{err}");
    }

    #[test]
    fn test_unknown_and_reserved_def_types_rejected() {
        let gw = gw_with(vec![], vec![def("d", "openid-conect", json!({}))]);
        let err = resolve_plugin_configs(&gw).unwrap_err();
        assert!(err.contains("unknown plugin type 'openid-conect'"), "{err}");

        for ty in RESERVED_TYPES {
            let gw = gw_with(vec![], vec![def("d", ty, json!({}))]);
            let err = resolve_plugin_configs(&gw).unwrap_err();
            assert!(err.contains("reserved"), "type {ty}: {err}");
        }
    }
}
