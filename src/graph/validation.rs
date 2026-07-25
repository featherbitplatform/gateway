//! Structural validation of policy node graphs, run before compilation
//! (e.g. at config load and on Admin API writes) so malformed policies are
//! rejected with actionable messages instead of failing at request time.

use std::collections::HashSet;

use crate::config::PolicyConfig;

/// Validates a policy's node graph structure, collecting all violations.
///
/// Enforced rules:
/// - the policy has a `listener` node (entry) and a `client` node (exit);
/// - every edge endpoint references an existing node;
/// - each input port has at most one incoming edge, except inputs of
///   `client` and `error-handler` nodes, which accept multiple;
/// - no orphan nodes (a node with neither incoming nor outgoing edges;
///   being named as the policy-level `error_handler` counts as connected);
/// - `error_handler`, if set, references an existing node.
///
/// Returns `Ok(())` when valid, otherwise `Err` with one message per
/// violation (validation does not stop at the first error).
pub fn validate_policy(policy: &PolicyConfig) -> Result<(), Vec<String>> {
    let mut errors = Vec::new();

    let node_ids: HashSet<&str> = policy.nodes.iter().map(|n| n.id.as_str()).collect();

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
    use crate::config::{EdgeConfig, NodeConfig, PolicyConfig};
    use std::collections::HashMap;

    fn listener_node() -> NodeConfig {
        NodeConfig {
            id: "listener".to_string(),
            node_type: "listener".to_string(),
            config: HashMap::new(),
            position: None,
        }
    }

    fn client_node() -> NodeConfig {
        NodeConfig {
            id: "client".to_string(),
            node_type: "client".to_string(),
            config: HashMap::new(),
            position: None,
        }
    }

    fn upstream_node() -> NodeConfig {
        NodeConfig {
            id: "backend".to_string(),
            node_type: "upstream".to_string(),
            config: HashMap::new(),
            position: None,
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
}
