//! The pre-compile pipeline shared by the sandbox, the MCP `validate_policy`
//! tool and config apply: structural validation, `config_ref` resolution and
//! supernode expansion, in that order.

use crate::config::{GatewayConfig, PluginConfigDef, PolicyConfig, SupernodeConfig};
use crate::graph::{expand_policy, validate_policy};

/// Turns a stored policy into the compile-ready form.
///
/// Errors are the human-readable strings the Admin API already returns:
/// `validate_policy` violations joined with `"; "`, then the first
/// resolution or expansion error.
pub fn prepare_policy(
    policy: PolicyConfig,
    supernodes: &[SupernodeConfig],
    plugin_configs: &[PluginConfigDef],
) -> Result<PolicyConfig, String> {
    if let Err(errors) = validate_policy(&policy) {
        return Err(errors.join("; "));
    }
    let mut tmp: GatewayConfig = serde_yaml::from_str("{}").expect("empty config parses");
    tmp.policies = vec![policy];
    tmp.supernodes = supernodes.to_vec();
    tmp.plugin_configs = plugin_configs.to_vec();
    let resolved = crate::config::resolve_plugin_configs(&tmp)?;
    for warning in crate::config::collect_template_warnings(&resolved) {
        tracing::warn!("{warning}");
    }
    let policy = resolved
        .policies
        .into_iter()
        .next()
        .expect("one policy in, one policy out");
    expand_policy(&policy, &resolved.supernodes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(yaml: &str) -> PolicyConfig {
        serde_yaml::from_str(yaml).unwrap()
    }

    #[test]
    fn rejects_structural_errors_with_joined_messages() {
        let p = policy("name: p\nnodes:\n  - id: l\n    type: listener\nedges: []\n");
        let err = prepare_policy(p, &[], &[]).unwrap_err();
        assert!(err.contains("client"), "{err}");
    }

    #[test]
    fn resolves_config_ref_and_passes_valid_policy_through() {
        let p = policy(
            "name: p\nnodes:\n  - id: l\n    type: listener\n  - id: e\n    type: echo\n    config_ref: shared-echo\n  - id: c\n    type: client\nedges:\n  - from: l.out\n    to: e.in\n  - from: e.out\n    to: c.in\n",
        );
        let pc: PluginConfigDef =
            serde_yaml::from_str("name: shared-echo\ntype: echo\nconfig:\n  body: hi\n").unwrap();
        let out = prepare_policy(p, &[], &[pc]).unwrap();
        let echo = out.nodes.iter().find(|n| n.id == "e").unwrap();
        assert_eq!(echo.config.get("body").and_then(|v| v.as_str()), Some("hi"));
    }

    #[test]
    fn unknown_config_ref_is_an_error() {
        let p = policy(
            "name: p\nnodes:\n  - id: l\n    type: listener\n  - id: e\n    type: echo\n    config_ref: nope\n  - id: c\n    type: client\nedges:\n  - from: l.out\n    to: e.in\n  - from: e.out\n    to: c.in\n",
        );
        let err = prepare_policy(p, &[], &[]).unwrap_err();
        assert!(err.contains("nope"), "{err}");
    }
}
