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
        /// The node id of the input boundary (e.g., "input" or "in1" if {id: in1, type: input}).
        input_node_id: String,
        /// Prefixed entry endpoint, e.g. `sec/auth.in`, or None for pass-through boundaries.
        entry: Option<String>,
        /// For pass-through instances: which boundary type the entry resolves to ("output" or "error").
        /// None if not a pass-through.
        pass_through_type: Option<String>,
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

        // Find the input boundary node (by type, not literal id).
        let input_node_id = boundary_map
            .iter()
            .find(|(_, ty)| ty.as_str() == "input")
            .map(|(id, _)| id.clone())
            .ok_or_else(|| format!("supernode '{}' has no input boundary node", def.name))?;

        // Find the target of input.out edge.
        let entry_edge = def
            .edges
            .iter()
            .find(|e| split_endpoint(&e.from).0 == input_node_id.as_str())
            .ok_or_else(|| {
                format!(
                    "supernode '{}' has no edge from input node '{}'",
                    def.name,
                    input_node_id.as_str()
                )
            })?;
        let (entry_target, _) = split_endpoint(&entry_edge.to);

        // Check if entry target is a boundary (pass-through case) and track which type.
        let (entry, pass_through_type) = if let Some(boundary_type) = boundary_map.get(entry_target)
        {
            (None, Some(boundary_type.clone()))
        } else {
            (Some(format!("{}/{}.in", inst.id, entry_target)), None)
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
                input_node_id,
                entry,
                pass_through_type,
                success_to,
                error_to,
            },
        );
    }

    // Resolves `target` (an edge endpoint) to the concrete endpoint it
    // ultimately reaches, iterating through chained pass-through instances
    // to a fixed point (a non-instance endpoint, or a normal instance's
    // already-resolved entry). `Ok(None)` means the chain terminates in an
    // unwired outer port, so the edge referencing `target` must be dropped.
    // `Err` means the chain cycles back on an instance already visited.
    fn resolve_target(
        splices: &HashMap<&str, Splice>,
        target: &str,
    ) -> Result<Option<String>, String> {
        let mut current = target.to_string();
        // Ordered so a cycle error can name the full loop, not just the
        // repeated node.
        let mut path: Vec<String> = Vec::new();
        loop {
            let (target_node, _) = split_endpoint(&current);
            let s = match splices.get(target_node) {
                Some(s) => s,
                None => return Ok(Some(current)),
            };
            if let Some(entry) = &s.entry {
                // Normal instance: entry is already a concrete inlined node.
                return Ok(Some(entry.clone()));
            }
            let pass_type = match &s.pass_through_type {
                Some(t) => t,
                None => return Ok(Some(current)),
            };
            if path.iter().any(|p| p == target_node) {
                path.push(target_node.to_string());
                return Err(format!(
                    "supernode pass-through cycle: {}",
                    path.join(" -> ")
                ));
            }
            path.push(target_node.to_string());
            let next = match pass_type.as_str() {
                "output" => &s.success_to,
                "error" => &s.error_to,
                _ => &None,
            };
            match next {
                Some(t) => current = t.clone(),
                None => return Ok(None),
            }
        }
    }

    // Proactively walk every pass-through instance's chain so a cycle is
    // reported even if no outer edge happens to traverse it.
    for (id, s) in &splices {
        if s.pass_through_type.is_some() {
            resolve_target(&splices, &format!("{id}.in"))
                .map_err(|err| format!("policy '{}': {}", policy.name, err))?;
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
    // edges; those entering one are redirected to its resolved entry point,
    // following chained pass-throughs to a fixed point.
    let mut edges: Vec<EdgeConfig> = Vec::new();
    for e in &policy.edges {
        let (from_node, _) = split_endpoint(&e.from);
        if splices.contains_key(from_node) {
            continue;
        }
        let (to_node, _) = split_endpoint(&e.to);
        if splices.contains_key(to_node) {
            if let Some(resolved) = resolve_target(&splices, &e.to)
                .map_err(|err| format!("policy '{}': {}", policy.name, err))?
            {
                edges.push(EdgeConfig {
                    from: e.from.clone(),
                    to: resolved,
                });
            }
        } else {
            edges.push(e.clone());
        }
    }

    for inst in &instances {
        let s = &splices[inst.id.as_str()];
        if s.pass_through_type.is_some() {
            // Pass-through supernode: skip inner edge processing; they're handled above.
            continue;
        }

        let prefix = |id: &str| format!("{}/{}", inst.id, id);

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
            if from_node == s.input_node_id.as_str() {
                continue; // spliced via the outer in-edge above
            }
            let from = format!("{}.{}", prefix(from_node), from_port);

            // Determine the target type using boundary_map.
            let to_node_type = s.boundary_map.get(to_node).map(|t| t.as_str());
            match to_node_type {
                Some("output") => {
                    if let Some(t) = &s.success_to {
                        if let Some(resolved) = resolve_target(&splices, t)
                            .map_err(|err| format!("policy '{}': {}", policy.name, err))?
                        {
                            edges.push(EdgeConfig { from, to: resolved });
                        }
                    }
                }
                Some("error") => {
                    if let Some(t) = &s.error_to {
                        if let Some(resolved) = resolve_target(&splices, t)
                            .map_err(|err| format!("policy '{}': {}", policy.name, err))?
                        {
                            edges.push(EdgeConfig { from, to: resolved });
                        }
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
            if let Some(resolved) = resolve_target(&splices, t)
                .map_err(|err| format!("policy '{}': {}", policy.name, err))?
            {
                for n in &s.def.nodes {
                    if BOUNDARY_TYPES.contains(&n.node_type.as_str())
                        || handled_errors.contains(n.id.as_str())
                    {
                        continue;
                    }
                    edges.push(EdgeConfig {
                        from: format!("{}.error", prefix(&n.id)),
                        to: resolved.clone(),
                    });
                }
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

    /// Error-boundary pass-through: input.out -> error.in (F3 fix).
    fn error_pass_through() -> SupernodeConfig {
        SupernodeConfig {
            name: "error-passthrough".into(),
            description: None,
            nodes: vec![
                node("input", "input"),
                node("output", "output"),
                node("error", "error"),
            ],
            edges: vec![edge("input.out", "error.in")],
        }
    }

    #[test]
    fn test_error_boundary_pass_through_with_wired_outer_error() {
        let p = PolicyConfig {
            name: "p".into(),
            error_handler: None,
            nodes: vec![
                node("listener", "listener"),
                supernode_instance("err_pass", "error-passthrough"),
                node("eh", "error-handler"),
            ],
            edges: vec![
                edge("listener.out", "err_pass.in"),
                edge("err_pass.error", "eh.in"),
            ],
        };
        let out = expand_policy(&p, &[error_pass_through()]).unwrap();
        // Instance node must be removed; no inner nodes inlined.
        assert!(!out.nodes.iter().any(|n| n.id.contains("err_pass")));
        // Outer in-edge must be redirected to error target.
        assert!(
            out.edges
                .iter()
                .any(|e| e.from == "listener.out" && e.to == "eh.in"),
            "edges: {:?}",
            out.edges
                .iter()
                .map(|e| format!("{}->{}", e.from, e.to))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_error_boundary_pass_through_with_unwired_outer_error() {
        let p = PolicyConfig {
            name: "p".into(),
            error_handler: None,
            nodes: vec![
                node("listener", "listener"),
                supernode_instance("err_pass", "error-passthrough"),
                node("client", "client"),
            ],
            edges: vec![
                edge("listener.out", "err_pass.in"),
                edge("err_pass.success", "client.in"), // Only success wired, not error
            ],
        };
        let out = expand_policy(&p, &[error_pass_through()]).unwrap();
        // Outer in-edge is dropped because error target is unwired.
        assert!(!out.edges.iter().any(|e| e.from == "listener.out"));
    }

    /// Mismatched-id input boundary: {id: in1, type: input} (F4 fix).
    fn mismatched_input_id() -> SupernodeConfig {
        SupernodeConfig {
            name: "custom-input".into(),
            description: None,
            nodes: vec![
                node("in1", "input"), // id != type
                node("output", "output"),
                node("error", "error"),
                node("process", "upstream"),
            ],
            edges: vec![
                edge("in1.out", "process.in"),
                edge("process.success", "output.in"),
            ],
        }
    }

    #[test]
    fn test_mismatched_id_input_boundary_expands_correctly() {
        let p = PolicyConfig {
            name: "p".into(),
            error_handler: None,
            nodes: vec![
                node("listener", "listener"),
                supernode_instance("custom", "custom-input"),
                node("client", "client"),
            ],
            edges: vec![
                edge("listener.out", "custom.in"),
                edge("custom.success", "client.in"),
            ],
        };
        let out = expand_policy(&p, &[mismatched_input_id()]).unwrap();
        // Inlining must work: process node should be present.
        let ids: HashSet<&str> = out.nodes.iter().map(|n| n.id.as_str()).collect();
        assert!(ids.contains("custom/process"), "process node not inlined");
        assert!(
            !ids.contains("custom/in1"),
            "input boundary should not be inlined"
        );
        // Edge from process.success must splice to client.in.
        assert!(out
            .edges
            .iter()
            .any(|e| e.from == "custom/process.success" && e.to == "client.in"));
        // No edge endpoint should reference the input boundary id "in1" (GAP B).
        assert!(
            !out.edges
                .iter()
                .any(|e| e.from.contains("in1") || e.to.contains("in1")),
            "no edge should reference input boundary id 'in1'; edges: {:?}",
            out.edges
                .iter()
                .map(|e| format!("{}->{}", e.from, e.to))
                .collect::<Vec<_>>()
        );
    }

    /// Chain through pass-through into another instance (GAP A).
    /// Verifies that targets passed through splices are resolved via resolve_target.
    #[test]
    fn test_pass_through_chain_into_another_instance() {
        // x is an error pass-through, y is normal
        let p = PolicyConfig {
            name: "p".into(),
            error_handler: None,
            nodes: vec![
                node("listener", "listener"),
                supernode_instance("x", "error-passthrough"),
                supernode_instance("y", "secured-call"),
                node("eh", "error-handler"),
            ],
            edges: vec![
                edge("listener.out", "x.in"),
                edge("x.error", "y.in"), // x's error target is y's input
                edge("y.success", "eh.in"),
            ],
        };
        let out = expand_policy(&p, &[error_pass_through(), secured_call()]).unwrap();
        // Verify that listener.out is spliced to y's entry (y/auth.in), not y.in.
        assert!(
            out.edges
                .iter()
                .any(|e| e.from == "listener.out" && e.to == "y/auth.in"),
            "listener.out should splice to y/auth.in; edges: {:?}",
            out.edges
                .iter()
                .map(|e| format!("{}->{}", e.from, e.to))
                .collect::<Vec<_>>()
        );
        // y.in should NOT appear as a target (only y/auth.in).
        assert!(
            !out.edges.iter().any(|e| e.to == "y.in"),
            "no edge should target instance port y.in (dangling); edges: {:?}",
            out.edges
                .iter()
                .map(|e| format!("{}->{}", e.from, e.to))
                .collect::<Vec<_>>()
        );
    }

    /// Multi-hop pass-through chain (Round-3 re-review Finding 1): x and y
    /// are both error-pass-throughs, z is a normal instance. resolve_target
    /// must iterate through both pass-through hops to reach z's real entry,
    /// not stop after resolving just one hop.
    #[test]
    fn test_multi_hop_pass_through_chain_resolves_to_fixed_point() {
        let p = PolicyConfig {
            name: "p".into(),
            error_handler: None,
            nodes: vec![
                node("listener", "listener"),
                supernode_instance("x", "error-passthrough"),
                supernode_instance("y", "error-passthrough"),
                supernode_instance("z", "secured-call"),
                node("eh", "error-handler"),
            ],
            edges: vec![
                edge("listener.out", "x.in"),
                edge("x.error", "y.in"),
                edge("y.error", "z.in"),
                edge("z.success", "eh.in"),
            ],
        };
        let out = expand_policy(&p, &[error_pass_through(), secured_call()]).unwrap();
        let edge_strs: Vec<String> = out
            .edges
            .iter()
            .map(|e| format!("{}->{}", e.from, e.to))
            .collect();
        // listener.out must resolve all the way through x and y to z's real entry.
        assert!(
            out.edges
                .iter()
                .any(|e| e.from == "listener.out" && e.to == "z/auth.in"),
            "listener.out should splice to z/auth.in; edges: {edge_strs:?}"
        );
        // No edge may reference the bare (now-removed) instance ids.
        assert!(
            !out.edges.iter().any(|e| e.from == "x.in"
                || e.to == "x.in"
                || e.from == "y.in"
                || e.to == "y.in"
                || e.from == "z.in"
                || e.to == "z.in"),
            "no edge should reference a bare instance port; edges: {edge_strs:?}"
        );
    }

    /// Cyclic multi-hop pass-through chain: x -> y -> x, both error-pass-throughs.
    /// resolve_target's cycle guard must catch this even though it takes two
    /// hops to loop back, not just an immediate self-reference.
    #[test]
    fn test_multi_hop_pass_through_cycle_is_error() {
        let p = PolicyConfig {
            name: "p".into(),
            error_handler: None,
            nodes: vec![
                node("listener", "listener"),
                supernode_instance("x", "error-passthrough"),
                supernode_instance("y", "error-passthrough"),
            ],
            edges: vec![
                edge("listener.out", "x.in"),
                edge("x.error", "y.in"),
                edge("y.error", "x.in"), // cycle: x -> y -> x
            ],
        };
        let err = expand_policy(&p, &[error_pass_through()]).unwrap_err();
        assert!(err.contains("cycle"), "got: {err}");
    }
}
