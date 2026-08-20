//! Compile-time expansion of supernode instances into flat policies.
//!
//! A policy node of `type: supernode` (config `{ name: <supernode> }`) is
//! replaced by the referenced definition's inner nodes, ids namespaced
//! `<instance-id>/<inner-id>`. Boundary pseudo-nodes (`input`/`output`/
//! `error`) are spliced onto the instance's outer edges. Runs after
//! [`validate_policy`](crate::graph::validate_policy) and before
//! [`compile_policy`](crate::graph::compile_policy); the engine never sees
//! a `supernode` node type.
//!
//! A definition may declare one or more `output` boundary nodes and one or
//! more `error` boundary nodes; each boundary node's id names an instance
//! port. For `output` boundaries this goes through
//! [`port_for_output_boundary`] — the special id `output` keeps the
//! historical `success` mapping, any other id IS the port name. For `error`
//! boundaries the id IS the port name directly, with no such special-case
//! mapping — except that the id `error` is also the one the black-box rule
//! targets by default. So an instance's exit map is per-boundary: each
//! output/error boundary has its own `exit_to` target (the `to` of the
//! matching outer `inst.<port>` edge). Every output-derived port is
//! **mandatory-wired** — an unwired one is a hard compile error naming the
//! instance and port, because the instance node is gone before
//! post-expansion port validation runs and nothing downstream could catch
//! it otherwise. Error-kind boundaries stay optional: an unwired one just
//! drops the corresponding edges (the policy catch-all, or the generic 500,
//! takes over). The black-box guarantee — every inner node with no error
//! edge of its own gets an implicit error edge — follows ONLY the
//! `error`-id boundary; other named error boundaries carry no such default.

use std::collections::{HashMap, HashSet};

use crate::config::{EdgeConfig, NodeConfig, PolicyConfig, SupernodeConfig};

/// Reserved boundary pseudo-node types (and required ids) in a definition.
/// Consumed by [`expand_policy`] and downstream tasks.
pub(crate) const BOUNDARY_TYPES: [&str; 3] = ["input", "output", "error"];

/// Splits `node_id.port` on the **last** dot, defaulting the port to `out`.
/// Mirror of `engine::parse_edge_endpoint` — the two must agree.
/// Consumed by [`expand_policy`] and downstream tasks.
pub(crate) fn split_endpoint(s: &str) -> (&str, &str) {
    match s.rfind('.') {
        Some(i) => (&s[..i], &s[i + 1..]),
        None => (s, "out"),
    }
}

/// Instance port name exposed by an output boundary node: the special id
/// `output` keeps the historical `success` mapping; any other id is the
/// port name itself. Consumed by expansion and its tests; the UI mirrors
/// this mapping in ui/src/policyGraph.ts::supernodePortSpec.
pub(crate) fn port_for_output_boundary(boundary_id: &str) -> &str {
    if boundary_id == "output" {
        "success"
    } else {
        boundary_id
    }
}

/// Inlines every `type: supernode` node of `policy` using `supernodes`.
///
/// Splicing rules (spec §2):
/// - outer `X.p -> inst.in` is redirected to the target of the definition's
///   `input.out` edge (prefixed);
/// - an instance exposes one output port per `output` boundary node — named
///   via [`port_for_output_boundary`] — plus one port per `error` boundary
///   node, named directly by its id. `out` is accepted as an alias for the
///   `output`-id boundary's `success` port. Any other port name on an outer
///   edge leaving the instance is rejected, listing the exposed ports (output
///   ports, then error ports); so is a second edge from the same port;
/// - every output-derived port is mandatory-wired: an outer edge from
///   `inst.<port>` must exist for each `output` boundary, or expansion fails
///   naming the instance and port — the instance node is gone before
///   post-expansion port validation runs, so this is the only place that can
///   catch it;
/// - inner edges into an `output` boundary are redirected to the target of
///   that boundary's outer edge (always present, per the rule above);
/// - inner edges into an `error` boundary follow that boundary's outer
///   `inst.<port>` edge, or are dropped when it is unwired (the policy
///   catch-all, or the generic 500, takes over — error-kind ports are
///   genuinely optional);
/// - every inner node with no error edge of its own gets an implicit error
///   edge to the outer target of the DEFAULT `error`-id boundary, when one is
///   wired (black-box guarantee) — other named error boundaries carry no
///   such default.
///
/// Consumed by [`compile_policy`] at graph-compilation time.
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
        /// For pass-through instances: the boundary node id the entry edge targets.
        pass_through_boundary: Option<String>,
        /// Boundary node id -> `to` endpoint of the outer edge wired to its port.
        /// Mandatory-wiring guarantees an entry for every output boundary;
        /// error-kind boundaries' entries are optional.
        exit_to: HashMap<String, String>,
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

        // Check if entry target is a boundary (pass-through case).
        let (entry, pass_through_boundary) = if boundary_map.contains_key(entry_target) {
            (None, Some(entry_target.to_string()))
        } else {
            (Some(format!("{}/{}.in", inst.id, entry_target)), None)
        };

        let output_ids: Vec<String> = def
            .nodes
            .iter()
            .filter(|n| n.node_type == "output")
            .map(|n| n.id.clone())
            .collect();
        let error_ids: Vec<String> = def
            .nodes
            .iter()
            .filter(|n| n.node_type == "error")
            .map(|n| n.id.clone())
            .collect();

        let mut exit_to: HashMap<String, String> = HashMap::new();
        for e in &policy.edges {
            let (from_node, from_port) = split_endpoint(&e.from);
            if from_node != inst.id {
                continue;
            }
            let port = if from_port == "out" {
                "success"
            } else {
                from_port
            };
            let boundary_id = output_ids
                .iter()
                .find(|id| port_for_output_boundary(id) == port)
                .or_else(|| error_ids.iter().find(|id| id.as_str() == port))
                .cloned();
            let Some(bid) = boundary_id else {
                let mut exposed: Vec<&str> = output_ids
                    .iter()
                    .map(|id| port_for_output_boundary(id))
                    .collect();
                exposed.extend(error_ids.iter().map(|id| id.as_str()));
                return Err(format!(
                    "policy '{}': unknown port '{}' on supernode instance '{}' — \
                     supernode '{}' exposes: {}",
                    policy.name,
                    from_port,
                    inst.id,
                    def.name,
                    exposed.join(", ")
                ));
            };
            if exit_to.insert(bid, e.to.clone()).is_some() {
                return Err(format!(
                    "policy '{}': duplicate edge from supernode instance '{}' port '{}' — \
                     each instance port accepts one edge",
                    policy.name, inst.id, port
                ));
            }
        }

        // Every output-derived port is mandatory-wired, matching the compile-time
        // rule for plugin nodes (engine.rs). The instance node is gone before
        // compile runs, so this is the only place a clear error can be raised.
        for oid in &output_ids {
            if !exit_to.contains_key(oid.as_str()) {
                let port = port_for_output_boundary(oid);
                return Err(format!(
                    "policy '{}': output port '{}' of supernode instance '{}' must be \
                     wired — add an edge from '{}.{}'",
                    policy.name, port, inst.id, inst.id, port
                ));
            }
        }

        splices.insert(
            inst.id.as_str(),
            Splice {
                def,
                boundary_map,
                input_node_id,
                entry,
                pass_through_boundary,
                exit_to,
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
            let pass_boundary = match &s.pass_through_boundary {
                Some(b) => b,
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
            match s.exit_to.get(pass_boundary.as_str()) {
                Some(t) => current = t.clone(),
                None => return Ok(None), // only reachable for an unwired error-kind boundary
            }
        }
    }

    // Proactively walk every pass-through instance's chain so a cycle is
    // reported even if no outer edge happens to traverse it.
    for (id, s) in &splices {
        if s.pass_through_boundary.is_some() {
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
        if s.pass_through_boundary.is_some() {
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

            // Determine whether the target is a boundary node using boundary_map.
            let to_is_boundary = matches!(
                s.boundary_map.get(to_node).map(|t| t.as_str()),
                Some("output") | Some("error")
            );
            if to_is_boundary {
                if let Some(t) = s.exit_to.get(to_node) {
                    if let Some(resolved) = resolve_target(&splices, t)
                        .map_err(|err| format!("policy '{}': {}", policy.name, err))?
                    {
                        edges.push(EdgeConfig { from, to: resolved });
                    }
                }
                // Unwired error-kind boundary: drop (policy catch-all takes over).
                // Output boundaries are always wired — checked above.
            } else {
                edges.push(EdgeConfig {
                    from,
                    to: format!("{}.{}", prefix(to_node), to_port),
                });
            }
        }

        // Black-box guarantee: unwired inner error ports exit through the
        // instance's DEFAULT error output — only the `error`-id boundary
        // carries this guarantee. `exit_to` can only hold key "error" when
        // the definition has an `error`-id error boundary and the policy
        // wired it: output ids can't be `error` (reserved), and the lookup
        // above inserts entries keyed by boundary id.
        if let Some(t) = s.exit_to.get("error") {
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
            config_ref: None,
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

    /// The `success` output port is now mandatory-wired; leaving it unwired
    /// is a hard error naming the instance and port.
    #[test]
    fn test_unwired_success_exit_is_rejected() {
        let mut p = policy_using("sec");
        p.edges.retain(|e| !e.from.starts_with("sec.")); // keep only listener->sec.in, eh edge
        let err = expand_policy(&p, &[secured_call()]).unwrap_err();
        assert!(
            err.contains("output port 'success' of supernode instance 'sec' must be wired"),
            "got: {err}"
        );
    }

    /// An unwired **error** exit still just drops edges: no implicit error
    /// edges are added, but the rest of the chain (success) still splices.
    #[test]
    fn test_unwired_error_exit_drops_edges() {
        let mut p = policy_using("sec");
        p.edges.retain(|e| e.from != "sec.error"); // drop only sec.error edge
        p.edges.retain(|e| !e.from.starts_with("eh.")); // now-orphaned eh edge
        p.nodes.retain(|n| n.id != "eh"); // now-orphaned eh node
        let out = expand_policy(&p, &[secured_call()]).unwrap();
        assert!(!out
            .edges
            .iter()
            .any(|e| e.from.starts_with("sec/auth.error")));
        assert!(!out.edges.iter().any(|e| e.from.starts_with("sec/up.error")));
        assert!(out
            .edges
            .iter()
            .any(|e| e.from == "sec/up.success" && e.to == "client.in"));
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
    fn test_pass_through_identity_with_unwired_outer_success_is_rejected() {
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
        let err = expand_policy(&p, &[identity_supernode()]).unwrap_err();
        assert!(
            err.contains("output port 'success' of supernode instance 'pass'"),
            "got: {err}"
        );
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

    /// Boundary with a non-`output` id: node type is "output" but id is
    /// "out1" — the instance port name IS the boundary id, not "success".
    #[test]
    fn test_named_output_boundary_port_name_is_its_id() {
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
            edges: vec![edge("listener.out", "sb.in"), edge("sb.out1", "client.in")],
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

    /// Definition with a custom-named outcome port from an inner node to
    /// the `output` boundary: input -> auth -> ...; auth.denied -> output.
    /// (Until Task 8 lands no plugin type declares "denied"; expansion is
    /// syntactic and must not care whether the port is declared.)
    fn outcome_port_supernode() -> SupernodeConfig {
        SupernodeConfig {
            name: "outcome-def".into(),
            description: None,
            nodes: vec![
                node("input", "input"),
                node("output", "output"),
                node("error", "error"),
                node("auth", "key-auth"),
            ],
            edges: vec![
                edge("input.out", "auth.in"),
                edge("auth.denied", "output.in"),
            ],
        }
    }

    /// A named outcome port on an inner node survives expansion with the
    /// instance prefix, targeting the node the definition wired it to.
    #[test]
    fn test_inner_outcome_port_is_prefixed_and_preserved() {
        let p = PolicyConfig {
            name: "p".into(),
            error_handler: None,
            nodes: vec![
                node("listener", "listener"),
                supernode_instance("sec", "outcome-def"),
                node("eh", "error-handler"),
                node("client", "client"),
            ],
            edges: vec![
                edge("listener.out", "sec.in"),
                edge("sec.success", "client.in"),
                edge("sec.error", "eh.in"),
            ],
        };
        let out = expand_policy(&p, &[outcome_port_supernode()]).unwrap();
        assert_eq!(
            edge_set(&out),
            vec![
                "listener.out->sec/auth.in",
                "sec/auth.denied->client.in", // custom port, prefixed, follows output boundary
                "sec/auth.error->eh.in",      // black-box: auth has no error edge of its own
            ]
        );
    }

    /// An inner outcome port wired to the `output` boundary follows the outer
    /// success edge, same as inner success ports do today — even when the
    /// outer success and error targets are distinct nodes, "denied" must
    /// land on the success target, never the error one.
    #[test]
    fn test_inner_outcome_port_to_output_boundary() {
        let p = PolicyConfig {
            name: "p".into(),
            error_handler: None,
            nodes: vec![
                node("listener", "listener"),
                supernode_instance("sec", "outcome-def"),
                node("eh", "error-handler"),
                node("client", "client"),
            ],
            edges: vec![
                edge("listener.out", "sec.in"),
                edge("sec.success", "client.in"),
                edge("sec.error", "eh.in"),
            ],
        };
        let out = expand_policy(&p, &[outcome_port_supernode()]).unwrap();
        assert!(
            out.edges
                .iter()
                .any(|e| e.from == "sec/auth.denied" && e.to == "client.in"),
            "edges: {:?}",
            out.edges
                .iter()
                .map(|e| format!("{}->{}", e.from, e.to))
                .collect::<Vec<_>>()
        );
        assert!(
            !out.edges
                .iter()
                .any(|e| e.from == "sec/auth.denied" && e.to == "eh.in"),
            "custom outcome port must not be misrouted to the error target"
        );
    }

    /// A custom-named port on the OUTER edge leaving a supernode instance
    /// itself (`sec.denied -> ...`, as opposed to a port on an inner node)
    /// must be rejected. An instance exposes only the two exits its boundary
    /// pseudo-nodes define; outcome ports live on inner nodes and are wired
    /// to a boundary *inside* the definition. Silently treating an unknown
    /// name as `success` would let a typo (or a genuinely wrong port) rewire
    /// the whole subgraph's success exit, and the instance node is gone by
    /// the time compile-time port validation runs, so nothing downstream
    /// could catch it.
    #[test]
    fn test_outer_custom_port_on_instance_is_rejected() {
        let p = PolicyConfig {
            name: "p".into(),
            error_handler: None,
            nodes: vec![
                node("listener", "listener"),
                supernode_instance("sec", "secured-call"),
                node("client", "client"),
            ],
            edges: vec![
                edge("listener.out", "sec.in"),
                edge("sec.denied", "client.in"), // custom port, not "success"/"out"/"error"
            ],
        };
        let err = expand_policy(&p, &[secured_call()]).unwrap_err();
        assert!(
            err.contains("unknown port 'denied'")
                && err.contains("supernode instance 'sec'")
                && err.contains("success"),
            "got: {err}"
        );
    }

    /// `out` is the documented YAML alias for `success` on an instance's exit
    /// and must keep working.
    #[test]
    fn test_outer_out_alias_on_instance_is_accepted() {
        let p = PolicyConfig {
            name: "p".into(),
            error_handler: None,
            nodes: vec![
                node("listener", "listener"),
                supernode_instance("sec", "secured-call"),
                node("client", "client"),
            ],
            edges: vec![edge("listener.out", "sec.in"), edge("sec.out", "client.in")],
        };
        let out = expand_policy(&p, &[secured_call()]).unwrap();
        assert!(
            out.edges
                .iter()
                .any(|e| e.from == "sec/up.success" && e.to == "client.in"),
            "edges: {:?}",
            out.edges
                .iter()
                .map(|e| format!("{}->{}", e.from, e.to))
                .collect::<Vec<_>>()
        );
    }

    /// Two success-flavoured outer edges (`sec.success` + `sec.out`) used to
    /// silently last-write-wins, dropping one of them. Reject the duplicate.
    #[test]
    fn test_duplicate_success_outer_edge_on_instance_is_rejected() {
        let p = PolicyConfig {
            name: "p".into(),
            error_handler: None,
            nodes: vec![
                node("listener", "listener"),
                supernode_instance("sec", "secured-call"),
                node("eh", "error-handler"),
                node("client", "client"),
            ],
            edges: vec![
                edge("listener.out", "sec.in"),
                edge("sec.success", "client.in"),
                edge("sec.out", "eh.in"), // second success-flavoured exit
            ],
        };
        let err = expand_policy(&p, &[secured_call()]).unwrap_err();
        assert!(
            err.contains("duplicate edge") && err.contains("'sec'"),
            "got: {err}"
        );
    }

    /// Same for two `error` exits.
    #[test]
    fn test_duplicate_error_outer_edge_on_instance_is_rejected() {
        let p = PolicyConfig {
            name: "p".into(),
            error_handler: None,
            nodes: vec![
                node("listener", "listener"),
                supernode_instance("sec", "secured-call"),
                node("eh", "error-handler"),
                node("client", "client"),
            ],
            edges: vec![
                edge("listener.out", "sec.in"),
                edge("sec.success", "client.in"),
                edge("sec.error", "eh.in"),
                edge("sec.error", "client.in"),
            ],
        };
        let err = expand_policy(&p, &[secured_call()]).unwrap_err();
        assert!(
            err.contains("duplicate edge") && err.contains("error"),
            "got: {err}"
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
                node("client", "client"),
            ],
            edges: vec![
                edge("listener.out", "err_pass.in"),
                edge("err_pass.error", "eh.in"),
                edge("err_pass.success", "client.in"),
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
                edge("x.success", "eh.in"),
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
                edge("x.success", "eh.in"),
                edge("y.success", "eh.in"),
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
                node("client", "client"),
            ],
            edges: vec![
                edge("listener.out", "x.in"),
                edge("x.error", "y.in"),
                edge("y.error", "x.in"), // cycle: x -> y -> x
                edge("x.success", "client.in"),
                edge("y.success", "client.in"),
            ],
        };
        let err = expand_policy(&p, &[error_pass_through()]).unwrap_err();
        assert!(err.contains("cycle"), "got: {err}");
    }

    /// gate definition with two output boundaries: `output` (success) and `denied`.
    fn named_ports_supernode() -> SupernodeConfig {
        SupernodeConfig {
            name: "gate".into(),
            description: None,
            nodes: vec![
                node("input", "input"),
                node("output", "output"),
                node("denied", "output"),
                node("error", "error"),
                node("auth", "key-auth"),
            ],
            edges: vec![
                edge("input.out", "auth.in"),
                edge("auth.success", "output.in"),
                edge("auth.denied", "denied.in"),
            ],
        }
    }

    /// Named ports splice to their own outer targets: `gate.success` and
    /// `gate.denied` land on different nodes.
    #[test]
    fn test_named_output_ports_splice_to_distinct_targets() {
        let p = PolicyConfig {
            name: "p".into(),
            error_handler: None,
            nodes: vec![
                node("listener", "listener"),
                supernode_instance("gate", "gate"),
                node("reject", "error-handler"),
                node("client", "client"),
            ],
            edges: vec![
                edge("listener.out", "gate.in"),
                edge("gate.success", "client.in"),
                edge("gate.denied", "reject.in"),
                edge("reject.success", "client.in"),
            ],
        };
        let out = expand_policy(&p, &[named_ports_supernode()]).unwrap();
        assert_eq!(
            edge_set(&out),
            vec![
                "gate/auth.denied->reject.in",
                "gate/auth.success->client.in",
                "listener.out->gate/auth.in",
                "reject.success->client.in",
            ]
        );
    }

    /// An unwired named port is a hard error naming the instance and port.
    #[test]
    fn test_unwired_named_port_is_rejected() {
        let p = PolicyConfig {
            name: "p".into(),
            error_handler: None,
            nodes: vec![
                node("listener", "listener"),
                supernode_instance("gate", "gate"),
                node("client", "client"),
            ],
            edges: vec![
                edge("listener.out", "gate.in"),
                edge("gate.success", "client.in"),
                // gate.denied deliberately unwired
            ],
        };
        let err = expand_policy(&p, &[named_ports_supernode()]).unwrap_err();
        assert!(
            err.contains("output port 'denied' of supernode instance 'gate' must be wired")
                && err.contains("add an edge from 'gate.denied'"),
            "got: {err}"
        );
    }

    /// `success` is mandatory too when an `output`-id boundary exists.
    #[test]
    fn test_unwired_success_port_is_rejected() {
        let p = PolicyConfig {
            name: "p".into(),
            error_handler: None,
            nodes: vec![
                node("listener", "listener"),
                supernode_instance("gate", "gate"),
                node("reject", "error-handler"),
                node("client", "client"),
            ],
            edges: vec![
                edge("listener.out", "gate.in"),
                edge("gate.denied", "reject.in"),
                edge("reject.success", "client.in"),
            ],
        };
        let err = expand_policy(&p, &[named_ports_supernode()]).unwrap_err();
        assert!(
            err.contains("output port 'success' of supernode instance 'gate' must be wired"),
            "got: {err}"
        );
    }

    /// A definition with only named outputs exposes no `success` port at all.
    fn named_only_supernode() -> SupernodeConfig {
        SupernodeConfig {
            name: "named-only".into(),
            description: None,
            nodes: vec![
                node("input", "input"),
                node("done", "output"),
                node("error", "error"),
                node("up", "upstream"),
            ],
            edges: vec![edge("input.out", "up.in"), edge("up.success", "done.in")],
        }
    }

    #[test]
    fn test_success_rejected_when_no_output_id_boundary() {
        let p = PolicyConfig {
            name: "p".into(),
            error_handler: None,
            nodes: vec![
                node("listener", "listener"),
                supernode_instance("n", "named-only"),
                node("client", "client"),
            ],
            edges: vec![edge("listener.out", "n.in"), edge("n.success", "client.in")],
        };
        let err = expand_policy(&p, &[named_only_supernode()]).unwrap_err();
        assert!(
            err.contains("unknown port 'success'") && err.contains("done"),
            "unknown-port error must list the exposed ports; got: {err}"
        );
    }

    #[test]
    fn test_named_only_supernode_routes_via_named_port() {
        let p = PolicyConfig {
            name: "p".into(),
            error_handler: None,
            nodes: vec![
                node("listener", "listener"),
                supernode_instance("n", "named-only"),
                node("client", "client"),
            ],
            edges: vec![edge("listener.out", "n.in"), edge("n.done", "client.in")],
        };
        let out = expand_policy(&p, &[named_only_supernode()]).unwrap();
        assert!(out
            .edges
            .iter()
            .any(|e| e.from == "n/up.success" && e.to == "client.in"));
    }

    /// Pass-through via a NAMED boundary: input.out -> denied.in resolves the
    /// outer in-edge to the target wired on the instance's `denied` port.
    #[test]
    fn test_pass_through_via_named_boundary() {
        let def = SupernodeConfig {
            name: "shortcut".into(),
            description: None,
            nodes: vec![
                node("input", "input"),
                node("denied", "output"),
                node("error", "error"),
            ],
            edges: vec![edge("input.out", "denied.in")],
        };
        let p = PolicyConfig {
            name: "p".into(),
            error_handler: None,
            nodes: vec![
                node("listener", "listener"),
                supernode_instance("s", "shortcut"),
                node("client", "client"),
            ],
            edges: vec![edge("listener.out", "s.in"), edge("s.denied", "client.in")],
        };
        let out = expand_policy(&p, &[def]).unwrap();
        assert!(out
            .edges
            .iter()
            .any(|e| e.from == "listener.out" && e.to == "client.in"));
    }

    /// Two edges on the same named port are duplicates.
    #[test]
    fn test_duplicate_named_port_outer_edge_is_rejected() {
        let p = PolicyConfig {
            name: "p".into(),
            error_handler: None,
            nodes: vec![
                node("listener", "listener"),
                supernode_instance("gate", "gate"),
                node("reject", "error-handler"),
                node("client", "client"),
            ],
            edges: vec![
                edge("listener.out", "gate.in"),
                edge("gate.success", "client.in"),
                edge("gate.denied", "reject.in"),
                edge("gate.denied", "client.in"),
                edge("reject.success", "client.in"),
            ],
        };
        let err = expand_policy(&p, &[named_ports_supernode()]).unwrap_err();
        assert!(
            err.contains("duplicate edge") && err.contains("'denied'"),
            "got: {err}"
        );
    }

    /// gate with two error boundaries: `error` (default) and `auth-error`.
    /// `auth.error` exits via `auth-error`; `up` has no error edge (black-box).
    fn multi_error_supernode() -> SupernodeConfig {
        SupernodeConfig {
            name: "gate".into(),
            description: None,
            nodes: vec![
                node("input", "input"),
                node("output", "output"),
                node("error", "error"),
                node("auth-error", "error"),
                node("auth", "key-auth"),
                node("up", "upstream"),
            ],
            edges: vec![
                edge("input.out", "auth.in"),
                edge("auth.success", "up.in"),
                edge("auth.denied", "up.in"),
                edge("auth.error", "auth-error.in"),
                edge("up.success", "output.in"),
            ],
        }
    }

    /// Named error ports route to their own targets; the black-box rule follows
    /// ONLY the default `error` port.
    #[test]
    fn test_named_error_port_and_default_black_box() {
        let p = PolicyConfig {
            name: "p".into(),
            error_handler: None,
            nodes: vec![
                node("listener", "listener"),
                supernode_instance("gate", "gate"),
                node("eh1", "error-handler"),
                node("eh2", "error-handler"),
                node("client", "client"),
            ],
            edges: vec![
                edge("listener.out", "gate.in"),
                edge("gate.success", "client.in"),
                edge("gate.error", "eh1.in"),
                edge("gate.auth-error", "eh2.in"),
                edge("eh1.success", "client.in"),
                edge("eh2.success", "client.in"),
            ],
        };
        let out = expand_policy(&p, &[multi_error_supernode()]).unwrap();
        // auth's own error edge follows the named boundary to eh2.
        assert!(out
            .edges
            .iter()
            .any(|e| e.from == "gate/auth.error" && e.to == "eh2.in"));
        // up has no error edge: black-box wires it to the DEFAULT error target.
        assert!(out
            .edges
            .iter()
            .any(|e| e.from == "gate/up.error" && e.to == "eh1.in"));
        // auth is handled inside the definition — no additional black-box edge.
        assert_eq!(
            out.edges
                .iter()
                .filter(|e| e.from == "gate/auth.error")
                .count(),
            1
        );
    }

    /// Definition whose only error boundary is named `oops`: no `error` port
    /// exists, and there is NO implicit black-box wiring.
    fn renamed_error_supernode() -> SupernodeConfig {
        SupernodeConfig {
            name: "renamed".into(),
            description: None,
            nodes: vec![
                node("input", "input"),
                node("output", "output"),
                node("oops", "error"),
                node("up", "upstream"),
            ],
            edges: vec![edge("input.out", "up.in"), edge("up.success", "output.in")],
        }
    }

    #[test]
    fn test_no_black_box_without_default_error_boundary() {
        let p = PolicyConfig {
            name: "p".into(),
            error_handler: None,
            nodes: vec![
                node("listener", "listener"),
                supernode_instance("r", "renamed"),
                node("eh", "error-handler"),
                node("client", "client"),
            ],
            edges: vec![
                edge("listener.out", "r.in"),
                edge("r.success", "client.in"),
                edge("r.oops", "eh.in"),
                edge("eh.success", "client.in"),
            ],
        };
        let out = expand_policy(&p, &[renamed_error_supernode()]).unwrap();
        // No implicit wiring: r/up.error stays unwired (policy catch-all).
        assert!(!out.edges.iter().any(|e| e.from == "r/up.error"));
    }

    /// With the default renamed away, port `error` is unknown — and the message
    /// lists the named error port.
    #[test]
    fn test_error_port_unknown_when_default_renamed() {
        let p = PolicyConfig {
            name: "p".into(),
            error_handler: None,
            nodes: vec![
                node("listener", "listener"),
                supernode_instance("r", "renamed"),
                node("client", "client"),
            ],
            edges: vec![
                edge("listener.out", "r.in"),
                edge("r.success", "client.in"),
                edge("r.error", "client.in"),
            ],
        };
        let err = expand_policy(&p, &[renamed_error_supernode()]).unwrap_err();
        assert!(
            err.contains("unknown port 'error'") && err.contains("oops"),
            "got: {err}"
        );
    }

    /// An unwired named error port just drops its exit edges (optional wiring).
    #[test]
    fn test_unwired_named_error_port_drops_exit_edges() {
        let mut def = renamed_error_supernode();
        def.nodes.push(node("auth", "key-auth"));
        def.edges = vec![
            edge("input.out", "auth.in"),
            edge("auth.success", "up.in"),
            edge("auth.denied", "up.in"),
            edge("auth.error", "oops.in"),
            edge("up.success", "output.in"),
        ];
        let p = PolicyConfig {
            name: "p".into(),
            error_handler: None,
            nodes: vec![
                node("listener", "listener"),
                supernode_instance("r", "renamed"),
                node("client", "client"),
            ],
            edges: vec![
                edge("listener.out", "r.in"),
                edge("r.success", "client.in"),
                // r.oops deliberately unwired — error-kind ports are optional
            ],
        };
        let out = expand_policy(&p, &[def]).unwrap();
        assert!(!out.edges.iter().any(|e| e.from == "r/auth.error"));
        assert!(out
            .edges
            .iter()
            .any(|e| e.from == "r/up.success" && e.to == "client.in"));
    }
}
