//! Structural validation of policy node graphs, run before compilation
//! (e.g. at config load and on Admin API writes) so malformed policies are
//! rejected with actionable messages instead of failing at request time.

use std::collections::HashSet;

use super::expand::{split_endpoint, BOUNDARY_TYPES};
use crate::config::{PolicyConfig, SupernodeConfig};

/// Validates a policy's node graph structure, collecting all violations.
///
/// Enforced rules:
/// - no two nodes share an id;
/// - the policy has a `listener` node (entry) and a `client` node (exit);
/// - every edge endpoint references an existing node;
/// - each input port has at most one incoming edge, except inputs of
///   `client` and `error-handler` nodes, which accept multiple;
/// - no orphan nodes (a node with neither incoming nor outgoing edges;
///   being named as the policy-level `error_handler` counts as connected);
/// - `error_handler`, if set, references an existing node that is not a
///   `type: supernode` instance (expansion removes instance nodes, so a
///   catch-all pointed at one would dangle at request time).
///
/// Returns `Ok(())` when valid, otherwise `Err` with one message per
/// violation (validation does not stop at the first error).
pub fn validate_policy(policy: &PolicyConfig) -> Result<(), Vec<String>> {
    let mut errors = Vec::new();

    let node_ids: HashSet<&str> = policy.nodes.iter().map(|n| n.id.as_str()).collect();

    // Duplicate node ids collapse silently in compile — with supernodes they
    // would silently drop an expansion — so reject each duplicate up front.
    let mut seen_ids: HashSet<&str> = HashSet::new();
    for node in &policy.nodes {
        if !seen_ids.insert(node.id.as_str()) {
            errors.push(format!("Duplicate node id '{}'", node.id));
        }
    }

    // Must have a listener node
    let has_listener = policy.nodes.iter().any(|n| n.node_type == "listener");
    if !has_listener {
        errors.push("Policy must have a 'listener' node".to_string());
    }

    // Must have a client node
    let has_client = policy.nodes.iter().any(|n| n.node_type == "client");
    if !has_client {
        errors.push("Policy must have a 'client' node".to_string());
    }

    // Node-id and reserved-type hygiene: `/` is the supernode namespace
    // separator, and boundary pseudo-types only exist inside definitions.
    for node in &policy.nodes {
        if node.id.contains('/') {
            errors.push(format!(
                "Node id '{}' must not contain '/' (reserved for supernode expansion)",
                node.id
            ));
        }
        if BOUNDARY_TYPES.contains(&node.node_type.as_str()) {
            errors.push(format!(
                "Node '{}' has type '{}', which is reserved for supernode definitions",
                node.id, node.node_type
            ));
        }
    }

    // Validate edges reference existing nodes
    for edge in &policy.edges {
        let from_node = edge.from.split('.').next().unwrap_or("");
        let to_node = edge.to.split('.').next().unwrap_or("");

        if !node_ids.contains(from_node) {
            errors.push(format!(
                "Edge references unknown source node: '{}'",
                from_node
            ));
        }
        if !node_ids.contains(to_node) {
            errors.push(format!(
                "Edge references unknown target node: '{}'",
                to_node
            ));
        }
    }

    // Check for multiple edges into the same input port.
    // Exceptions: client nodes (multiple paths can deliver the response) and
    // error-handler nodes (can receive errors from multiple nodes).
    let client_ids: HashSet<&str> = policy
        .nodes
        .iter()
        .filter(|n| n.node_type == "client")
        .map(|n| n.id.as_str())
        .collect();
    let error_handler_ids: HashSet<&str> = policy
        .nodes
        .iter()
        .filter(|n| n.node_type == "error-handler")
        .map(|n| n.id.as_str())
        .collect();

    let mut input_targets: HashSet<String> = HashSet::new();
    for edge in &policy.edges {
        let target = &edge.to;
        let to_node = target.split('.').next().unwrap_or("");

        let is_client = client_ids.contains(to_node);
        let is_error_handler = error_handler_ids.contains(to_node);

        if !is_client && !is_error_handler && !input_targets.insert(target.clone()) {
            errors.push(format!(
                "Node '{}' input '{}' has multiple incoming edges — each input accepts only one edge",
                to_node, target
            ));
        }
    }

    // Check for orphan nodes (no incoming or outgoing edges)
    let mut connected_nodes: HashSet<&str> = HashSet::new();
    for edge in &policy.edges {
        let from_node = edge.from.split('.').next().unwrap_or("");
        let to_node = edge.to.split('.').next().unwrap_or("");
        connected_nodes.insert(from_node);
        connected_nodes.insert(to_node);
    }

    // Also include the catch-all handler if specified
    if let Some(ref handler) = policy.error_handler {
        connected_nodes.insert(handler.as_str());
    }

    for node in &policy.nodes {
        if !connected_nodes.contains(node.id.as_str()) {
            errors.push(format!("Orphan node '{}' has no connections", node.id));
        }
    }

    // Validate error_handler references an existing node
    if let Some(ref handler) = policy.error_handler {
        if !node_ids.contains(handler.as_str()) {
            errors.push(format!(
                "Policy error_handler references unknown node: '{}'",
                handler
            ));
        } else if let Some(node) = policy.nodes.iter().find(|n| n.id == *handler) {
            if node.node_type == "supernode" {
                errors.push(format!(
                    "Policy error_handler cannot reference supernode instance '{}' — point it at a concrete node",
                    handler
                ));
            }
        }
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

/// Validates a supernode definition's structure, collecting all violations.
///
/// Enforced rules (spec §3):
/// - exactly one boundary node each of type `input`/`output`/`error`, with
///   id equal to its type;
/// - inner nodes must not be `listener`/`client`/`supernode`, must not use
///   reserved ids, and must not contain `/`;
/// - exactly one edge leaves `input`; no edges into `input` or out of
///   `output`/`error`;
/// - every edge endpoint references an existing node;
/// - one incoming edge per input port, except `output`/`error` boundaries
///   and `error-handler`-typed inner nodes (fan-in allowed);
/// - no orphan inner nodes (unconnected `output`/`error` boundaries are
///   fine — not every subgraph uses both exits).
pub fn validate_supernode(sn: &SupernodeConfig) -> Result<(), Vec<String>> {
    let mut errors = Vec::new();

    for ty in BOUNDARY_TYPES {
        let matching: Vec<&crate::config::NodeConfig> =
            sn.nodes.iter().filter(|n| n.node_type == ty).collect();
        match matching.as_slice() {
            [one] if one.id == ty => {}
            [one] => errors.push(format!(
                "Supernode '{}': boundary node of type '{}' must have id '{}' (got '{}')",
                sn.name, ty, ty, one.id
            )),
            [] => errors.push(format!(
                "Supernode '{}' must declare an '{}' boundary node",
                sn.name, ty
            )),
            _ => errors.push(format!(
                "Supernode '{}' declares more than one '{}' node",
                sn.name, ty
            )),
        }
    }

    for n in &sn.nodes {
        let is_boundary = BOUNDARY_TYPES.contains(&n.node_type.as_str());
        if n.id.contains('/') {
            errors.push(format!(
                "Supernode '{}': node id '{}' must not contain '/'",
                sn.name, n.id
            ));
        }
        if !is_boundary {
            if BOUNDARY_TYPES.contains(&n.id.as_str()) {
                errors.push(format!(
                    "Supernode '{}': node id '{}' is reserved for boundary nodes",
                    sn.name, n.id
                ));
            }
            if ["listener", "client", "supernode"].contains(&n.node_type.as_str()) {
                errors.push(format!(
                    "Supernode '{}': node '{}' has forbidden type '{}' \
                     (supernodes cannot contain endpoints or other supernodes)",
                    sn.name, n.id, n.node_type
                ));
            }
        }
    }

    let node_ids: HashSet<&str> = sn.nodes.iter().map(|n| n.id.as_str()).collect();
    for edge in &sn.edges {
        let (from_node, _) = split_endpoint(&edge.from);
        let (to_node, _) = split_endpoint(&edge.to);
        if !node_ids.contains(from_node) {
            errors.push(format!(
                "Supernode '{}': edge references unknown source node: '{}'",
                sn.name, from_node
            ));
        }
        if !node_ids.contains(to_node) {
            errors.push(format!(
                "Supernode '{}': edge references unknown target node: '{}'",
                sn.name, to_node
            ));
        }
        if to_node == "input" {
            errors.push(format!(
                "Supernode '{}': the 'input' boundary cannot have incoming edges",
                sn.name
            ));
        }
        if from_node == "output" || from_node == "error" {
            errors.push(format!(
                "Supernode '{}': the '{}' boundary cannot have outgoing edges",
                sn.name, from_node
            ));
        }
    }

    let input_out_edges = sn
        .edges
        .iter()
        .filter(|e| split_endpoint(&e.from).0 == "input")
        .count();
    if input_out_edges != 1 {
        errors.push(format!(
            "Supernode '{}' must have exactly one edge from input.out (found {})",
            sn.name, input_out_edges
        ));
    }

    // One incoming edge per input port; boundary exits and error-handlers fan in.
    let fan_in_ok: HashSet<&str> = sn
        .nodes
        .iter()
        .filter(|n| {
            n.node_type == "error-handler" || n.node_type == "output" || n.node_type == "error"
        })
        .map(|n| n.id.as_str())
        .collect();
    let mut input_targets: HashSet<String> = HashSet::new();
    for edge in &sn.edges {
        let (to_node, _) = split_endpoint(&edge.to);
        if !fan_in_ok.contains(to_node) && !input_targets.insert(edge.to.clone()) {
            errors.push(format!(
                "Supernode '{}': node '{}' input '{}' has multiple incoming edges",
                sn.name, to_node, edge.to
            ));
        }
    }

    // Orphans: every non-boundary node needs at least one edge.
    let mut connected: HashSet<&str> = HashSet::new();
    for edge in &sn.edges {
        connected.insert(split_endpoint(&edge.from).0);
        connected.insert(split_endpoint(&edge.to).0);
    }
    for n in &sn.nodes {
        if !BOUNDARY_TYPES.contains(&n.node_type.as_str()) && !connected.contains(n.id.as_str()) {
            errors.push(format!(
                "Supernode '{}': Orphan node '{}' has no connections",
                sn.name, n.id
            ));
        }
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{EdgeConfig, NodeConfig, PolicyConfig, SupernodeConfig};
    use std::collections::HashMap;

    fn listener_node() -> NodeConfig {
        NodeConfig {
            id: "listener".to_string(),
            node_type: "listener".to_string(),
            config: HashMap::new(),
            config_ref: None,
            position: None,
        }
    }

    fn client_node() -> NodeConfig {
        NodeConfig {
            id: "client".to_string(),
            node_type: "client".to_string(),
            config: HashMap::new(),
            config_ref: None,
            position: None,
        }
    }

    fn upstream_node() -> NodeConfig {
        NodeConfig {
            id: "backend".to_string(),
            node_type: "upstream".to_string(),
            config: HashMap::new(),
            config_ref: None,
            position: None,
        }
    }

    fn boundary_nodes() -> Vec<NodeConfig> {
        ["input", "output", "error"]
            .into_iter()
            .map(|t| NodeConfig {
                id: t.to_string(),
                node_type: t.to_string(),
                config: HashMap::new(),
                config_ref: None,
                position: None,
            })
            .collect()
    }

    fn inner(id: &str, ty: &str) -> NodeConfig {
        NodeConfig {
            id: id.to_string(),
            node_type: ty.to_string(),
            config: HashMap::new(),
            config_ref: None,
            position: None,
        }
    }

    fn sn_edge(from: &str, to: &str) -> EdgeConfig {
        EdgeConfig {
            from: from.to_string(),
            to: to.to_string(),
        }
    }

    fn valid_supernode() -> SupernodeConfig {
        let mut nodes = boundary_nodes();
        nodes.push(inner("up", "upstream"));
        SupernodeConfig {
            name: "sn".to_string(),
            description: None,
            nodes,
            edges: vec![
                sn_edge("input.out", "up.in"),
                sn_edge("up.success", "output.in"),
                sn_edge("up.error", "error.in"),
            ],
        }
    }

    #[test]
    fn test_valid_simple_policy() {
        let policy = PolicyConfig {
            name: "test".to_string(),
            error_handler: None,
            nodes: vec![listener_node(), upstream_node(), client_node()],
            edges: vec![
                EdgeConfig {
                    from: "listener.out".to_string(),
                    to: "backend.in".to_string(),
                },
                EdgeConfig {
                    from: "backend.success".to_string(),
                    to: "client.in".to_string(),
                },
            ],
        };
        assert!(validate_policy(&policy).is_ok());
    }

    #[test]
    fn test_missing_listener() {
        let policy = PolicyConfig {
            name: "test".to_string(),
            error_handler: None,
            nodes: vec![upstream_node(), client_node()],
            edges: vec![],
        };
        let errors = validate_policy(&policy).unwrap_err();
        assert!(errors.iter().any(|e| e.contains("listener")));
    }

    #[test]
    fn test_missing_client() {
        let policy = PolicyConfig {
            name: "test".to_string(),
            error_handler: None,
            nodes: vec![listener_node(), upstream_node()],
            edges: vec![EdgeConfig {
                from: "listener.out".to_string(),
                to: "backend.in".to_string(),
            }],
        };
        let errors = validate_policy(&policy).unwrap_err();
        assert!(errors.iter().any(|e| e.contains("client")));
    }

    #[test]
    fn test_unknown_edge_reference() {
        let policy = PolicyConfig {
            name: "test".to_string(),
            error_handler: None,
            nodes: vec![listener_node(), client_node()],
            edges: vec![EdgeConfig {
                from: "listener.out".to_string(),
                to: "nonexistent.in".to_string(),
            }],
        };
        let errors = validate_policy(&policy).unwrap_err();
        assert!(errors.iter().any(|e| e.contains("nonexistent")));
    }

    #[test]
    fn test_client_allows_multiple_inputs() {
        let policy = PolicyConfig {
            name: "test".to_string(),
            error_handler: None,
            nodes: vec![listener_node(), upstream_node(), client_node()],
            edges: vec![
                EdgeConfig {
                    from: "listener.out".to_string(),
                    to: "backend.in".to_string(),
                },
                EdgeConfig {
                    from: "backend.success".to_string(),
                    to: "client.in".to_string(),
                },
                EdgeConfig {
                    from: "backend.error".to_string(),
                    to: "client.in".to_string(),
                },
            ],
        };
        assert!(validate_policy(&policy).is_ok());
    }

    #[test]
    fn test_valid_supernode_passes() {
        assert!(validate_supernode(&valid_supernode()).is_ok());
    }

    #[test]
    fn test_supernode_missing_boundary_nodes() {
        let mut sn = valid_supernode();
        sn.nodes.retain(|n| n.node_type != "error");
        let errors = validate_supernode(&sn).unwrap_err();
        assert!(
            errors.iter().any(|e| e.contains("'error' boundary node")),
            "{errors:?}"
        );
    }

    #[test]
    fn test_supernode_boundary_id_must_match_type() {
        let mut sn = valid_supernode();
        sn.nodes
            .iter_mut()
            .find(|n| n.node_type == "input")
            .unwrap()
            .id = "start".into();
        let errors = validate_supernode(&sn).unwrap_err();
        assert!(
            errors.iter().any(|e| e.contains("must have id 'input'")),
            "{errors:?}"
        );
    }

    #[test]
    fn test_supernode_forbidden_inner_types() {
        for ty in ["listener", "client", "supernode"] {
            let mut sn = valid_supernode();
            sn.nodes.push(inner("x", ty));
            sn.edges.push(sn_edge("up.success", "x.in"));
            let errors = validate_supernode(&sn).unwrap_err();
            assert!(
                errors.iter().any(|e| e.contains("forbidden type")),
                "type {ty}: {errors:?}"
            );
        }
    }

    #[test]
    fn test_supernode_reserved_inner_ids_and_slash() {
        let mut sn = valid_supernode();
        sn.nodes.push(inner("output2/x", "cors"));
        sn.edges.push(sn_edge("up.success", "output2/x.in"));
        let errors = validate_supernode(&sn).unwrap_err();
        assert!(
            errors.iter().any(|e| e.contains("must not contain '/'")),
            "{errors:?}"
        );
    }

    #[test]
    fn test_supernode_input_needs_exactly_one_outgoing_edge() {
        let mut sn = valid_supernode();
        sn.nodes.push(inner("cors", "cors"));
        sn.edges.push(sn_edge("input.out", "cors.in"));
        sn.edges.push(sn_edge("cors.success", "output.in"));
        let errors = validate_supernode(&sn).unwrap_err();
        assert!(
            errors.iter().any(|e| e.contains("exactly one edge")),
            "{errors:?}"
        );

        let mut sn = valid_supernode();
        sn.edges.retain(|e| !e.from.starts_with("input."));
        let errors = validate_supernode(&sn).unwrap_err();
        assert!(
            errors.iter().any(|e| e.contains("exactly one edge")),
            "{errors:?}"
        );
    }

    #[test]
    fn test_supernode_boundary_direction_rules() {
        let mut sn = valid_supernode();
        sn.edges.push(sn_edge("output.out", "up.in")); // out of output: invalid
        sn.edges.push(sn_edge("up.success", "input.in")); // into input: invalid
        let errors = validate_supernode(&sn).unwrap_err();
        assert!(
            errors.iter().any(|e| e.contains("cannot have outgoing")),
            "{errors:?}"
        );
        assert!(
            errors.iter().any(|e| e.contains("cannot have incoming")),
            "{errors:?}"
        );
    }

    /// Two branches may exit through the same boundary port (fan-in), and an
    /// unconnected `error` boundary is not an orphan.
    #[test]
    fn test_supernode_fan_in_to_output_and_unused_error_ok() {
        let mut nodes = boundary_nodes();
        nodes.push(inner("a", "cors"));
        nodes.push(inner("b", "gzip"));
        let sn = SupernodeConfig {
            name: "fan".to_string(),
            description: None,
            nodes,
            edges: vec![
                sn_edge("input.out", "a.in"),
                sn_edge("a.success", "b.in"),
                sn_edge("a.error", "output.in"),
                sn_edge("b.success", "output.in"),
            ],
        };
        assert!(
            validate_supernode(&sn).is_ok(),
            "{:?}",
            validate_supernode(&sn)
        );
    }

    #[test]
    fn test_supernode_dangling_edge_and_orphan() {
        let mut sn = valid_supernode();
        sn.edges.push(sn_edge("ghost.success", "output.in"));
        sn.nodes.push(inner("lonely", "cors"));
        let errors = validate_supernode(&sn).unwrap_err();
        assert!(
            errors
                .iter()
                .any(|e| e.contains("unknown source node: 'ghost'")),
            "{errors:?}"
        );
        assert!(
            errors.iter().any(|e| e.contains("Orphan node 'lonely'")),
            "{errors:?}"
        );
    }

    #[test]
    fn test_policy_rejects_slash_ids_and_reserved_types() {
        let mut policy = PolicyConfig {
            name: "test".to_string(),
            error_handler: None,
            nodes: vec![listener_node(), upstream_node(), client_node()],
            edges: vec![
                EdgeConfig {
                    from: "listener.out".to_string(),
                    to: "backend.in".to_string(),
                },
                EdgeConfig {
                    from: "backend.success".to_string(),
                    to: "client.in".to_string(),
                },
            ],
        };
        policy.nodes[1].id = "a/b".to_string();
        policy.edges[0].to = "a/b.in".to_string();
        policy.edges[1].from = "a/b.success".to_string();
        let errors = validate_policy(&policy).unwrap_err();
        assert!(
            errors.iter().any(|e| e.contains("must not contain '/'")),
            "{errors:?}"
        );

        let mut policy2 = PolicyConfig {
            name: "test2".to_string(),
            error_handler: None,
            nodes: vec![listener_node(), inner("x", "input"), client_node()],
            edges: vec![
                EdgeConfig {
                    from: "listener.out".to_string(),
                    to: "x.in".to_string(),
                },
                EdgeConfig {
                    from: "x.success".to_string(),
                    to: "client.in".to_string(),
                },
            ],
        };
        policy2.nodes[1].node_type = "input".to_string();
        let errors = validate_policy(&policy2).unwrap_err();
        assert!(
            errors.iter().any(|e| e.contains("reserved for supernode")),
            "{errors:?}"
        );
    }

    /// I-1: a policy whose `error_handler` names a `type: supernode`
    /// instance passes structural checks against the un-expanded node list
    /// but dangles after expansion (the instance node is removed). Must be
    /// rejected at validate_policy time, before expansion ever runs.
    #[test]
    fn test_error_handler_rejects_supernode_instance() {
        let mut sn_instance = inner("sec", "supernode");
        sn_instance
            .config
            .insert("name".to_string(), serde_json::json!("secured-call"));

        let policy = PolicyConfig {
            name: "test".to_string(),
            error_handler: Some("sec".to_string()),
            nodes: vec![listener_node(), sn_instance, client_node()],
            edges: vec![
                EdgeConfig {
                    from: "listener.out".to_string(),
                    to: "sec.in".to_string(),
                },
                EdgeConfig {
                    from: "sec.success".to_string(),
                    to: "client.in".to_string(),
                },
            ],
        };
        let errors = validate_policy(&policy).unwrap_err();
        assert!(
            errors
                .iter()
                .any(|e| e.contains("error_handler") && e.contains("sec")),
            "{errors:?}"
        );
    }

    /// FINDING D: two nodes sharing an id collapse silently in compile —
    /// with supernodes this would silently drop an expansion — so
    /// validate_policy must reject every duplicate.
    #[test]
    fn test_duplicate_node_ids_rejected() {
        let policy = PolicyConfig {
            name: "test".to_string(),
            error_handler: None,
            nodes: vec![
                listener_node(),
                upstream_node(),
                upstream_node(),
                client_node(),
            ],
            edges: vec![
                EdgeConfig {
                    from: "listener.out".to_string(),
                    to: "backend.in".to_string(),
                },
                EdgeConfig {
                    from: "backend.success".to_string(),
                    to: "client.in".to_string(),
                },
            ],
        };
        let errors = validate_policy(&policy).unwrap_err();
        assert!(
            errors
                .iter()
                .any(|e| e.contains("Duplicate node id 'backend'")),
            "{errors:?}"
        );
    }
}
