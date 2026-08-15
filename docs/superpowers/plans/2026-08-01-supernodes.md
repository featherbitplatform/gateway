# Supernodes Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Reusable named subgraphs ("supernodes") with one input, one success output, and one error output, stored as first-class config entities and inlined into policies at compile time.

**Architecture:** A new `supernodes:` section on `GatewayConfig`; a pure `expand_policy()` function inlines `type: supernode` instance nodes into flat policies (ids namespaced `instance/inner`) before `compile_policy` — the engine, metrics, and trace code are untouched. Admin API + etcd + web UI treat supernodes exactly like policies (CRUD, key family, library editor).

**Tech Stack:** Rust (serde, axum, tokio), React 19 + @xyflow/react (ui/), Playwright (e2e/), Docusaurus (website/).

**Spec:** `docs/superpowers/specs/2026-08-01-supernodes-design.md` — read it first.

## Global Constraints

- Conventional Commits, **no Co-Authored-By trailer** (project CLAUDE.md).
- Work on branch `feature/supernodes` (already created off `develop`).
- No new Rust or npm dependencies.
- Namespace separator is `/` (safe because `parse_edge_endpoint` splits on the **last dot**). Node ids must never contain `/`.
- Reserved boundary node types AND ids inside a supernode: `input`, `output`, `error`. Reserved node types in policies too (rejected there).
- Supernodes may not contain `listener`, `client`, or `supernode` nodes (no nesting in V1).
- Expansion output is never persisted; stored config always keeps the compact `type: supernode` reference form.
- Every Rust step: run tests with `cargo test <name> -- --exact` (or a substring filter) from the repo root; full `cargo test` must stay green at every commit.
- UI verification: `cd ui && npm run build` must pass (tsc + vite). There is no UI unit-test infra; the e2e suite covers behavior.

---

### Task 1: Config model — `SupernodeConfig`

**Files:**
- Modify: `src/config/gateway.rs` (structs end ~line 127)
- Modify: `src/config/mod.rs:16` (re-export)

**Interfaces:**
- Produces: `crate::config::SupernodeConfig { name: String, description: Option<String>, nodes: Vec<NodeConfig>, edges: Vec<EdgeConfig> }`; `GatewayConfig.supernodes: Vec<SupernodeConfig>`. All later tasks import `SupernodeConfig` from `crate::config`.

- [ ] **Step 1: Write the failing test** — append to the bottom of `src/config/gateway.rs`:

```rust
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
        })
        .unwrap();
        let back: GatewayConfig = serde_yaml::from_str(&out).unwrap();
        assert_eq!(back.supernodes[0].name, "secured-call");
        assert_eq!(back.supernodes[0].nodes.len(), 4);
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test test_supernodes_default_empty_and_roundtrip -- --exact`
Expected: COMPILE ERROR — `GatewayConfig` has no field `supernodes`, no struct `SupernodeConfig`.

- [ ] **Step 3: Implement** — in `src/config/gateway.rs` add to `GatewayConfig` (after the `consumers` field, ~line 37):

```rust
    /// Reusable named subgraphs, referenced from policies by nodes of
    /// `type: supernode`; inlined at compile time (see src/graph/expand.rs).
    #[serde(default)]
    pub supernodes: Vec<SupernodeConfig>,
```

and after `EdgeConfig` (end of file, before the new tests module):

```rust
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
```

In `src/config/mod.rs:16` extend the re-export:

```rust
pub use gateway::{
    EdgeConfig, GatewayConfig, MatchRule, NodeConfig, PolicyConfig, RouteConfig, SupernodeConfig,
};
```

Fix the one `GatewayConfig` struct literal that now fails to compile: `src/config_store/etcd.rs:273-277` (`gateway_from_kvs`) — add `supernodes: Vec::new(),` to the literal. (Full etcd support comes in Task 7; this just keeps the build green.)

- [ ] **Step 4: Run tests**

Run: `cargo test test_supernodes_default_empty_and_roundtrip -- --exact` → PASS, then `cargo test` → all green.

- [ ] **Step 5: Commit**

```bash
git add src/config/gateway.rs src/config/mod.rs src/config_store/etcd.rs
git commit -m "feat(config): add SupernodeConfig and GatewayConfig.supernodes"
```

---

### Task 2: Expansion — `src/graph/expand.rs`

**Files:**
- Create: `src/graph/expand.rs`
- Modify: `src/graph/mod.rs`

**Interfaces:**
- Consumes: `crate::config::{EdgeConfig, NodeConfig, PolicyConfig, SupernodeConfig}` (Task 1).
- Produces: `crate::graph::expand_policy(policy: &PolicyConfig, supernodes: &[SupernodeConfig]) -> Result<PolicyConfig, String>`; `pub(crate) const BOUNDARY_TYPES: [&str; 3]`; `pub(crate) fn split_endpoint(&str) -> (&str, &str)` (last-dot split, mirrors `engine::parse_edge_endpoint`). Tasks 3, 4, 5 use these.

- [ ] **Step 1: Create `src/graph/expand.rs` with the tests first** (module skeleton + tests, no implementation yet):

```rust
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
pub(crate) const BOUNDARY_TYPES: [&str; 3] = ["input", "output", "error"];

/// Splits `node_id.port` on the **last** dot, defaulting the port to `out`.
/// Mirror of `engine::parse_edge_endpoint` — the two must agree.
pub(crate) fn split_endpoint(s: &str) -> (&str, &str) {
    match s.rfind('.') {
        Some(i) => (&s[..i], &s[i + 1..]),
        None => (s, "out"),
    }
}

// expand_policy goes here (Step 3)

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
        n.config
            .insert("name".into(), serde_json::json!(name));
        n
    }

    fn edge(from: &str, to: &str) -> EdgeConfig {
        EdgeConfig { from: from.into(), to: to.into() }
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
            !out.nodes.iter().any(|n| BOUNDARY_TYPES.contains(&n.node_type.as_str())),
            "boundary pseudo-nodes must not leak into the expanded policy"
        );

        assert_eq!(
            edge_set(&out),
            vec![
                "eh.success->client.in",
                "listener.out->sec/auth.in",          // outer in-edge -> entry
                "sec/auth.error->eh.in",              // error boundary -> outer error target
                "sec/auth.success->sec/up.in",        // inner edge, prefixed
                "sec/up.error->eh.in",                // implicit black-box error edge
                "sec/up.success->client.in",          // output boundary -> outer success target
            ]
        );
    }

    /// Unwired outer ports: success exit edges are dropped (end-of-chain),
    /// error boundary edges are dropped (policy catch-all), and NO implicit
    /// error edges are added.
    #[test]
    fn test_unwired_outer_ports_drop_exit_edges() {
        let mut p = policy_using("sec");
        p.edges
            .retain(|e| !e.from.starts_with("sec.")); // keep only listener->sec.in, eh edge
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
            edges: vec![edge("listener.out", "sec.in"), edge("sec.success", "client.in")],
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
            edges: vec![edge("listener.out", "sec.in"), edge("sec.success", "client.in")],
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
}
```

- [ ] **Step 2: Wire the module and run tests to verify they fail** — in `src/graph/mod.rs` add `mod expand;` after `mod engine;` and `pub use expand::expand_policy;` after the engine re-export.

Run: `cargo test expand -- --nocapture`
Expected: COMPILE ERROR — `expand_policy` not found.

- [ ] **Step 3: Implement `expand_policy`** in `src/graph/expand.rs` (replace the `// expand_policy goes here` marker):

```rust
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
        /// Prefixed entry endpoint, e.g. `sec/auth.in`.
        entry: String,
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
        // validate_supernode guarantees exactly one edge leaves `input`.
        let entry_edge = def
            .edges
            .iter()
            .find(|e| split_endpoint(&e.from).0 == "input")
            .ok_or_else(|| format!("supernode '{}' has no edge from input.out", def.name))?;
        let (entry_node, _) = split_endpoint(&entry_edge.to);

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
                entry: format!("{}/{}.in", inst.id, entry_node),
                success_to,
                error_to,
            },
        );
    }

    // Non-instance nodes survive as-is; instance nodes are replaced below.
    let mut nodes: Vec<NodeConfig> = policy
        .nodes
        .iter()
        .filter(|n| n.node_type != "supernode")
        .cloned()
        .collect();

    // Outer edges: those leaving an instance are replaced by inner exit
    // edges; those entering one are redirected to its entry node.
    let mut edges: Vec<EdgeConfig> = Vec::new();
    for e in &policy.edges {
        let (from_node, _) = split_endpoint(&e.from);
        if splices.contains_key(from_node) {
            continue;
        }
        let (to_node, _) = split_endpoint(&e.to);
        match splices.get(to_node) {
            Some(s) => edges.push(EdgeConfig { from: e.from.clone(), to: s.entry.clone() }),
            None => edges.push(e.clone()),
        }
    }

    for inst in &instances {
        let s = &splices[inst.id.as_str()];
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
            if from_node == "input" {
                continue; // spliced via the outer in-edge above
            }
            let from = format!("{}.{}", prefix(from_node), from_port);
            match to_node {
                "output" => {
                    if let Some(t) = &s.success_to {
                        edges.push(EdgeConfig { from, to: t.clone() });
                    }
                }
                "error" => {
                    if let Some(t) = &s.error_to {
                        edges.push(EdgeConfig { from, to: t.clone() });
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
                    to: t.clone(),
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
```

Update the `src/graph/mod.rs` doc comment's first paragraph to mention expansion (one sentence: "Policies referencing supernodes are inlined by `expand_policy` before compilation.").

- [ ] **Step 4: Run tests**

Run: `cargo test expand` → all 7 new tests PASS; `cargo test` → green.

- [ ] **Step 5: Commit**

```bash
git add src/graph/expand.rs src/graph/mod.rs
git commit -m "feat(graph): compile-time expansion of supernode instances"
```

---

### Task 3: Validation — `validate_supernode` + policy-side rules

**Files:**
- Modify: `src/graph/validation.rs`
- Modify: `src/graph/mod.rs` (re-export)

**Interfaces:**
- Consumes: `SupernodeConfig` (Task 1), `expand::{BOUNDARY_TYPES, split_endpoint}` (Task 2).
- Produces: `crate::graph::validate_supernode(&SupernodeConfig) -> Result<(), Vec<String>>`. Task 4 calls it in `compile_routes`.

- [ ] **Step 1: Write the failing tests** — append inside the existing `mod tests` of `src/graph/validation.rs`:

```rust
    use crate::config::SupernodeConfig;

    fn boundary_nodes() -> Vec<NodeConfig> {
        ["input", "output", "error"]
            .into_iter()
            .map(|t| NodeConfig {
                id: t.to_string(),
                node_type: t.to_string(),
                config: HashMap::new(),
                position: None,
            })
            .collect()
    }

    fn inner(id: &str, ty: &str) -> NodeConfig {
        NodeConfig {
            id: id.to_string(),
            node_type: ty.to_string(),
            config: HashMap::new(),
            position: None,
        }
    }

    fn sn_edge(from: &str, to: &str) -> EdgeConfig {
        EdgeConfig { from: from.to_string(), to: to.to_string() }
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
    fn test_valid_supernode_passes() {
        assert!(validate_supernode(&valid_supernode()).is_ok());
    }

    #[test]
    fn test_supernode_missing_boundary_nodes() {
        let mut sn = valid_supernode();
        sn.nodes.retain(|n| n.node_type != "error");
        let errors = validate_supernode(&sn).unwrap_err();
        assert!(errors.iter().any(|e| e.contains("'error' boundary node")), "{errors:?}");
    }

    #[test]
    fn test_supernode_boundary_id_must_match_type() {
        let mut sn = valid_supernode();
        sn.nodes.iter_mut().find(|n| n.node_type == "input").unwrap().id = "start".into();
        let errors = validate_supernode(&sn).unwrap_err();
        assert!(errors.iter().any(|e| e.contains("must have id 'input'")), "{errors:?}");
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
        assert!(errors.iter().any(|e| e.contains("must not contain '/'")), "{errors:?}");
    }

    #[test]
    fn test_supernode_input_needs_exactly_one_outgoing_edge() {
        let mut sn = valid_supernode();
        sn.nodes.push(inner("cors", "cors"));
        sn.edges.push(sn_edge("input.out", "cors.in"));
        sn.edges.push(sn_edge("cors.success", "output.in"));
        let errors = validate_supernode(&sn).unwrap_err();
        assert!(errors.iter().any(|e| e.contains("exactly one edge")), "{errors:?}");

        let mut sn = valid_supernode();
        sn.edges.retain(|e| !e.from.starts_with("input."));
        let errors = validate_supernode(&sn).unwrap_err();
        assert!(errors.iter().any(|e| e.contains("exactly one edge")), "{errors:?}");
    }

    #[test]
    fn test_supernode_boundary_direction_rules() {
        let mut sn = valid_supernode();
        sn.edges.push(sn_edge("output.out", "up.in")); // out of output: invalid
        sn.edges.push(sn_edge("up.success", "input.in")); // into input: invalid
        let errors = validate_supernode(&sn).unwrap_err();
        assert!(errors.iter().any(|e| e.contains("cannot have outgoing")), "{errors:?}");
        assert!(errors.iter().any(|e| e.contains("cannot have incoming")), "{errors:?}");
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
        assert!(validate_supernode(&sn).is_ok(), "{:?}", validate_supernode(&sn));
    }

    #[test]
    fn test_supernode_dangling_edge_and_orphan() {
        let mut sn = valid_supernode();
        sn.edges.push(sn_edge("ghost.success", "output.in"));
        sn.nodes.push(inner("lonely", "cors"));
        let errors = validate_supernode(&sn).unwrap_err();
        assert!(errors.iter().any(|e| e.contains("unknown source node: 'ghost'")), "{errors:?}");
        assert!(errors.iter().any(|e| e.contains("Orphan node 'lonely'")), "{errors:?}");
    }

    #[test]
    fn test_policy_rejects_slash_ids_and_reserved_types() {
        let mut policy = PolicyConfig {
            name: "test".to_string(),
            error_handler: None,
            nodes: vec![listener_node(), upstream_node(), client_node()],
            edges: vec![
                EdgeConfig { from: "listener.out".to_string(), to: "backend.in".to_string() },
                EdgeConfig { from: "backend.success".to_string(), to: "client.in".to_string() },
            ],
        };
        policy.nodes[1].id = "a/b".to_string();
        policy.edges[0].to = "a/b.in".to_string();
        policy.edges[1].from = "a/b.success".to_string();
        let errors = validate_policy(&policy).unwrap_err();
        assert!(errors.iter().any(|e| e.contains("must not contain '/'")), "{errors:?}");

        let mut policy2 = PolicyConfig {
            name: "test2".to_string(),
            error_handler: None,
            nodes: vec![listener_node(), inner("x", "input"), client_node()],
            edges: vec![
                EdgeConfig { from: "listener.out".to_string(), to: "x.in".to_string() },
                EdgeConfig { from: "x.success".to_string(), to: "client.in".to_string() },
            ],
        };
        policy2.nodes[1].node_type = "input".to_string();
        let errors = validate_policy(&policy2).unwrap_err();
        assert!(errors.iter().any(|e| e.contains("reserved for supernode")), "{errors:?}");
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test validation` — Expected: COMPILE ERROR (`validate_supernode` not found).

- [ ] **Step 3: Implement.** In `src/graph/validation.rs`:

(a) Add imports: `use crate::config::SupernodeConfig;` and `use crate::graph::expand::{split_endpoint, BOUNDARY_TYPES};` — adjust to `use super::expand::...` if the compiler prefers.

(b) In `validate_policy`, after the client check (~line 37), add:

```rust
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
```

(c) Append `validate_supernode` after `validate_policy` (before `mod tests`):

```rust
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
        .filter(|n| n.node_type == "error-handler" || n.node_type == "output" || n.node_type == "error")
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
```

(d) In `src/graph/mod.rs` change the validation re-export to `pub use validation::{validate_policy, validate_supernode};`.

Note: the UI serializes source handles as `success`, so an edge from the input boundary may arrive as `input.success` rather than `input.out` — the rules above only match on the *node* side of the endpoint, which covers both. Do not tighten them to the literal port `out`.

- [ ] **Step 4: Run tests**

Run: `cargo test validation` → PASS (old + new); `cargo test` → green.

- [ ] **Step 5: Commit**

```bash
git add src/graph/validation.rs src/graph/mod.rs
git commit -m "feat(graph): validate supernode definitions and reserved ids"
```

---

### Task 4: Wire expansion into `SharedState::compile_routes`

**Files:**
- Modify: `src/state.rs` (`compile_routes`, lines 154-179, plus the `use crate::graph::...` import near the top)

**Interfaces:**
- Consumes: `expand_policy`, `validate_supernode` (Tasks 2-3).
- Produces: gateway-level behavior — any config path (file load, Admin API, etcd, hot reload) now validates supernodes, rejects duplicates, and compiles expanded policies. Tasks 5-7 rely on this being the single choke point.

- [ ] **Step 1: Write the failing tests.** `src/state.rs` has a `#[cfg(test)]` module (find it; if absent, create one at the bottom). Add:

```rust
    fn state_from_yaml(gateway_yaml: &str) -> Result<(), String> {
        let system: crate::config::SystemConfig = serde_yaml::from_str("{}").unwrap();
        let gw: crate::config::GatewayConfig = serde_yaml::from_str(gateway_yaml).unwrap();
        let state = SharedState::new(
            system,
            serde_yaml::from_str("{}").unwrap(),
            None,
            std::sync::Arc::new(crate::config_store::FileConfigStore::new(
                std::path::PathBuf::from("gateway.yaml"),
            )),
        )
        .unwrap();
        state.validate_gateway(&gw)
    }

    const SUPERNODE_GATEWAY: &str = r#"
supernodes:
  - name: secured-call
    nodes:
      - { id: input,  type: input }
      - { id: output, type: output }
      - { id: error,  type: error }
      - { id: up, type: upstream, config: { url: "http://127.0.0.1:9" } }
    edges:
      - { from: input.out,  to: up.in }
      - { from: up.success, to: output.in }
routes:
  - name: r
    match: { path: "/*" }
    policy: p
policies:
  - name: p
    nodes:
      - { id: listener, type: listener }
      - { id: sec, type: supernode, config: { name: secured-call } }
      - { id: client, type: client }
    edges:
      - { from: listener.out, to: sec.in }
      - { from: sec.success, to: client.in }
"#;

    #[test]
    fn test_policy_with_supernode_compiles() {
        assert_eq!(state_from_yaml(SUPERNODE_GATEWAY), Ok(()));
    }

    #[test]
    fn test_unknown_supernode_reference_rejected() {
        let yaml = SUPERNODE_GATEWAY.replace("name: secured-call } }", "name: nope } }");
        let err = state_from_yaml(&yaml).unwrap_err();
        assert!(err.contains("unknown supernode"), "{err}");
    }

    #[test]
    fn test_invalid_supernode_definition_rejected() {
        // Remove the input boundary node -> validate_supernode must fail.
        let yaml = SUPERNODE_GATEWAY.replace("- { id: input,  type: input }\n", "");
        let err = state_from_yaml(&yaml).unwrap_err();
        assert!(err.contains("Invalid supernode"), "{err}");
    }

    #[test]
    fn test_duplicate_supernode_names_rejected() {
        let dup = SUPERNODE_GATEWAY.replace(
            "supernodes:\n",
            "supernodes:\n  - name: secured-call\n    nodes: []\n    edges: []\n",
        );
        let err = state_from_yaml(&dup).unwrap_err();
        assert!(err.contains("Duplicate supernode"), "{err}");
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test test_policy_with_supernode_compiles -- --exact`
Expected: FAIL — error mentions `Unknown plugin type: supernode` (or similar from `create_plugin`), because nothing expands yet.

- [ ] **Step 3: Implement** — in `src/state.rs`, extend the graph import to `use crate::graph::{compile_policy, expand_policy, validate_policy, validate_supernode, CompiledGraph};` (match the existing import style), and rework `compile_routes`:

```rust
    fn compile_routes(
        gateway: &GatewayConfig,
        resources: &Arc<PluginResources>,
    ) -> Result<Vec<(RouteConfig, Arc<CompiledGraph>)>, String> {
        // Supernode definitions are validated first: policies expand against
        // them, so a broken definition must fail before any policy does.
        let mut seen = std::collections::HashSet::new();
        for sn in &gateway.supernodes {
            if !seen.insert(sn.name.as_str()) {
                return Err(format!("Duplicate supernode name '{}'", sn.name));
            }
            if let Err(errors) = validate_supernode(sn) {
                return Err(format!("Invalid supernode '{}': {:?}", sn.name, errors));
            }
        }

        let mut policy_map = std::collections::HashMap::new();
        for policy in &gateway.policies {
            if let Err(errors) = validate_policy(policy) {
                return Err(format!("Invalid policy '{}': {:?}", policy.name, errors));
            }
            // Inline supernode instances; the engine never sees them.
            let expanded = expand_policy(policy, &gateway.supernodes)?;
            let compiled = compile_policy(&expanded, resources.clone())?;
            policy_map.insert(policy.name.clone(), Arc::new(compiled));
        }
        // ... rest unchanged (route binding loop)
```

- [ ] **Step 4: Run tests**

Run: `cargo test state` and `cargo test` → all green. Note: `test_duplicate_supernode_names_rejected` proves delete-protection too — a candidate config whose policy references a missing supernode fails `validate_gateway`, which is exactly what a DELETE-while-referenced commit produces.

- [ ] **Step 5: Commit**

```bash
git add src/state.rs
git commit -m "feat(state): expand supernodes in compile_routes with validation"
```

---

### Task 5: Sandbox support

**Files:**
- Modify: `src/admin/debug.rs` (the `run_sandbox` handler, around lines 221-254)

**Interfaces:**
- Consumes: `expand_policy` (Task 2).
- Produces: sandbox runs (`POST /api/debug/sandbox`) support policies and ad-hoc node lists that reference supernodes.

- [ ] **Step 1: Write the failing test** — add to the existing `mod tests` in `src/admin/debug.rs` (handlers are unit-called directly there, e.g. `run_sandbox(State(s), Json(req))` at line ~408; the `state_with` helper at line ~320 builds an empty-gateway state — this test needs a populated gateway, so it builds its own state the same way):

```rust
    /// A sandbox run of a policy that uses a supernode must expand it the
    /// same way the data plane does — namespaced inner steps in the trace.
    #[tokio::test]
    async fn test_sandbox_expands_supernodes() {
        let system = SystemConfig {
            debug: DebugConfig {
                enabled: true,
                ..Default::default()
            },
            ..serde_yaml::from_str::<SystemConfig>("{}").unwrap()
        };
        let gateway: GatewayConfig = serde_yaml::from_str(
            r#"
supernodes:
  - name: secured-call
    nodes:
      - { id: input,  type: input }
      - { id: output, type: output }
      - { id: error,  type: error }
      - { id: c, type: cors }
    edges:
      - { from: input.out, to: c.in }
      - { from: c.success, to: output.in }
policies:
  - name: p
    nodes:
      - { id: listener, type: listener }
      - { id: sec, type: supernode, config: { name: secured-call } }
      - { id: client, type: client }
    edges:
      - { from: listener.out, to: sec.in }
      - { from: sec.success, to: client.in }
"#,
        )
        .unwrap();
        let s = Arc::new(
            SharedState::new(
                system,
                gateway,
                None,
                Arc::new(FileConfigStore::new(std::path::PathBuf::from(
                    "gateway.yaml",
                ))),
            )
            .unwrap(),
        );

        let req = SandboxRequest {
            policy: Some("p".to_string()),
            ..Default::default()
        };
        let resp = run_sandbox(State(s), Json(req)).await.into_response();
        assert_eq!(resp.status(), StatusCode::OK);

        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let steps = v["trace"]["steps"].as_array().unwrap();
        assert!(
            steps
                .iter()
                .any(|s| s["node_id"].as_str().unwrap_or("").starts_with("sec/")),
            "expected a namespaced sec/* step, got: {v}"
        );
    }
```

Caveat: this constructs `SharedState::new` with a gateway that references a supernode, so it only compiles/passes once Task 4 is merged (task order matters). If `SharedState::new` rejects the config before the sandbox is even reached, the failure in Step 2 will come from the `unwrap()` — that is still the correct "fails before implementation" signal.

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test test_sandbox_expands_supernodes -- --exact`
Expected: FAIL — 400 response, "Unknown plugin type: supernode".

- [ ] **Step 3: Implement** — in `run_sandbox`, right after the `validate_policy` check (line ~248-250) and before `compile_policy` (line ~251), insert:

```rust
    // Inline supernode references the same way compile_routes does — the
    // sandbox must never diverge from what the data plane executes.
    let supernodes = { state.gateway.read().await.supernodes.clone() };
    let policy = match crate::graph::expand_policy(&policy, &supernodes) {
        Ok(p) => p,
        Err(e) => return bad_request(e),
    };
```

- [ ] **Step 4: Run tests**

Run: `cargo test test_sandbox_expands_supernodes -- --exact` → PASS; `cargo test debug` → green.

- [ ] **Step 5: Commit**

```bash
git add src/admin/debug.rs
git commit -m "feat(debug): expand supernodes in sandbox runs"
```

---

### Task 6: Admin API — `/api/supernodes` CRUD

**Files:**
- Create: `src/admin/supernodes.rs`
- Modify: `src/admin/mod.rs` (module decl + `build_router` line ~138-142)

**Interfaces:**
- Consumes: `SupernodeConfig` (Task 1); commit-path validation (Task 4).
- Produces: `GET /api/supernodes`, `GET/PUT/DELETE /api/supernodes/{name}` (PUT = upsert). The UI client (Task 8) calls these.

- [ ] **Step 1: Create `src/admin/supernodes.rs`** — a full mirror of `src/admin/policies.rs`'s CRUD handlers (same doc-comment style, same commit pattern), with tests:

```rust
//! Admin API endpoints for supernode CRUD. Mutations rewrite the in-memory
//! gateway config and trigger validation + recompilation of every policy
//! (supernodes are inlined at compile time), so a breaking edit or a
//! delete-while-referenced is rejected with 400 before anything changes.

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router};

use crate::config::SupernodeConfig;
use crate::state::SharedState;

/// Builds the router for `/api/supernodes`.
pub fn router() -> Router<Arc<SharedState>> {
    Router::new()
        .route("/api/supernodes", get(list_supernodes))
        .route(
            "/api/supernodes/{name}",
            get(get_supernode).put(update_supernode).delete(delete_supernode),
        )
}

/// `GET /api/supernodes` — returns all supernode definitions as a JSON array.
async fn list_supernodes(State(state): State<Arc<SharedState>>) -> impl IntoResponse {
    let gw = state.gateway.read().await;
    Json(&gw.supernodes).into_response()
}

/// `GET /api/supernodes/{name}` — returns the named definition as JSON.
///
/// Errors: `404 Not Found` if no supernode with that name exists.
async fn get_supernode(
    State(state): State<Arc<SharedState>>,
    Path(name): Path<String>,
) -> impl IntoResponse {
    let gw = state.gateway.read().await;
    match gw.supernodes.iter().find(|s| s.name == name) {
        Some(sn) => Json(sn).into_response(),
        None => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "not_found"})),
        )
            .into_response(),
    }
}

/// `PUT /api/supernodes/{name}` — upserts a definition from the JSON body
/// (the path name overrides any name in the body), then revalidates and
/// recompiles all route graphs — every policy using this supernode picks up
/// the change atomically. Returns `{"status": "updated"}` on success.
///
/// Errors: `400 Bad Request` if the definition is invalid or any consuming
/// policy stops compiling (the previous compiled routes stay active).
async fn update_supernode(
    State(state): State<Arc<SharedState>>,
    Path(name): Path<String>,
    Json(mut sn): Json<SupernodeConfig>,
) -> impl IntoResponse {
    sn.name = name.clone();
    let candidate = {
        let gw = state.gateway.read().await;
        let mut candidate = gw.clone();
        if let Some(existing) = candidate.supernodes.iter_mut().find(|s| s.name == name) {
            *existing = sn;
        } else {
            candidate.supernodes.push(sn);
        }
        candidate
    };

    match state.config_store.clone().commit(&state, candidate).await {
        Ok(_) => Json(serde_json::json!({"status": "updated"})).into_response(),
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": e})),
        )
            .into_response(),
    }
}

/// `DELETE /api/supernodes/{name}` — removes the named definition, then
/// revalidates and recompiles. Returns `{"status": "deleted"}` on success.
///
/// Errors: `404 Not Found` if it does not exist; `400 Bad Request` if a
/// policy still references it (recompilation fails, nothing changes).
async fn delete_supernode(
    State(state): State<Arc<SharedState>>,
    Path(name): Path<String>,
) -> impl IntoResponse {
    let candidate = {
        let gw = state.gateway.read().await;
        let mut candidate = gw.clone();
        let before = candidate.supernodes.len();
        candidate.supernodes.retain(|s| s.name != name);
        if candidate.supernodes.len() == before {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({"error": "not_found"})),
            )
                .into_response();
        }
        candidate
    };

    match state.config_store.clone().commit(&state, candidate).await {
        Ok(_) => Json(serde_json::json!({"status": "deleted"})).into_response(),
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": e})),
        )
            .into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{GatewayConfig, SystemConfig};
    use crate::config_store::FileConfigStore;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    fn test_state(gateway_yaml: &str) -> Arc<SharedState> {
        let system: SystemConfig = serde_yaml::from_str("{}").unwrap();
        let gateway: GatewayConfig = serde_yaml::from_str(gateway_yaml).unwrap();
        Arc::new(
            SharedState::new(
                system,
                gateway,
                None,
                Arc::new(FileConfigStore::new(std::path::PathBuf::from("gateway.yaml"))),
            )
            .unwrap(),
        )
    }

    fn app(state: Arc<SharedState>) -> Router {
        router().with_state(state)
    }

    const VALID_SN: &str = r#"{
        "name": "secured-call",
        "nodes": [
            { "id": "input",  "type": "input",  "config": {} },
            { "id": "output", "type": "output", "config": {} },
            { "id": "error",  "type": "error",  "config": {} },
            { "id": "up", "type": "upstream", "config": { "url": "http://127.0.0.1:9" } }
        ],
        "edges": [
            { "from": "input.out",  "to": "up.in" },
            { "from": "up.success", "to": "output.in" }
        ]
    }"#;

    async fn put_supernode(state: &Arc<SharedState>, name: &str, body: &str) -> StatusCode {
        app(state.clone())
            .oneshot(
                Request::put(format!("/api/supernodes/{name}"))
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap()
            .status()
    }

    #[tokio::test]
    async fn test_put_list_get_delete_roundtrip() {
        let state = test_state("{}");
        assert_eq!(put_supernode(&state, "secured-call", VALID_SN).await, StatusCode::OK);

        let resp = app(state.clone())
            .oneshot(Request::get("/api/supernodes/secured-call").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let resp = app(state.clone())
            .oneshot(
                Request::delete("/api/supernodes/secured-call").body(Body::empty()).unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let resp = app(state)
            .oneshot(Request::get("/api/supernodes/secured-call").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn test_put_invalid_definition_is_400() {
        let state = test_state("{}");
        // Missing boundary nodes entirely.
        let bad = r#"{ "name": "x", "nodes": [], "edges": [] }"#;
        assert_eq!(put_supernode(&state, "x", bad).await, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn test_delete_while_referenced_is_400() {
        let state = test_state("{}");
        assert_eq!(put_supernode(&state, "secured-call", VALID_SN).await, StatusCode::OK);

        // Reference it from a policy by committing a candidate directly.
        let candidate = {
            let gw = state.gateway.read().await;
            let mut c = gw.clone();
            c.policies.push(serde_yaml::from_str(r#"
name: p
nodes:
  - { id: listener, type: listener }
  - { id: sec, type: supernode, config: { name: secured-call } }
  - { id: client, type: client }
edges:
  - { from: listener.out, to: sec.in }
  - { from: sec.success, to: client.in }
"#).unwrap());
            c
        };
        state.config_store.clone().commit(&state, candidate).await.unwrap();

        let resp = app(state.clone())
            .oneshot(
                Request::delete("/api/supernodes/secured-call").body(Body::empty()).unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn test_delete_missing_is_404() {
        let state = test_state("{}");
        let resp = app(state)
            .oneshot(Request::delete("/api/supernodes/nope").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test supernodes` — Expected: COMPILE ERROR (module not declared).

- [ ] **Step 3: Mount the router** — in `src/admin/mod.rs`: add `mod supernodes;` next to `mod policies;`, and in `build_router` add `.merge(supernodes::router())` after `.merge(policies::router())` (inside the authed block, before the auth layer).

- [ ] **Step 4: Run tests**

Run: `cargo test supernodes` → PASS; `cargo test` → green (including the three catalog drift-guard tests — `supernode` is deliberately NOT in the `/api/plugins` catalog).

- [ ] **Step 5: Commit**

```bash
git add src/admin/supernodes.rs src/admin/mod.rs
git commit -m "feat(admin): supernode CRUD endpoints"
```

---

### Task 7: etcd store — `supernodes/` key family

**Files:**
- Modify: `src/config_store/etcd.rs` (key fns ~193-201, `write_all` 204-218, `commit` 242-256, `gateway_from_kvs` 272-307, `is_empty` 324-326)

**Interfaces:**
- Consumes: `SupernodeConfig`, `GatewayConfig.supernodes` (Task 1).
- Produces: `<prefix>/supernodes/<name>` keys; seed/commit/load parity with routes/policies/consumers.

- [ ] **Step 1: Write the failing tests** — `src/config_store/etcd.rs` has (or gets) a `#[cfg(test)]` module; the pure functions are testable without a server. Add:

```rust
    #[test]
    fn test_gateway_from_kvs_parses_supernodes() {
        let sn = serde_json::json!({
            "name": "secured-call",
            "nodes": [ { "id": "input", "type": "input", "config": {} } ],
            "edges": []
        });
        let kvs = vec![(
            "gw/supernodes/secured-call".to_string(),
            serde_json::to_vec(&sn).unwrap(),
        )];
        let gw = gateway_from_kvs("gw", kvs).unwrap();
        assert_eq!(gw.supernodes.len(), 1);
        assert_eq!(gw.supernodes[0].name, "secured-call");
    }

    #[test]
    fn test_gateway_from_kvs_bad_supernode_json_is_error() {
        let kvs = vec![("gw/supernodes/x".to_string(), b"not json".to_vec())];
        let err = gateway_from_kvs("gw", kvs).unwrap_err();
        assert!(err.contains("bad supernode"), "{err}");
    }

    #[test]
    fn test_is_empty_counts_supernodes() {
        let mut gw: GatewayConfig = serde_yaml::from_str("{}").unwrap();
        assert!(is_empty(&gw));
        gw.supernodes.push(SupernodeConfig {
            name: "s".into(),
            description: None,
            nodes: vec![],
            edges: vec![],
        });
        assert!(!is_empty(&gw));
    }
```

(Import `SupernodeConfig` in the test module's `use` lines; follow the existing test imports in this file.)

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test etcd` — Expected: FAIL — supernode key parsed into the `_ => {}` arm (first test asserts len 1, gets 0); `is_empty` ignores supernodes.

- [ ] **Step 3: Implement** — four mechanical additions, each mirroring the `consumers` handling exactly:

1. Key fn (after `consumer_key`, line ~201): `fn supernode_key(&self, name: &str) -> String { format!("{}/supernodes/{}", self.prefix, name) }`
2. `write_all`: a fourth loop over `&gw.supernodes` using `supernode_key`.
3. `commit`: a fourth put-loop over `&candidate.supernodes` inserting into `desired`.
4. `gateway_from_kvs`: a `"supernodes"` match arm deserializing `SupernodeConfig` with error text `bad supernode '{key}': {e}` (import `SupernodeConfig` at the top with the other config imports); the struct literal already has `supernodes: Vec::new()` from Task 1.
5. `is_empty`: `&& gw.supernodes.is_empty()`.
6. Module doc comment (lines 1-19): mention the fourth key family, and note the pre-existing caveat that an older build sharing the prefix garbage-collects unknown kinds on its next commit.

- [ ] **Step 4: Run tests**

Run: `cargo test etcd` → PASS; `cargo test` → green.

- [ ] **Step 5: Commit**

```bash
git add src/config_store/etcd.rs
git commit -m "feat(etcd): persist supernodes under <prefix>/supernodes/"
```

---

### Task 8: UI — types, API client, visual identity

**Files:**
- Modify: `ui/src/types/index.ts`, `ui/src/api/client.ts`, `ui/src/pluginMeta.tsx`, `ui/src/components/PluginNode.tsx`

**Interfaces:**
- Produces: `Supernode` TS type; `api.listSupernodes/getSupernode/updateSupernode/deleteSupernode`; pluginMeta entries for `supernode`/`input`/`output`/`error`; PluginNode handle layout for boundary + instance nodes. Tasks 9-10 consume all of these.

- [ ] **Step 1: Types** — in `ui/src/types/index.ts` after the `Policy`-related interfaces (~line 87):

```ts
/**
 * A reusable named subgraph with a fixed input/output/error boundary,
 * usable in any policy as a single node of type `supernode`.
 *
 * @remarks
 * Served and persisted by the CRUD handlers in src/admin/supernodes.rs;
 * inlined into policies at compile time by src/graph/expand.rs.
 */
export interface Supernode {
  /** Unique supernode name, referenced from policy nodes' `config.name`. */
  name: string;
  /** Optional human-readable description shown in the library. */
  description?: string;
  /** Inner plugin nodes plus the `input`/`output`/`error` boundary nodes. */
  nodes: PolicyNode[];
  /** Directed connections; boundary edges use `input.out` / `output.in` / `error.in`. */
  edges: PolicyEdge[];
}
```

- [ ] **Step 2: API client** — in `ui/src/api/client.ts`, import `Supernode` in the type import list, and add after the Policies block (~line 118):

```ts
  // Supernodes
  /** `GET /api/supernodes` — returns all supernode definitions. */
  listSupernodes: () => request<Supernode[]>('/api/supernodes'),
  /** `GET /api/supernodes/{name}` — returns the named definition. */
  getSupernode: (name: string) => request<Supernode>(`/api/supernodes/${name}`),
  /** `PUT /api/supernodes/{name}` — upserts the named definition (also used to create). */
  updateSupernode: (name: string, sn: Supernode) =>
    request(`/api/supernodes/${name}`, { method: 'PUT', body: JSON.stringify(sn) }),
  /** `DELETE /api/supernodes/{name}` — removes the definition; 400 while referenced. */
  deleteSupernode: (name: string) =>
    request(`/api/supernodes/${name}`, { method: 'DELETE' }),
```

- [ ] **Step 3: pluginMeta** — in `ui/src/pluginMeta.tsx`, add four entries to the `pluginMeta` map (before the closing `};` at ~line 179), reusing already-imported icons (`Boxes`, `LogIn`, `LogOut`, `TriangleAlert`):

```ts
  // Supernodes and their boundary pseudo-nodes (src/graph/expand.rs)
  supernode:            { color: '#8b5cf6', icon: Boxes },
  input:                { color: '#64748b', icon: LogIn },
  output:               { color: '#64748b', icon: LogOut },
  error:                { color: '#b91c1c', icon: TriangleAlert },
```

- [ ] **Step 4: PluginNode handles** — in `ui/src/components/PluginNode.tsx` replace the two booleans at lines 64-65 with role predicates covering the boundary types:

```ts
  // Entry-like nodes have no input handle; terminal-like nodes have no
  // outputs. `input`/`output`/`error` are supernode boundary pseudo-nodes
  // (see src/graph/expand.rs) and mirror listener/client on the canvas.
  const isEntry = nodeData.pluginType === 'listener' || nodeData.pluginType === 'input';
  const isTerminal =
    nodeData.pluginType === 'client' ||
    nodeData.pluginType === 'output' ||
    nodeData.pluginType === 'error';
```

and update the three handle conditions: `{!isEntry && (` for the `in` handle, `{!isTerminal && (` for `success` (with `top: isEntry ? '50%' : '36%'`), `{!isEntry && !isTerminal && (` for `error`.

- [ ] **Step 5: Verify + commit**

Run: `cd ui && npm run build` → success. Also `cargo test test_every_catalog_plugin_has_an_icon -- --exact` → still PASS.

```bash
git add ui/src/types/index.ts ui/src/api/client.ts ui/src/pluginMeta.tsx ui/src/components/PluginNode.tsx
git commit -m "feat(ui): supernode types, API client, node visuals"
```

---

### Task 9: UI — GraphCanvas supernode mode + palette section

**Files:**
- Modify: `ui/src/components/GraphCanvas.tsx`, `ui/src/components/PluginDrawer.tsx`, `ui/src/components/NodeInspector.tsx`

**Interfaces:**
- Consumes: `Supernode` type + pluginMeta/PluginNode changes (Task 8).
- Produces: `GraphCanvas` props gain `kind: 'policy' | 'supernode'`, `supernodes: Supernode[]`; `PluginDrawer` props gain `supernodes: Supernode[]`, `onAddSupernode: (sn: Supernode) => void`. Task 10 (App) drives these.

- [ ] **Step 1: GraphCanvas props & mode.** Add `Supernode` to the type imports of both `GraphCanvas.tsx` and `PluginDrawer.tsx` (`import type { ..., Supernode } from '../types';`). In `GraphCanvasProps` add:

```ts
  /** Whether the canvas is editing a policy or a supernode definition. */
  kind: 'policy' | 'supernode';
  /** Supernode definitions offered in the policy palette (empty in supernode mode). */
  supernodes: Supernode[];
```

`policy: Policy | null` stays the shared graph shape: App converts a `Supernode` to `{ name, nodes, edges }` (a `Policy` without `error_handler`) before passing it in, so GraphCanvas round-trips both kinds through the existing `policyToNodes`/`nodesToPolicy` helpers unchanged. In supernode mode the auto-layout entry point is the `input` node — change the `policyToNodes` line 111 lookup to:

```ts
  const entryNode = policy.nodes.find((n) => n.type === 'listener' || n.type === 'input');
  if (entryNode) layout(entryNode.id, 1);
```

- [ ] **Step 2: Instance labels.** Supernode instance nodes should show which definition they reference. In `policyToNodes`'s node mapping, compute the label:

```ts
    data: {
      label:
        node.type === 'supernode' && typeof node.config?.name === 'string'
          ? `⬡ ${node.config.name}`
          : node.id,
      ...
```

- [ ] **Step 3: onConnect fan-in exception.** In `onConnect` (line ~288-292), extend the exception set so boundary exits accept fan-in, mirroring `validate_supernode`:

```ts
        const isClient = targetType === 'client';
        const isErrorHandler = targetType === 'error-handler';
        const isBoundaryExit = targetType === 'output' || targetType === 'error';
        if (!isClient && !isErrorHandler && !isBoundaryExit) {
```

- [ ] **Step 4: Palette.** Pass through to `PluginDrawer`: `<PluginDrawer ... supernodes={kind === 'policy' ? supernodes : []} onAddSupernode={handleAddSupernode} />`. Add the handler next to `handleAddScript`:

```ts
  const handleAddSupernode = (sn: Supernode) => {
    const id = `${sn.name}-${Date.now().toString(36)}`;
    const newNode: Node = {
      id,
      type: 'pluginNode',
      position: { x: 300, y: 200 + nodes.length * 80 },
      data: {
        label: `⬡ ${sn.name}`,
        pluginType: 'supernode',
        config: { name: sn.name },
        onSelect: handleSelect,
      } satisfies PluginNodeData,
    };
    setNodes((nds) => [...nds, newNode]);
    setSelectedNodeId(id);
    setDrawerOpen(false);
  };
```

In `PluginDrawer.tsx`: add the two props (typed, doc-commented like the others), and render a "Supernodes" section above the plugin categories, modeled 1:1 on the existing scripts section (hidden when `supernodes` is empty; each row shows name + description, uses `getPluginMeta('supernode')` for the tile, fires `onAddSupernode(sn)`). Include supernodes in the search filter the same way scripts are matched.

- [ ] **Step 5: Inspector.** In `NodeInspector.tsx` line 137, extend the fixed set:

```ts
  const isFixed = ['listener', 'client', 'input', 'output', 'error'].includes(data.pluginType);
```

and for supernode instances render a read-only reference instead of the config form: when `data.pluginType === 'supernode'`, show the referenced name (`String(data.config?.name ?? '')`) with a hint line "Reusable subgraph — edit its definition from the Supernodes section in the sidebar." (plain div styled like the existing field labels; no jump-link plumbing in V1 — the sidebar section is one click away).

- [ ] **Step 6: Verify + commit**

Run: `cd ui && npm run build` → success (this catches every missed required-prop callsite; fix any the compiler reports — App is updated next task, so if the build fails ONLY in App.tsx, that is expected: add temporary `kind="policy" supernodes={[]}` there or proceed straight to Task 10 and build once).

```bash
git add ui/src/components/GraphCanvas.tsx ui/src/components/PluginDrawer.tsx ui/src/components/NodeInspector.tsx
git commit -m "feat(ui): supernode editing mode and palette section"
```

---

### Task 10: UI — App + Sidebar library section

**Files:**
- Modify: `ui/src/App.tsx`, `ui/src/components/Sidebar.tsx`

**Interfaces:**
- Consumes: everything from Tasks 8-9.
- Produces: a "Supernodes" sidebar section (list/create/delete/select); selecting one opens it in the canvas in supernode mode.

- [ ] **Step 1: App state.** In `App.tsx`:
- Add state: `const [supernodes, setSupernodes] = useState<Supernode[]>([]);` and `const [selectedSupernode, setSelectedSupernode] = useState<string | null>(null);` (import `Supernode`).
- In `loadData`, add `api.listSupernodes()` to the `Promise.all` and `setSupernodes(sn)`.
- Selection is mutually exclusive: `onSelectRoute` also clears `selectedSupernode`, and vice versa (wrap the setters in small handlers).
- Derive the canvas input:

```ts
  const selectedSupernodeDef = supernodes.find((s) => s.name === selectedSupernode) || null;
  // A supernode is edited through the same canvas contract as a policy.
  const canvasPolicy: Policy | null = selectedSupernodeDef
    ? { name: selectedSupernodeDef.name, nodes: selectedSupernodeDef.nodes, edges: selectedSupernodeDef.edges }
    : selectedPolicy;
```

- Render: `<GraphCanvas key={canvasPolicy?.name ?? ''} kind={selectedSupernodeDef ? 'supernode' : 'policy'} policy={canvasPolicy} supernodes={supernodes} ... onSavePolicy={handleSaveGraph} />` where:

```ts
  const handleSaveGraph = async (graph: Policy) => {
    if (selectedSupernodeDef) {
      try {
        await api.updateSupernode(graph.name, {
          name: graph.name,
          description: selectedSupernodeDef.description,
          nodes: graph.nodes,
          edges: graph.edges,
        });
        await loadData();
        setToast({ tone: 'success', title: 'Supernode saved', message: graph.name });
      } catch (e) {
        setToast({ tone: 'error', title: 'Failed to save supernode', message: `${e}` });
      }
      return;
    }
    await handleSavePolicy(graph);
  };
```

- Create-supernode dialog (mirror the create-route dialog): one name field; on submit seed a minimal valid definition and select it:

```ts
      await api.updateSupernode(name, {
        name,
        nodes: [
          { id: 'input', type: 'input', config: {}, position: { x: 0, y: 150 } },
          { id: 'output', type: 'output', config: {}, position: { x: 500, y: 150 } },
          { id: 'error', type: 'error', config: {}, position: { x: 500, y: 330 } },
        ],
        edges: [{ from: 'input.out', to: 'output.in' }],
      });
```

- Delete-supernode confirmation (mirror delete-route); surface the 400 error text in the toast — that is the delete-while-referenced message.

- [ ] **Step 2: Sidebar.** Add props: `supernodes: Supernode[]`, `selectedSupernode: string | null`, `onSelectSupernode: (name: string) => void`, `onCreateSupernode: () => void`, `onDeleteSupernode: (name: string) => void`. Render a second list section under Routes, visually identical (eyebrow "Supernodes", + New button, rows with name + description-or-node-count, hover delete X). Reuse the exact row markup/styles of the routes list.

- [ ] **Step 3: Verify + commit**

Run: `cd ui && npm run build` → success. Then a manual smoke test — `cargo run` (or `docker compose up`) with `ui_enabled`, open the UI: create a supernode, add a node inside it, save; create a policy node from the palette; save the policy; check `GET /api/config/export` includes `supernodes:`.

```bash
git add ui/src/App.tsx ui/src/components/Sidebar.tsx
git commit -m "feat(ui): supernode library section in sidebar"
```

---

### Task 11: UI — TraceViewer namespaced ids

**Files:**
- Modify: `ui/src/components/TraceViewer.tsx` (step list rows ~line 149-232)

**Interfaces:**
- Consumes: nothing new — trace steps already carry namespaced `node_id`s like `sec/up` after expansion.
- Produces: readable rendering of namespaced steps.

- [ ] **Step 1: Implement.** Where the step list renders `{s.node_id}` (line ~189) and the detail header renders `{step.node_id}` (line ~221), split on the FIRST `/` and render the instance prefix muted:

```tsx
  /** Renders `sec/up` as a muted `sec /` prefix + the inner id, so supernode
   *  instances group visually; plain ids render unchanged. */
  const NodeId = ({ id }: { id: string }) => {
    const slash = id.indexOf('/');
    if (slash === -1) return <>{id}</>;
    return (
      <>
        <span style={{ color: 'var(--text-muted)' }}>{id.slice(0, slash + 1)}</span>
        {id.slice(slash + 1)}
      </>
    );
  };
```

Use `<NodeId id={s.node_id} />` in both places (and for `next_node_id` at line ~232).

- [ ] **Step 2: Verify + commit**

Run: `cd ui && npm run build` → success.

```bash
git add ui/src/components/TraceViewer.tsx
git commit -m "feat(ui): render namespaced supernode ids in trace viewer"
```

---

### Task 12: E2E scenario + testbook

**Files:**
- Create: `e2e/tests/supernodes.spec.ts`
- Modify: `e2e/E2E_TESTBOOK.md`

**Interfaces:**
- Consumes: helpers in `e2e/helpers/admin.ts` (`adminApi`, `dataPlane`, `deleteRouteIfPresent`) — read that file first and reuse its idioms; the seeded `echo-policy` (its `upstream` node config points at the suite's echo backend).

- [ ] **Step 1: Write the spec** — `e2e/tests/supernodes.spec.ts`:

```ts
/**
 * Supernode scenarios. See E2E_TESTBOOK.md ("Supernodes").
 */
import {test, expect} from '@playwright/test';

import {adminApi, dataPlane, deleteRouteIfPresent} from '../helpers/admin';

/** Builds a supernode wrapping the seeded echo upstream (fetched live so the
 *  test tracks the suite's isolated backend port). */
async function echoSupernode(api: Awaited<ReturnType<typeof adminApi>>) {
  const echoPolicy = (await (await api.get('/api/policies/echo-policy')).json()) as {
    nodes: {id: string; type: string; config: Record<string, unknown>}[];
  };
  const upstream = echoPolicy.nodes.find((n) => n.type === 'upstream');
  expect(upstream).toBeTruthy();
  return {
    name: 'e2e-secured-call',
    nodes: [
      {id: 'input', type: 'input', config: {}},
      {id: 'output', type: 'output', config: {}},
      {id: 'error', type: 'error', config: {}},
      {id: 'up', type: 'upstream', config: upstream!.config},
    ],
    edges: [
      {from: 'input.out', to: 'up.in'},
      {from: 'up.success', to: 'output.in'},
    ],
  };
}

test.describe('Supernodes', () => {
  test('E2E-SN-01: CRUD + policy use + data plane + delete protection', async () => {
    const api = await adminApi();
    await deleteRouteIfPresent(api, 'sn-route');
    await api.delete('/api/policies/sn-policy');
    await api.delete('/api/supernodes/e2e-secured-call');

    // Create the definition.
    const sn = await echoSupernode(api);
    expect((await api.put('/api/supernodes/e2e-secured-call', {data: sn})).ok()).toBeTruthy();

    // Use it from a policy + route.
    expect(
      (
        await api.put('/api/policies/sn-policy', {
          data: {
            name: 'sn-policy',
            nodes: [
              {id: 'listener', type: 'listener', config: {}},
              {id: 'sec', type: 'supernode', config: {name: 'e2e-secured-call'}},
              {id: 'client', type: 'client', config: {}},
            ],
            edges: [
              {from: 'listener.out', to: 'sec.in'},
              {from: 'sec.success', to: 'client.in'},
            ],
          },
        })
      ).ok(),
    ).toBeTruthy();
    expect(
      (
        await api.post('/api/routes', {
          data: {name: 'sn-route', match: {path: '/sn-e2e/*', methods: ['GET']}, policy: 'sn-policy'},
        })
      ).ok(),
    ).toBeTruthy();

    // The expanded pipeline serves traffic.
    const dp = await dataPlane();
    const res = await dp.get('/sn-e2e/hello');
    expect(res.status()).toBe(200);

    // Export keeps the compact form and includes the definition.
    const yaml = await (await api.get('/api/config/export')).text();
    expect(yaml).toContain('supernodes:');
    expect(yaml).toContain('e2e-secured-call');
    expect(yaml).toContain('type: supernode');
    expect(yaml).not.toContain('sec/up'); // expansion is never persisted

    // Deleting a referenced supernode must fail...
    expect((await api.delete('/api/supernodes/e2e-secured-call')).status()).toBe(400);

    // ...and succeed once the consumer is gone.
    await api.delete('/api/routes/sn-route');
    await api.delete('/api/policies/sn-policy');
    expect((await api.delete('/api/supernodes/e2e-secured-call')).ok()).toBeTruthy();

    await dp.dispose();
    await api.dispose();
  });
});
```

Before finalizing, open `e2e/helpers/admin.ts` and one existing spec (`admin-api.spec.ts`) — if `dataPlane()` has a different shape (e.g. returns a URL, not a context), adapt those two lines to match how `data-plane.spec.ts` issues requests. If the debug trigger header is exercised in `debug.spec.ts`, optionally extend the test: send the request with that header, then `GET /api/debug/traces` and assert some step `node_id` starts with `sec/`.

- [ ] **Step 2: Run**

```bash
cargo build --release
cd e2e && npm test -- supernodes.spec.ts
```
Expected: PASS.

- [ ] **Step 3: Testbook** — add a "Supernodes" section to `e2e/E2E_TESTBOOK.md` following the existing per-scenario format (scenario id E2E-SN-01, what it covers, expected outcome).

- [ ] **Step 4: Commit**

```bash
git add e2e/tests/supernodes.spec.ts e2e/E2E_TESTBOOK.md
git commit -m "test(e2e): supernode CRUD, expansion, and delete-protection scenario"
```

---

### Task 13: Docs

**Files:**
- Create: `website/docs/concepts/supernodes.md`
- Modify: `website/sidebars.ts` (or wherever `concepts/*` pages are registered — check how `concepts/policies-and-graphs.md` is listed), `website/docs/guides/admin-api.md`, `website/docs/reference/roadmap.md` (honest-state ledger), `CLAUDE.md` (project intro mentions feature set — add supernodes to the core features bullet list)

- [ ] **Step 1: Concept page** — `website/docs/concepts/supernodes.md` covering, with YAML examples copied from the spec (`docs/superpowers/specs/2026-08-01-supernodes-design.md` §1): what a supernode is; the three boundary nodes; using one from a policy; black-box error behavior; compile-time expansion and what it means for traces/metrics (`instance/inner` ids); V1 limits (no parameters, no nesting); export/seeding behavior. Follow the front-matter and tone of `website/docs/concepts/policies-and-graphs.md`.

- [ ] **Step 2: Admin API guide** — add the four `/api/supernodes` endpoints to `website/docs/guides/admin-api.md`, formatted like the policies section, including the 400 delete-while-referenced behavior.

- [ ] **Step 3: Roadmap + CLAUDE.md** — mark supernodes as shipped in `website/docs/reference/roadmap.md`; add "**Supernodes** — reusable named subgraphs inlined into policies at compile time" to the Core features list in `CLAUDE.md`.

- [ ] **Step 4: Verify + commit**

Run: `cd website && npm run build` → success (catches broken links/sidebar refs).

```bash
git add website/docs website/sidebars.ts CLAUDE.md
git commit -m "docs: supernodes concept page, admin API reference, roadmap"
```

---

### Task 14: Final verification

- [ ] `cargo test` — full suite green.
- [ ] `cargo build --release && cd e2e && npm test` — full e2e suite green (not just the new spec).
- [ ] `cd ui && npm run build` and `cd website && npm run build` — both green.
- [ ] Manual sweep per spec Verification section: `docker compose up`, build a supernode in the UI, wire it into a policy, hit the route; check `/metrics` shows `node_id="<instance>/<inner>"` labels; check the Debug panel trace shows namespaced steps; check `GET /api/config/export` contains `supernodes:`.
- [ ] Use superpowers:requesting-code-review, then superpowers:finishing-a-development-branch (PR target: `develop`).
