//! Load-time scan for unresolved `{{...}}` template syntax.
//!
//! [`collect_template_warnings`] walks every string leaf of every node
//! `config` across policies, supernodes, and `plugin_configs`, collecting one
//! coordinated warning per unresolved reference via
//! [`Template::source_warnings_only`]. It is pure logging support: it never
//! fails the config, it only surfaces likely typos (e.g.
//! `{{request.headres.x}}`) at load time instead of leaving them to render as
//! silent literals on every request.
//!
//! Call this on the gateway config *after* [`resolve_plugin_configs`] has run
//! (`SharedState::compile_routes` does so right after resolution; the debug
//! sandbox does the same on its synthesized/stored single-policy gateway
//! before compiling). Walking the resolved copy means a `config_ref`'d node's
//! merged-in shared config is covered too, reached through the policy/supernode
//! walk. `plugin_configs` are *also* walked directly (as their own container)
//! so a shared config with no current referrer still gets flagged.
//!
//! Warnings are advisory only, not proof of a bug: the walker inspects every
//! string leaf generically and has no notion of per-plugin field semantics.
//! A handful of fields intentionally use their string value verbatim, never
//! templated — e.g. `limit-conn`'s `key` in `key_type: constant` mode — so a
//! constant value that happens to contain `{{...}}` draws a spurious warning
//! here. Accepted false positive; do not try to special-case field names in
//! this generic walker.
//!
//! [`resolve_plugin_configs`]: super::resolve_plugin_configs

use serde_json::Value;

use super::gateway::GatewayConfig;
use crate::vars::template::Template;

/// Walks every string leaf of every node config in `gw` (policies, supernodes,
/// plugin_configs) and returns one fully-coordinated warning per unresolved
/// `{{...}}` reference found. See the module doc for placement and caveats.
pub fn collect_template_warnings(gw: &GatewayConfig) -> Vec<String> {
    let mut warnings = Vec::new();

    for policy in &gw.policies {
        for node in &policy.nodes {
            for (key, value) in &node.config {
                for (path, warning) in leaves(value, key) {
                    warnings.push(format!(
                        "policy '{}' node '{}' key '{}': {}",
                        policy.name, node.id, path, warning
                    ));
                }
            }
        }
    }

    for sn in &gw.supernodes {
        for node in &sn.nodes {
            for (key, value) in &node.config {
                for (path, warning) in leaves(value, key) {
                    warnings.push(format!(
                        "supernode '{}' node '{}' key '{}': {}",
                        sn.name, node.id, path, warning
                    ));
                }
            }
        }
    }

    for def in &gw.plugin_configs {
        for (key, value) in &def.config {
            for (path, warning) in leaves(value, key) {
                warnings.push(format!(
                    "plugin-config '{}' key '{}': {}",
                    def.name, path, warning
                ));
            }
        }
    }

    warnings
}

/// Recursively collects `(path, warning)` pairs from every string leaf of
/// `value`. `path` starts at `root` (the top-level config key) and accumulates
/// dotted object keys (`headers.x-custom`) and `[i]` array indices
/// (`list[1]`) down to the leaf that produced the warning.
fn leaves(value: &Value, root: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    walk(value, root, &mut out);
    out
}

fn walk(value: &Value, path: &str, out: &mut Vec<(String, String)>) {
    match value {
        Value::String(s) => {
            for warning in Template::source_warnings_only(s) {
                out.push((path.to_string(), warning));
            }
        }
        Value::Array(items) => {
            for (i, item) in items.iter().enumerate() {
                walk(item, &format!("{path}[{i}]"), out);
            }
        }
        Value::Object(map) => {
            for (k, v) in map {
                walk(v, &format!("{path}.{k}"), out);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{NodeConfig, PluginConfigDef, PolicyConfig, SupernodeConfig};
    use serde_json::json;

    fn node(id: &str, ty: &str, config: serde_json::Value) -> NodeConfig {
        NodeConfig {
            id: id.into(),
            node_type: ty.into(),
            config: serde_json::from_value(config).unwrap(),
            config_ref: None,
            position: None,
        }
    }

    fn gw() -> GatewayConfig {
        serde_yaml::from_str("{}").unwrap()
    }

    /// A malformed reference (`headres` typo'd for `headers`) inside a single
    /// policy node config key must yield exactly one warning naming the
    /// policy, node, and key.
    #[test]
    fn test_unknown_reference_yields_one_named_warning() {
        let mut gw = gw();
        gw.policies = vec![PolicyConfig {
            name: "p".into(),
            error_handler: None,
            nodes: vec![node(
                "n",
                "proxy-rewrite",
                json!({"uri": "{{request.headres.x}}"}),
            )],
            edges: Vec::new(),
        }];

        let warnings = collect_template_warnings(&gw);

        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("policy 'p'"), "{}", warnings[0]);
        assert!(warnings[0].contains("node 'n'"), "{}", warnings[0]);
        assert!(warnings[0].contains("key 'uri'"), "{}", warnings[0]);
        assert!(
            warnings[0].contains("{{request.headres.x}}"),
            "{}",
            warnings[0]
        );
    }

    /// A valid, fully-recognized reference must never produce a warning.
    #[test]
    fn test_valid_reference_yields_no_warnings() {
        let mut gw = gw();
        gw.policies = vec![PolicyConfig {
            name: "p".into(),
            error_handler: None,
            nodes: vec![node(
                "n",
                "proxy-rewrite",
                json!({"uri": "{{request.path}}"}),
            )],
            edges: Vec::new(),
        }];

        let warnings = collect_template_warnings(&gw);

        assert!(warnings.is_empty(), "{warnings:?}");
    }

    /// Leaves nested inside objects and arrays must be reached and their
    /// accumulated path reported.
    #[test]
    fn test_nested_object_and_array_leaves_covered() {
        let mut gw = gw();
        gw.policies = vec![PolicyConfig {
            name: "p".into(),
            error_handler: None,
            nodes: vec![node(
                "n",
                "proxy-rewrite",
                json!({
                    "headers": { "x-custom": "{{request.headres.x}}" },
                    "list": ["fine", "{{client.bogus}}"]
                }),
            )],
            edges: Vec::new(),
        }];

        let warnings = collect_template_warnings(&gw);

        assert_eq!(warnings.len(), 2, "{warnings:?}");
        assert!(
            warnings
                .iter()
                .any(|w| w.contains("key 'headers.x-custom'")),
            "{warnings:?}"
        );
        assert!(
            warnings.iter().any(|w| w.contains("key 'list[1]'")),
            "{warnings:?}"
        );
    }

    /// Supernode node configs must be walked too, named by their own
    /// container ("supernode '<name>' node '<id>'"), not "policy".
    #[test]
    fn test_supernode_container_named() {
        let mut gw = gw();
        gw.supernodes = vec![SupernodeConfig {
            name: "sn".into(),
            description: None,
            nodes: vec![node(
                "inner",
                "proxy-rewrite",
                json!({"uri": "{{request.headres.x}}"}),
            )],
            edges: Vec::new(),
        }];

        let warnings = collect_template_warnings(&gw);

        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("supernode 'sn'"), "{}", warnings[0]);
        assert!(warnings[0].contains("node 'inner'"), "{}", warnings[0]);
        assert!(warnings[0].contains("key 'uri'"), "{}", warnings[0]);
    }

    /// `plugin_configs` definitions must be walked directly (as their own
    /// container, not requiring a referencing node) and named
    /// "plugin-config '<name>'", with no "node" segment.
    #[test]
    fn test_plugin_config_container_named() {
        let mut gw = gw();
        gw.plugin_configs = vec![PluginConfigDef {
            name: "shared".into(),
            plugin_type: "proxy-rewrite".into(),
            description: None,
            config: serde_json::from_value(json!({"uri": "{{request.headres.x}}"})).unwrap(),
        }];

        let warnings = collect_template_warnings(&gw);

        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(
            warnings[0].contains("plugin-config 'shared'"),
            "{}",
            warnings[0]
        );
        assert!(warnings[0].contains("key 'uri'"), "{}", warnings[0]);
        assert!(!warnings[0].contains("node "), "{}", warnings[0]);
    }

    #[test]
    fn test_empty_gateway_yields_no_warnings() {
        assert!(collect_template_warnings(&gw()).is_empty());
    }
}
