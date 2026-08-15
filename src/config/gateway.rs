//! Schema for `gateway.yaml`: routes (match rules bound to a policy) and
//! node-graph policies (nodes plus success/error edges). This file is the
//! hot-reloadable half of the configuration and is also mutated at runtime
//! by the Admin API.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Root of `gateway.yaml`: the route table and the policies it references.
///
/// Both sections default to empty, so a missing or minimal file is valid.
///
/// ```yaml
/// routes:
///   - name: api
///     match: { path: /api/* }
///     policy: api-policy
/// policies:
///   - name: api-policy
///     nodes:
///       - { id: in, type: listener }
///       - { id: up, type: upstream, config: { url: "http://backend:3000" } }
///     edges:
///       - { from: in.out, to: up.in }
/// ```
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct GatewayConfig {
    /// Routes evaluated in declaration order; the first match wins.
    #[serde(default)]
    pub routes: Vec<RouteConfig>,
    /// Policies referenced by name from `routes`.
    #[serde(default)]
    pub policies: Vec<PolicyConfig>,
    /// Named API clients with per-auth-plugin credentials; resolved by auth
    /// nodes configured with `use_consumers: true`.
    #[serde(default)]
    pub consumers: Vec<crate::consumers::ConsumerConfig>,
    /// Reusable named subgraphs, referenced from policies by nodes of
    /// `type: supernode`; inlined at compile time (see src/graph/expand.rs).
    #[serde(default)]
    pub supernodes: Vec<SupernodeConfig>,
    /// Named, typed plugin configurations shared by any number of plugin
    /// nodes via `config_ref`; resolved at compile time (src/config/resolve.rs).
    #[serde(default)]
    pub plugin_configs: Vec<PluginConfigDef>,
}

/// Binds a request match rule to a named policy.
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct RouteConfig {
    /// Unique route name, used in logs, metrics labels, and the Admin API.
    pub name: String,
    /// Conditions a request must satisfy (YAML key: `match`).
    #[serde(rename = "match")]
    pub match_rule: MatchRule,
    /// Name of the [`PolicyConfig`] to execute; must exist or compilation fails.
    pub policy: String,
}

/// Request-matching conditions for a route.
///
/// All specified criteria must match (logical AND); every field defaults to
/// unset/empty, which matches any request.
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct MatchRule {
    /// Path pattern to match (e.g. `/api/*`); `None` matches any path.
    #[serde(default)]
    pub path: Option<String>,
    /// Allowed HTTP methods; empty means any method.
    #[serde(default)]
    pub methods: Vec<String>,
    /// Required header name → value pairs; empty means no header constraints.
    #[serde(default)]
    pub headers: HashMap<String, String>,
    /// Required `Host` value; `None` matches any host.
    #[serde(default)]
    pub host: Option<String>,
}

/// A node-graph policy: a named pipeline of plugin nodes wired by edges.
///
/// Compiled into a `CompiledGraph` at load/reload time; execution starts at
/// the `listener` node and follows each node's `success`/`error` ports.
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct PolicyConfig {
    /// Unique policy name, referenced by [`RouteConfig::policy`].
    pub name: String,
    /// Optional id of a node to jump to when a node errors without an explicit error edge.
    #[serde(default)]
    pub error_handler: Option<String>,
    /// Plugin nodes making up the pipeline.
    #[serde(default)]
    pub nodes: Vec<NodeConfig>,
    /// Directed connections between node ports.
    #[serde(default)]
    pub edges: Vec<EdgeConfig>,
}

/// One plugin node in a policy graph.
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct NodeConfig {
    /// Unique node id within the policy, referenced by edges as `id.port`.
    pub id: String,
    /// Plugin type (YAML key: `type`), e.g. `upstream`, `key-auth`, `script`;
    /// must be one of the registered plugin types.
    #[serde(rename = "type")]
    pub node_type: String,
    /// Free-form plugin-specific configuration; defaults to empty.
    #[serde(default)]
    pub config: HashMap<String, serde_json::Value>,
    /// Optional name of a shared [`PluginConfigDef`] to inherit configuration
    /// from. The effective config is the shared config with this node's own
    /// `config` keys layered on top (shallow merge, local wins), materialized
    /// at compile time — the stored form always keeps the reference.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_ref: Option<String>,
    /// Canvas coordinates for the Web UI node editor; ignored by the engine
    /// and omitted from serialized output when unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub position: Option<Position>,
}

/// 2D canvas coordinates of a node in the Web UI graph editor.
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct Position {
    pub x: f64,
    pub y: f64,
}

/// A directed edge between two node ports, written as `node_id.port`.
///
/// Ports: `out`, `success`, `error` on the source side; `in` on the target
/// side (e.g. `from: auth.success`, `to: upstream.in`).
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct EdgeConfig {
    /// Source endpoint, `node_id.port`.
    pub from: String,
    /// Target endpoint, `node_id.port`.
    pub to: String,
}

/// A reusable named subgraph with a fixed boundary: exactly one `input`,
/// one `output`, and one `error` pseudo-node (declared in `nodes` like a
/// policy declares `listener`/`client`, so the UI can persist positions).
///
/// Instances appear in policies as nodes of `type: supernode` with
/// `config: { name: <this name> }` and are inlined at compile time —
/// stored configuration always keeps the compact reference form.
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct SupernodeConfig {
    /// Unique supernode name, referenced from policy nodes' `config.name`.
    pub name: String,
    /// Optional human-readable description (shown in the UI library).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Inner plugin nodes plus the three boundary pseudo-nodes.
    #[serde(default)]
    pub nodes: Vec<NodeConfig>,
    /// Directed connections; boundary edges use `input.out`, `output.in`,
    /// `error.in` endpoints.
    #[serde(default)]
    pub edges: Vec<EdgeConfig>,
}

/// A named, shared plugin configuration, referenced by plugin nodes of the
/// matching type via [`NodeConfig::config_ref`]. Editing a shared config
/// re-resolves and recompiles every referencing policy atomically.
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct PluginConfigDef {
    /// Unique name, referenced by `config_ref`.
    pub name: String,
    /// Plugin type this config is for (YAML key: `type`); only nodes of the
    /// same type may reference it.
    #[serde(rename = "type")]
    pub plugin_type: String,
    /// Optional human-readable description (shown in the UI library).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// The shared plugin configuration; same shape as [`NodeConfig::config`].
    #[serde(default)]
    pub config: HashMap<String, serde_json::Value>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Old configs without a `supernodes:` section must stay valid, and a
    /// definition must round-trip through YAML unchanged.
    #[test]
    fn test_supernodes_default_empty_and_roundtrip() {
        let gw: GatewayConfig = serde_yaml::from_str("{}").unwrap();
        assert!(gw.supernodes.is_empty());

        let yaml = r#"
supernodes:
  - name: secured-call
    description: "auth + upstream"
    nodes:
      - { id: input,  type: input }
      - { id: output, type: output }
      - { id: error,  type: error }
      - { id: up, type: upstream, config: { url: "http://svc" } }
    edges:
      - { from: input.out,  to: up.in }
      - { from: up.success, to: output.in }
      - { from: up.error,   to: error.in }
"#;
        let gw: GatewayConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(gw.supernodes.len(), 1);
        let sn = &gw.supernodes[0];
        assert_eq!(sn.name, "secured-call");
        assert_eq!(sn.description.as_deref(), Some("auth + upstream"));
        assert_eq!(sn.nodes.len(), 4);
        assert_eq!(sn.edges.len(), 3);

        let out = serde_yaml::to_string(&GatewayConfig {
            routes: vec![],
            policies: vec![],
            consumers: vec![],
            supernodes: gw.supernodes.clone(),
            plugin_configs: vec![],
        })
        .unwrap();
        let back: GatewayConfig = serde_yaml::from_str(&out).unwrap();
        assert_eq!(back.supernodes[0].name, "secured-call");
        assert_eq!(back.supernodes[0].nodes.len(), 4);
    }

    /// Old configs stay valid; a shared config and a `config_ref` round-trip
    /// through YAML; `config_ref` is omitted from output when unset.
    #[test]
    fn test_plugin_configs_default_empty_and_roundtrip() {
        let gw: GatewayConfig = serde_yaml::from_str("{}").unwrap();
        assert!(gw.plugin_configs.is_empty());

        let yaml = r#"
plugin_configs:
  - name: corp-oidc
    type: openid-connect
    description: "Corporate IdP client"
    config:
      client_id: gateway
      scope: openid
policies:
  - name: p
    nodes:
      - { id: auth, type: openid-connect, config_ref: corp-oidc, config: { scope: "openid profile" } }
"#;
        let gw: GatewayConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(gw.plugin_configs.len(), 1);
        let def = &gw.plugin_configs[0];
        assert_eq!(def.name, "corp-oidc");
        assert_eq!(def.plugin_type, "openid-connect");
        assert_eq!(def.description.as_deref(), Some("Corporate IdP client"));
        assert_eq!(def.config["client_id"], serde_json::json!("gateway"));
        let node = &gw.policies[0].nodes[0];
        assert_eq!(node.config_ref.as_deref(), Some("corp-oidc"));
        assert_eq!(node.config["scope"], serde_json::json!("openid profile"));

        // Round-trip keeps the reference form; nodes without a ref omit the key.
        let out = serde_yaml::to_string(&gw).unwrap();
        assert!(out.contains("config_ref: corp-oidc"), "{out}");
        let plain: GatewayConfig = serde_yaml::from_str(
            "policies:\n  - name: q\n    nodes:\n      - { id: a, type: cors }\n",
        )
        .unwrap();
        let plain_out = serde_yaml::to_string(&plain).unwrap();
        assert!(!plain_out.contains("config_ref"), "{plain_out}");
    }
}
