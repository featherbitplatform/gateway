//! Compile-time expansion of supernode instances into flat policies.
//!
//! A policy node of `type: supernode` (config `{ name: <supernode> }`) is
//! replaced by the referenced definition's inner nodes, ids namespaced
//! `<instance-id>/<inner-id>`. Boundary pseudo-nodes (`input`/`output`/
//! `error`) are spliced onto the instance's outer edges. Runs after
//! [`validate_policy`](crate::graph::validate_policy) and before
//! [`compile_policy`](crate::graph::compile_policy); the engine never sees
//! a `supernode` node type.

use std::collections::{HashMap, HashSet};

use crate::config::{EdgeConfig, NodeConfig, PolicyConfig, SupernodeConfig};

/// Reserved boundary pseudo-node types (and required ids) in a definition.
/// Consumed by [`expand_policy`] and downstream tasks.
#[allow(dead_code)]
pub(crate) const BOUNDARY_TYPES: [&str; 3] = ["input", "output", "error"];

/// Splits `node_id.port` on the **last** dot, defaulting the port to `out`.
/// Mirror of `engine::parse_edge_endpoint` — the two must agree.
/// Consumed by [`expand_policy`] and downstream tasks.
#[allow(dead_code)]
pub(crate) fn split_endpoint(s: &str) -> (&str, &str) {
    match s.rfind('.') {
        Some(i) => (&s[..i], &s[i + 1..]),
        None => (s, "out"),
    }
}

/// Inlines every `type: supernode` node of `policy` using `supernodes`.
///
/// Splicing rules (spec §2):
/// - outer `X.p -> inst.in` is redirected to the target of the definition's
///   `input.out` edge (prefixed);
/// - inner edges into `output` are redirected to the target of the outer
///   `inst.success`/`inst.out` edge, or dropped when it is unwired
///   (end-of-chain);
/// - inner edges into `error` likewise follow the outer `inst.error` edge,
///   or are dropped (policy catch-all);
/// - every inner node with no error edge of its own gets an implicit error
///   edge to the outer error target when one is wired (black-box guarantee).
///
/// Consumed by [`compile_policy`] at graph-compilation time.
#[allow(dead_code)]
pub fn expand_policy(
    policy: &PolicyConfig,
    supernodes: &[SupernodeConfig],
) -> Result<PolicyConfig, String> {
    let instances: Vec<&NodeConfig> = policy
        .nodes
        .iter()
        .filter(|n| n.node_type == "supernode")
        .collect();
    if instances.is_empty() {
        return Ok(policy.clone());
    }

    let by_name: HashMap<&str, &SupernodeConfig> =
        supernodes.iter().map(|s| (s.name.as_str(), s)).collect();

    /// Boundary wiring resolved per instance.
    struct Splice<'a> {
        def: &'a SupernodeConfig,
        /// Boundary id -> type map for the definition (e.g., "input" -> "input").
        boundary_map: HashMap<String, String>,
        /// Prefixed entry endpoint, e.g. `sec/auth.in`, or None for pass-through boundaries.
        entry: Option<String>,
        /// `to` endpoint of the outer `inst.success`/`inst.out` edge.
        success_to: Option<String>,
        /// `to` endpoint of the outer `inst.error` edge.
        error_to: Option<String>,
    }

    let mut splices: HashMap<&str, Splice> = HashMap::new();
    for inst in &instances {
        let name = inst
            .config
            .get("name")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                format!(
                    "policy '{}': supernode node '{}' is missing config.name",
                    policy.name, inst.id
                )
            })?;
        let def = *by_name.get(name).ok_or_else(|| {
            format!(
                "policy '{}': node '{}' references unknown supernode '{}'",
                policy.name, inst.id, name
            )
        })?;

        // Reject nested supernodes (V1 limitation).
        if def.nodes.iter().any(|n| n.node_type == "supernode") {
            return Err(format!(
                "policy '{}': supernode '{}' contains nested supernode; nesting not supported in V1",
                policy.name, def.name
            ));
        }

        // Build boundary_map: node id -> boundary type.
        let mut boundary_map = HashMap::new();
        for n in &def.nodes {
            if BOUNDARY_TYPES.contains(&n.node_type.as_str()) {
                boundary_map.insert(n.id.clone(), n.node_type.clone());
            }
        }

        // Find the target of input.out edge.
        let entry_edge = def
            .edges
            .iter()
            .find(|e| split_endpoint(&e.from).0 == "input")
            .ok_or_else(|| format!("supernode '{}' has no edge from input.out", def.name))?;
        let (entry_target, _) = split_endpoint(&entry_edge.to);

        // Check if entry target is a boundary (pass-through case).
        let entry = if boundary_map.contains_key(entry_target) {
            None
        } else {
            Some(format!("{}/{}.in", inst.id, entry_target))
        };

        let mut success_to = None;
        let mut error_to = None;
        for e in &policy.edges {
            let (from_node, from_port) = split_endpoint(&e.from);
            if from_node == inst.id {
                match from_port {
                    "success" | "out" => success_to = Some(e.to.clone()),
                    "error" => error_to = Some(e.to.clone()),
                    other => {
                        return Err(format!(
                            "policy '{}': unknown port '{}' on supernode node '{}'",
                            policy.name, other, inst.id
                        ))
                    }
                }
            }
        }
        splices.insert(
            inst.id.as_str(),
            Splice {
                def,
                boundary_map,
                entry,
                success_to,
                error_to,
            },
        );
    }

    // Detect pass-through cycles: if pass-through instance A points to pass-through
    // instance B which points back to A, reject the cycle.
    for inst_id in splices.keys() {
        let mut path = vec![*inst_id];
        let mut curr = *inst_id;
        while let Some(s) = splices.get(curr) {
            if s.entry.is_none() {
                // This is a pass-through; check where it leads.
                if let Some(target_endpoint) = &s.success_to {
                    let (target_node, _) = split_endpoint(target_endpoint);
                    if splices.contains_key(target_node) {
                        if path.contains(&target_node) {
                            let cycle_str = path
                                .iter()
                                .chain(std::iter::once(&target_node))
                                .copied()
                                .collect::<Vec<_>>()
                                .join(" -> ");
                            return Err(format!(
                                "policy '{}': supernode pass-through cycle: {}",
                                policy.name, cycle_str
                            ));
                        }
                        path.push(target_node);
                        curr = target_node;
                    } else {
                        break;
                    }
                } else {
                    break;
                }
            } else {
                break;
            }
        }
    }

    // Non-instance nodes survive as-is; instance nodes are replaced below.
    let mut nodes: Vec<NodeConfig> = policy
        .nodes
        .iter()
        .filter(|n| n.node_type != "supernode")
        .cloned()
        .collect();

    // Outer edges: those leaving an instance are replaced by inner exit
    // edges; those entering one are redirected to its entry node or pass-through target.
    let mut edges: Vec<EdgeConfig> = Vec::new();
    for e in &policy.edges {
        let (from_node, _) = split_endpoint(&e.from);
        if splices.contains_key(from_node) {
            continue;
        }
        let (to_node, _) = split_endpoint(&e.to);
        match splices.get(to_node) {
            Some(s) => {
                if let Some(entry) = &s.entry {
                    edges.push(EdgeConfig {
                        from: e.from.clone(),
                        to: entry.clone(),
                    });
                } else {
                    // Pass-through: redirect to success target or drop.
                    if let Some(t) = &s.success_to {
                        edges.push(EdgeConfig {
                            from: e.from.clone(),
                            to: t.clone(),
                        });
                    }
                }
            }
            None => edges.push(e.clone()),
        }
    }

    for inst in &instances {
        let s = &splices[inst.id.as_str()];
        if s.entry.is_none() {
            // Pass-through supernode: skip inner edge processing; they're handled above.
            continue;
        }

        let prefix = |id: &str| format!("{}/{}", inst.id, id);

        // Resolve target if it points to another instance.
        let resolve_target = |target: &str| -> String {
            let (target_node, _) = split_endpoint(target);
            if let Some(target_splice) = splices.get(target_node) {
                if let Some(entry) = &target_splice.entry {
                    entry.clone()
                } else {
                    // Target is pass-through; use its success_to.
                    target_splice
                        .success_to
                        .as_ref()
                        .map(|t| t.to_string())
                        .unwrap_or_else(|| target.to_string())
                }
            } else {
                target.to_string()
            }
        };

        // Inner nodes whose error port is wired inside the definition.
        let handled_errors: HashSet<&str> = s
            .def
            .edges
            .iter()
            .filter(|e| split_endpoint(&e.from).1 == "error")
            .map(|e| split_endpoint(&e.from).0)
            .collect();

        for n in &s.def.nodes {
            if BOUNDARY_TYPES.contains(&n.node_type.as_str()) {
                continue;
            }
            let mut inlined = n.clone();
            inlined.id = prefix(&n.id);
            inlined.position = None;
            nodes.push(inlined);
        }

        for e in &s.def.edges {
            let (from_node, from_port) = split_endpoint(&e.from);
            let (to_node, to_port) = split_endpoint(&e.to);
            if from_node == "input" {
                continue; // spliced via the outer in-edge above
            }
            let from = format!("{}.{}", prefix(from_node), from_port);

            // Determine the target type using boundary_map.
            let to_node_type = s.boundary_map.get(to_node).map(|t| t.as_str());
            match to_node_type {
                Some("output") => {
                    if let Some(t) = &s.success_to {
                        edges.push(EdgeConfig {
                            from,
                            to: resolve_target(t),
                        });
                    }
                }
                Some("error") => {
                    if let Some(t) = &s.error_to {
                        edges.push(EdgeConfig {
                            from,
                            to: resolve_target(t),
                        });
                    }
                }
                _ => edges.push(EdgeConfig {
                    from,
                    to: format!("{}.{}", prefix(to_node), to_port),
                }),
            }
        }

        // Black-box guarantee: unwired inner error ports exit through the
        // instance's error output when the policy connected one.
        if let Some(t) = &s.error_to {
            for n in &s.def.nodes {
                if BOUNDARY_TYPES.contains(&n.node_type.as_str())
                    || handled_errors.contains(n.id.as_str())
                {
                    continue;
                }
                edges.push(EdgeConfig {
                    from: format!("{}.error", prefix(&n.id)),
                    to: resolve_target(t),
                });
            }
        }
    }

    Ok(PolicyConfig {
        name: policy.name.clone(),
        error_handler: policy.error_handler.clone(),
        nodes,
        edges,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap as Map;

    fn node(id: &str, ty: &str) -> NodeConfig {
        NodeConfig {
            id: id.into(),
            node_type: ty.into(),
            config: Map::new(),
            position: None,
        }
    }

    fn supernode_instance(id: &str, name: &str) -> NodeConfig {
        let mut n = node(id, "supernode");
        n.config.insert("name".into(), serde_json::json!(name));
        n
    }

    fn edge(from: &str, to: &str) -> EdgeConfig {
        EdgeConfig {
            from: from.into(),
            to: to.into(),
        }
    }

    /// auth -> up, auth errors exit via the error boundary, up.success exits
    /// via output; up's error port is deliberately unwired (black-box test).
    fn secured_call() -> SupernodeConfig {
        SupernodeConfig {
            name: "secured-call".into(),
            description: None,
            nodes: vec![
                node("input", "input"),
                node("output", "output"),
                node("error", "error"),
                node("auth", "key-auth"),
                node("up", "upstream"),
            ],
            edges: vec![
                edge("input.out", "auth.in"),
                edge("auth.success", "up.in"),
                edge("auth.error", "error.in"),
                edge("up.success", "output.in"),
            ],
        }
    }

    fn policy_using(instance: &str) -> PolicyConfig {
        PolicyConfig {
            name: "p".into(),
            error_handler: None,
            nodes: vec![
                node("listener", "listener"),
                supernode_instance(instance, "secured-call"),
                node("eh", "error-handler"),
                node("client", "client"),
            ],
            edges: vec![
                edge("listener.out", &format!("{instance}.in")),
                edge(&format!("{instance}.success"), "client.in"),
                edge(&format!("{instance}.error"), "eh.in"),
                edge("eh.success", "client.in"),
            ],
        }
    }

    fn edge_set(p: &PolicyConfig) -> Vec<String> {
        let mut v: Vec<String> = p
            .edges
            .iter()
            .map(|e| format!("{}->{}", e.from, e.to))
            .collect();
        v.sort();
        v
    }

    #[test]
    fn test_policy_without_instances_is_unchanged() {
        let p = PolicyConfig {
            name: "plain".into(),
            error_handler: None,
            nodes: vec![node("listener", "listener"), node("client", "client")],
            edges: vec![edge("listener.out", "client.in")],
        };
        let out = expand_policy(&p, &[secured_call()]).unwrap();
        assert_eq!(out.nodes.len(), 2);
        assert_eq!(edge_set(&out), vec!["listener.out->client.in"]);
    }

    #[test]
    fn test_happy_path_inlines_and_splices() {
        let out = expand_policy(&policy_using("sec"), &[secured_call()]).unwrap();

        let ids: HashSet<&str> = out.nodes.iter().map(|n| n.id.as_str()).collect();
        assert!(ids.contains("sec/auth") && ids.contains("sec/up"));
        assert!(!ids.contains("sec"), "instance node must be removed");
        assert!(
            !out.nodes
                .iter()
                .any(|n| BOUNDARY_TYPES.contains(&n.node_type.as_str())),
            "boundary pseudo-nodes must not leak into the expanded policy"
        );

        assert_eq!(
            edge_set(&out),
            vec![
                "eh.success->client.in",
                "listener.out->sec/auth.in",   // outer in-edge -> entry
                "sec/auth.error->eh.in",       // error boundary -> outer error target
                "sec/auth.success->sec/up.in", // inner edge, prefixed
                "sec/up.error->eh.in",         // implicit black-box error edge
                "sec/up.success->client.in",   // output boundary -> outer success target
            ]
        );
    }

    /// Unwired outer ports: success exit edges are dropped (end-of-chain),
    /// error boundary edges are dropped (policy catch-all), and NO implicit
    /// error edges are added.
    #[test]
    fn test_unwired_outer_ports_drop_exit_edges() {
        let mut p = policy_using("sec");
        p.edges.retain(|e| !e.from.starts_with("sec.")); // keep only listener->sec.in, eh edge
        let out = expand_policy(&p, &[secured_call()]).unwrap();
        assert!(!out.edges.iter().any(|e| e.from == "sec/up.success"));
        assert!(!out.edges.iter().any(|e| e.from == "sec/auth.error"));
        assert!(!out.edges.iter().any(|e| e.from == "sec/up.error"));
    }

    #[test]
    fn test_two_instances_of_same_supernode_get_distinct_namespaces() {
        let p = PolicyConfig {
            name: "p2".into(),
            error_handler: None,
            nodes: vec![
                node("listener", "listener"),
                supernode_instance("a", "secured-call"),
                supernode_instance("b", "secured-call"),
                node("client", "client"),
            ],
            edges: vec![
                edge("listener.out", "a.in"),
                edge("a.success", "b.in"),
                edge("b.success", "client.in"),
            ],
        };
        let out = expand_policy(&p, &[secured_call()]).unwrap();
        let ids: HashSet<&str> = out.nodes.iter().map(|n| n.id.as_str()).collect();
        for id in ["a/auth", "a/up", "b/auth", "b/up"] {
            assert!(ids.contains(id), "missing {id}");
        }
        // a's output boundary must splice into b's entry node.
        assert!(out
            .edges
            .iter()
            .any(|e| e.from == "a/up.success" && e.to == "b/auth.in"));
    }

    #[test]
    fn test_unknown_supernode_is_an_error() {
        let p = PolicyConfig {
            name: "p".into(),
            error_handler: None,
            nodes: vec![
                node("listener", "listener"),
                supernode_instance("sec", "nope"),
                node("client", "client"),
            ],
            edges: vec![
                edge("listener.out", "sec.in"),
                edge("sec.success", "client.in"),
            ],
        };
        let err = expand_policy(&p, &[secured_call()]).unwrap_err();
        assert!(err.contains("unknown supernode 'nope'"), "got: {err}");
        assert!(err.contains("'sec'"), "got: {err}");
    }

    #[test]
    fn test_missing_config_name_is_an_error() {
        let p = PolicyConfig {
            name: "p".into(),
            error_handler: None,
            nodes: vec![
                node("listener", "listener"),
                node("sec", "supernode"), // no config.name
                node("client", "client"),
            ],
            edges: vec![
                edge("listener.out", "sec.in"),
                edge("sec.success", "client.in"),
            ],
        };
        let err = expand_policy(&p, &[secured_call()]).unwrap_err();
        assert!(err.contains("missing config.name"), "got: {err}");
    }

    /// Positions are UI-only and meaningless after inlining.
    #[test]
    fn test_inner_positions_are_dropped() {
        let mut def = secured_call();
        for n in &mut def.nodes {
            n.position = Some(crate::config::Position { x: 1.0, y: 2.0 });
        }
        let out = expand_policy(&policy_using("sec"), &[def]).unwrap();
        assert!(out
            .nodes
            .iter()
            .filter(|n| n.id.starts_with("sec/"))
            .all(|n| n.position.is_none()));
    }

    /// Pass-through identity supernode: input.out -> output.in (minimal).
    /// UI seeds newly created supernodes exactly like this.
    fn identity_supernode() -> SupernodeConfig {
        SupernodeConfig {
            name: "identity".into(),
            description: None,
            nodes: vec![
                node("input", "input"),
                node("output", "output"),
                node("error", "error"),
            ],
            edges: vec![edge("input.out", "output.in")],
        }
    }

    #[test]
    fn test_pass_through_identity_with_wired_outer_success() {
        let p = PolicyConfig {
            name: "p".into(),
            error_handler: None,
            nodes: vec![
                node("listener", "listener"),
                supernode_instance("pass", "identity"),
                node("client", "client"),
            ],
            edges: vec![
                edge("listener.out", "pass.in"),
                edge("pass.success", "client.in"),
            ],
        };
        let out = expand_policy(&p, &[identity_supernode()]).unwrap();
        // Instance node must be removed; no inner nodes inlined.
        assert!(!out.nodes.iter().any(|n| n.id.contains("pass")));
        // Outer edge redirected to outer success target.
        assert!(out
            .edges
            .iter()
            .any(|e| e.from == "listener.out" && e.to == "client.in"));
    }

    #[test]
    fn test_pass_through_identity_with_unwired_outer_success() {
        let p = PolicyConfig {
            name: "p".into(),
            error_handler: None,
            nodes: vec![
                node("listener", "listener"),
                supernode_instance("pass", "identity"),
                node("client", "client"),
            ],
            edges: vec![
                edge("listener.out", "pass.in"),
                edge("pass.error", "client.in"),
            ],
        };
        let out = expand_policy(&p, &[identity_supernode()]).unwrap();
        // Outer success edge unwired; outer in-edge is dropped.
        assert!(!out.edges.iter().any(|e| e.from == "listener.out"));
    }

    #[test]
    fn test_pass_through_cycle_is_error() {
        let p = PolicyConfig {
            name: "p".into(),
            error_handler: None,
            nodes: vec![
                node("listener", "listener"),
                supernode_instance("a", "identity"),
                supernode_instance("b", "identity"),
                node("client", "client"),
            ],
            edges: vec![
                edge("listener.out", "a.in"),
                edge("a.success", "b.in"),
                edge("b.success", "a.in"), // Cycle: b -> a -> b
                edge("a.error", "client.in"),
            ],
        };
        let err = expand_policy(&p, &[identity_supernode()]).unwrap_err();
        assert!(err.contains("pass-through cycle"), "got: {err}");
        assert!(err.contains("a") && err.contains("b"), "got: {err}");
    }

    /// Boundary with mismatched id: node type is "output" but id is "out1".
    /// Splicing must use node type, not id.
    #[test]
    fn test_boundary_by_type_with_mismatched_id() {
        let def = SupernodeConfig {
            name: "custom-boundary".into(),
            description: None,
            nodes: vec![
                node("input", "input"),
                node("out1", "output"), // Mismatched: id != type
                node("err1", "error"),
                node("process", "key-auth"),
            ],
            edges: vec![
                edge("input.out", "process.in"),
                edge("process.success", "out1.in"),
            ],
        };
        let p = PolicyConfig {
            name: "p".into(),
            error_handler: None,
            nodes: vec![
                node("listener", "listener"),
                supernode_instance("sb", "custom-boundary"),
                node("client", "client"),
            ],
            edges: vec![
                edge("listener.out", "sb.in"),
                edge("sb.success", "client.in"),
            ],
        };
        let out = expand_policy(&p, &[def]).unwrap();
        // Edge from process.success -> out1.in must splice to client.in.
        assert!(
            out.edges
                .iter()
                .any(|e| e.from == "sb/process.success" && e.to == "client.in"),
            "edges: {:?}",
            out.edges
                .iter()
                .map(|e| format!("{}->{}", e.from, e.to))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_nested_supernode_is_error() {
        let nested_def = SupernodeConfig {
            name: "nested".into(),
            description: None,
            nodes: vec![
                node("input", "input"),
                node("output", "output"),
                node("error", "error"),
                supernode_instance("inner", "identity"), // Nested supernode!
            ],
            edges: vec![
                edge("input.out", "inner.in"),
                edge("inner.success", "output.in"),
            ],
        };
        let p = PolicyConfig {
            name: "p".into(),
            error_handler: None,
            nodes: vec![
                node("listener", "listener"),
                supernode_instance("n", "nested"),
                node("client", "client"),
            ],
            edges: vec![edge("listener.out", "n.in"), edge("n.success", "client.in")],
        };
        let err = expand_policy(&p, &[nested_def, identity_supernode()]).unwrap_err();
        assert!(
            err.contains("nested") && err.contains("nesting not supported"),
            "got: {err}"
        );
    }
}
