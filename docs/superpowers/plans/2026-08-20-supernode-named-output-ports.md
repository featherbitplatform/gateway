# Supernode Named Output Ports Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Supernode definitions declare arbitrary named output ports (one per `type: output` boundary node), instances expose them like plugin outcome ports, and the policy editor can extract a multi-node selection into a new supernode.

**Architecture:** The definition schema is unchanged — extra `type: output` boundary nodes in `nodes` carry the port names (node id = port name; id `output` keeps mapping to instance port `success`). `validate_supernode` allows 1+ output boundaries; `expand_policy` generalizes its success/error exit pair to a per-boundary exit map and enforces mandatory wiring of every output-derived instance port. The UI derives instance port rows from the referenced definition and gains an extraction flow built on a pure `extractSupernode` helper.

**Tech Stack:** Rust (serde, existing graph engine), React + TypeScript + ReactFlow (@xyflow/react), Vitest, Playwright.

**Spec:** `docs/superpowers/specs/2026-08-20-supernode-named-output-ports-design.md`

## Global Constraints

- Conventional Commits, **no Co-Authored-By trailer** (project CLAUDE.md overrides the harness default).
- All work on branch `feature/supernode-named-ports` off `develop`; PR targets `develop`. Do not push or open the PR until Francesco gives the go-ahead (delivery-workflow memory).
- Reserved output-boundary ids (exact list): `input`, `error`, `in`, `out`, `success`. The id `output` is legal and maps to instance port `success` (alias `out`).
- `input` and `error` boundaries stay singular with id == type. At least one `type: output` node is required.
- Every output-derived instance port is mandatory-wired in policies; `error` stays optional.
- Rust tests: `cargo test`. UI tests: `cd ui && npm test` (vitest). Lint: `cd ui && npm run lint`. UI type-check/build: `cd ui && npm run build`.
- Backward compatibility: every existing test that isn't explicitly updated below must keep passing unchanged.

---

### Task 1: Backend validation — multiple named output boundaries

**Files:**
- Modify: `src/graph/validation.rs` (fn `validate_supernode`, lines ~157-245, plus its doc comment and tests)

**Interfaces:**
- Consumes: `SupernodeConfig` (unchanged shape), `BOUNDARY_TYPES` and `split_endpoint` from `src/graph/expand.rs`.
- Produces: `validate_supernode` accepting 1+ `type: output` nodes with unique non-reserved ids; a new `pub(crate) const RESERVED_OUTPUT_IDS: [&str; 5] = ["input", "error", "in", "out", "success"];` exported from `validation.rs` for reuse in error messages/tests. Task 2 relies on validation guaranteeing: unique node ids, ≥1 output boundary, singular input/error with id == type, no reserved output ids.

- [ ] **Step 1: Write the failing tests** — add to the `tests` module in `src/graph/validation.rs`:

```rust
/// Named output ports: any number of `type: output` boundary nodes, each
/// id becoming an instance port name (spec §1-2).
#[test]
fn test_supernode_multiple_output_boundaries_accepted() {
    let mut nodes = boundary_nodes(); // input/output/error
    nodes.push(inner("denied", "output"));
    nodes.push(inner("auth", "key-auth"));
    let sn = SupernodeConfig {
        name: "gate".to_string(),
        description: None,
        nodes,
        edges: vec![
            sn_edge("input.out", "auth.in"),
            sn_edge("auth.success", "output.in"),
            sn_edge("auth.denied", "denied.in"),
        ],
    };
    assert_eq!(validate_supernode(&sn), Ok(()));
}

/// A definition may have only named outputs (no `output`-id node): the
/// instance then has no `success` port.
#[test]
fn test_supernode_only_named_outputs_accepted() {
    let nodes: Vec<NodeConfig> = vec![
        inner("input", "input"),
        inner("done", "output"),
        inner("error", "error"),
        inner("up", "upstream"),
    ];
    let sn = SupernodeConfig {
        name: "named-only".to_string(),
        description: None,
        nodes,
        edges: vec![
            sn_edge("input.out", "up.in"),
            sn_edge("up.success", "done.in"),
        ],
    };
    assert_eq!(validate_supernode(&sn), Ok(()));
}

/// Reserved ids collide with fixed instance port names.
#[test]
fn test_supernode_reserved_output_ids_rejected() {
    for id in ["input", "error", "in", "out", "success"] {
        let mut sn = valid_supernode();
        sn.nodes.push(inner(id, "output"));
        let errors = validate_supernode(&sn).unwrap_err();
        assert!(
            errors.iter().any(|e| e.contains("reserved")),
            "id {id}: {errors:?}"
        );
    }
}

/// Zero output boundaries is still an error.
#[test]
fn test_supernode_zero_output_boundaries_rejected() {
    let mut sn = valid_supernode();
    sn.nodes.retain(|n| n.node_type != "output");
    sn.edges.retain(|e| !e.to.starts_with("output."));
    // keep `up` connected so only the output complaint fires
    sn.edges.push(sn_edge("up.success", "error.in"));
    let errors = validate_supernode(&sn).unwrap_err();
    assert!(
        errors
            .iter()
            .any(|e| e.contains("at least one 'output' boundary")),
        "{errors:?}"
    );
}

/// Duplicate node ids inside a definition are rejected (two outputs with
/// the same id would otherwise be one ambiguous port).
#[test]
fn test_supernode_duplicate_node_ids_rejected() {
    let mut sn = valid_supernode();
    sn.nodes.push(inner("denied", "output"));
    sn.nodes.push(inner("denied", "output"));
    let errors = validate_supernode(&sn).unwrap_err();
    assert!(
        errors.iter().any(|e| e.contains("duplicate node id 'denied'")),
        "{errors:?}"
    );
}

/// Named output boundaries obey the no-outgoing-edges rule like `output`/`error`.
#[test]
fn test_supernode_named_output_boundary_no_outgoing_edges() {
    let mut sn = valid_supernode();
    sn.nodes.push(inner("denied", "output"));
    sn.edges.push(sn_edge("denied.out", "up.in"));
    let errors = validate_supernode(&sn).unwrap_err();
    assert!(
        errors
            .iter()
            .any(|e| e.contains("'denied'") && e.contains("cannot have outgoing")),
        "{errors:?}"
    );
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test test_supernode_multiple_output_boundaries_accepted test_supernode_only_named_outputs_accepted test_supernode_reserved_output_ids_rejected test_supernode_zero_output_boundaries_rejected test_supernode_duplicate_node_ids_rejected test_supernode_named_output_boundary_no_outgoing_edges`
Expected: FAIL — multiple/named outputs trip "declares more than one 'output' node" / "must have id 'output'"; the duplicate-id and reserved-id tests fail for lack of those checks.

- [ ] **Step 3: Rework `validate_supernode`** — replace the boundary loop (the `for ty in BOUNDARY_TYPES` block) and generalize the direction rule:

```rust
/// Output-boundary ids that would collide with an instance's fixed port
/// names (spec §1). `output` is the one special id: it maps to `success`.
pub(crate) const RESERVED_OUTPUT_IDS: [&str; 5] = ["input", "error", "in", "out", "success"];
```

Inside `validate_supernode`:

```rust
// Duplicate ids: two output boundaries sharing an id would be one
// ambiguous instance port; inner-node duplicates collapse in compile.
let mut seen_ids: HashSet<&str> = HashSet::new();
for n in &sn.nodes {
    if !seen_ids.insert(n.id.as_str()) {
        errors.push(format!(
            "Supernode '{}': duplicate node id '{}'",
            sn.name, n.id
        ));
    }
}

// input/error: exactly one each, id == type (unchanged rule).
for ty in ["input", "error"] {
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

// output: one or more; each id is an instance port name (`output` -> the
// `success` port), so reserved ids that collide with fixed port names are
// rejected.
let output_nodes: Vec<&crate::config::NodeConfig> =
    sn.nodes.iter().filter(|n| n.node_type == "output").collect();
if output_nodes.is_empty() {
    errors.push(format!(
        "Supernode '{}' must declare at least one 'output' boundary node",
        sn.name
    ));
}
for o in &output_nodes {
    if RESERVED_OUTPUT_IDS.contains(&o.id.as_str()) {
        errors.push(format!(
            "Supernode '{}': output boundary id '{}' is reserved — it would \
             collide with a fixed instance port name",
            sn.name, o.id
        ));
    }
}
```

Then generalize the edge-direction rule. Build a set of exit-boundary ids once, above the edge loop:

```rust
let exit_boundary_ids: HashSet<&str> = sn
    .nodes
    .iter()
    .filter(|n| n.node_type == "output" || n.node_type == "error")
    .map(|n| n.id.as_str())
    .collect();
```

and replace `if from_node == "output" || from_node == "error"` with `if exit_boundary_ids.contains(from_node)` (same error message, which already interpolates the node id).

Also update the inner-node reserved-id check: `BOUNDARY_TYPES.contains(&n.id.as_str())` stays as-is for non-boundary nodes (an inner node named `input`/`output`/`error` is still confusing) — no change needed there. Update the function's doc comment: replace "exactly one boundary node each of type `input`/`output`/`error`" with the new rules (1+ outputs, ids are port names, reserved list, duplicate ids rejected).

- [ ] **Step 4: Run the full validation test module**

Run: `cargo test --lib validation`
Expected: PASS, including all pre-existing tests (`test_supernode_missing_boundary_nodes` still passes because it removes `error`; `test_supernode_boundary_id_must_match_type` renames `input` which is still singular).

- [ ] **Step 5: Commit**

```bash
git add src/graph/validation.rs
git commit -m "feat(graph): allow multiple named output boundaries in supernode validation"
```

---

### Task 2: Expansion — named instance ports with mandatory wiring

**Files:**
- Modify: `src/graph/expand.rs` (fn `expand_policy`, the `Splice` struct, `resolve_target`, module doc comment, and tests)
- Modify: `src/config/gateway.rs:169-188` (SupernodeConfig doc comment only)

**Interfaces:**
- Consumes: validation guarantees from Task 1 (but expansion must stay total for un-validated input: unknown-name/missing-boundary paths keep returning `Err`, not panicking).
- Produces: `expand_policy` accepting outer edges on any port derived from an output boundary; erroring on unwired output ports with the message shape `policy '<p>': output port '<port>' of supernode instance '<id>' must be wired — add an edge from '<id>.<port>'`; erroring on unknown ports with a message that lists the exposed ports. The UI (Tasks 3-6) mirrors the id↔port mapping: boundary id `output` ⇄ port `success`, any other output boundary id ⇄ port of the same name.

- [ ] **Step 1: Write the failing tests** — add to the `tests` module in `src/graph/expand.rs`:

```rust
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
        edges: vec![
            edge("input.out", "up.in"),
            edge("up.success", "done.in"),
        ],
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
        edges: vec![
            edge("listener.out", "n.in"),
            edge("n.success", "client.in"),
        ],
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
        edges: vec![
            edge("listener.out", "n.in"),
            edge("n.done", "client.in"),
        ],
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
        edges: vec![
            edge("listener.out", "s.in"),
            edge("s.denied", "client.in"),
        ],
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
```

- [ ] **Step 2: Update the existing tests whose semantics the spec changes** (same file). Mandatory wiring makes some previously-tolerated shapes into errors, and mismatched output ids become real port names:

  1. `test_unwired_outer_ports_drop_exit_edges` — rename to `test_unwired_success_exit_is_rejected` and assert `expand_policy(...).unwrap_err()` contains `"output port 'success' of supernode instance 'sec' must be wired"`. Keep a second part asserting that an unwired **error** exit still just drops edges: start from `policy_using("sec")`, remove only the `sec.error` edge (and the now-orphaned `eh` node and its edge), expand, and assert no `sec/auth.error->` or `sec/up.error->` edges exist while `sec/up.success->client.in` does.
  2. `test_pass_through_identity_with_unwired_outer_success` — rename to `test_pass_through_identity_with_unwired_outer_success_is_rejected`; the identity definition's `success` port is now mandatory, so expanding with only `pass.error` wired must return an `Err` containing `"output port 'success' of supernode instance 'pass'"`.
  3. `test_boundary_by_type_with_mismatched_id` — the `out1` output boundary now means the instance exposes port `out1`, not `success`. Rewire the policy edge to `edge("sb.out1", "client.in")` and keep asserting `sb/process.success->client.in` results. Rename to `test_named_output_boundary_port_name_is_its_id`.
  4. `test_outer_custom_port_on_instance_is_rejected` — still valid (secured-call has no `denied` boundary), but update the asserted message to the new unknown-port wording: assert `err.contains("unknown port 'denied'")` and that the message lists `success` among the exposed ports.
  5. `test_error_boundary_pass_through_with_unwired_outer_error` wires only `err_pass.success` — the mandatory `success` port IS wired and `error` is optional, so it stays valid: no edit.
  5b. `test_error_boundary_pass_through_with_wired_outer_error` wires only `err_pass.error` — the definition has an `output`-id boundary, so its `success` port is now mandatory and unwired. Add `node("client", "client")` and `edge("err_pass.success", "client.in")` so the test keeps asserting the error-pass-through splice. Same fix in `test_pass_through_chain_into_another_instance`, whose `x` (error-pass-through) leaves `x.success` unwired: add `edge("x.success", "eh.in")`.
  6. `test_pass_through_identity_with_wired_outer_success` uses only `pass.success` wired and no `pass.error` — stays valid (error optional). No edit.
  7. `secured_call()`-based happy-path tests (`test_happy_path_inlines_and_splices`, `test_two_instances_of_same_supernode_get_distinct_namespaces`, alias/duplicate tests) wire `success` already — no edits beyond what compiles.
  8. `test_pass_through_cycle_is_error` and the multi-hop chain tests wire the ports they traverse; where a cycle test leaves `b.success`→`a.in` and `a.error` wired but `b`'s own error/success set incomplete, add the minimal extra outer edges so the *only* failure is the cycle (e.g. give `b` a `b.error`? — no: identity exposes `success` + `error`; in `test_pass_through_cycle_is_error` both `a.success` and `b.success` are wired and errors are optional, so no edit; in `test_multi_hop_pass_through_cycle_is_error` `x.success`/`y.success` are NOT wired — add `node("client", "client")` plus `edge("x.success", "client.in")` and `edge("y.success", "client.in")` so the cycle error is what fires).
  9. `test_multi_hop_pass_through_chain_resolves_to_fixed_point` — `x`/`y` are error-pass-throughs whose `success` ports are unwired; add `edge("x.success", "eh.in")` and `edge("y.success", "eh.in")` (the `secured_call` instance `z` already wires its `success`).

- [ ] **Step 3: Run tests to verify the new ones fail and the updated ones fail for the right reason**

Run: `cargo test --lib expand`
Expected: new tests FAIL (unknown port 'denied' rejected by the old success/error-only match); updated tests FAIL against old code where semantics changed.

- [ ] **Step 4: Rework `expand_policy`.** Replace the `Splice` struct and the per-instance outer-edge scan:

```rust
/// Boundary wiring resolved per instance.
struct Splice<'a> {
    def: &'a SupernodeConfig,
    /// Boundary id -> type map for the definition.
    boundary_map: HashMap<String, String>,
    /// The node id of the input boundary.
    input_node_id: String,
    /// Prefixed entry endpoint, e.g. `sec/auth.in`, or None for pass-through boundaries.
    entry: Option<String>,
    /// For pass-through instances: the boundary node id the entry edge targets.
    pass_through_boundary: Option<String>,
    /// Boundary node id -> `to` endpoint of the outer edge wired to its port.
    /// Mandatory-wiring guarantees an entry for every output boundary; the
    /// error boundary's entry is optional.
    exit_to: HashMap<String, String>,
    /// Node id of the error boundary (found by type, tolerating id mismatch).
    error_node_id: Option<String>,
}
```

Add a helper above `expand_policy` (next to `split_endpoint`):

```rust
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
```

Per-instance resolution (replacing the `success_to`/`error_to` scan):

```rust
let error_node_id = boundary_map
    .iter()
    .find(|(_, ty)| ty.as_str() == "error")
    .map(|(id, _)| id.clone());
let output_ids: Vec<String> = def
    .nodes
    .iter()
    .filter(|n| n.node_type == "output")
    .map(|n| n.id.clone())
    .collect();

let mut exit_to: HashMap<String, String> = HashMap::new();
for e in &policy.edges {
    let (from_node, from_port) = split_endpoint(&e.from);
    if from_node != inst.id {
        continue;
    }
    let port = if from_port == "out" { "success" } else { from_port };
    let boundary_id = if port == "error" {
        error_node_id.clone()
    } else {
        output_ids
            .iter()
            .find(|id| port_for_output_boundary(id) == port)
            .cloned()
    };
    let Some(bid) = boundary_id else {
        let mut exposed: Vec<&str> = output_ids
            .iter()
            .map(|id| port_for_output_boundary(id))
            .collect();
        exposed.push("error");
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
```

The pass-through detection changes from a type to the boundary id:

```rust
let (entry, pass_through_boundary) = if boundary_map.contains_key(entry_target) {
    (None, Some(entry_target.to_string()))
} else {
    (Some(format!("{}/{}.in", inst.id, entry_target)), None)
};
```

`resolve_target`'s pass-through step becomes:

```rust
let pass_boundary = match &s.pass_through_boundary {
    Some(b) => b,
    None => return Ok(Some(current)),
};
// ... cycle check unchanged ...
match s.exit_to.get(pass_boundary.as_str()) {
    Some(t) => current = t.clone(),
    None => return Ok(None), // only reachable for an unwired error boundary
}
```

Inner-edge splicing generalizes the `Some("output")`/`Some("error")` match:

```rust
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
    // Unwired error boundary: drop (policy catch-all takes over). Output
    // boundaries are always wired — checked above.
} else {
    edges.push(EdgeConfig {
        from,
        to: format!("{}.{}", prefix(to_node), to_port),
    });
}
```

The black-box error rule reads `s.error_node_id.as_ref().and_then(|id| s.exit_to.get(id.as_str()))` instead of `s.error_to`.

Update the module doc comment (lines 29-51) to describe the per-boundary exit map, mandatory wiring, and the `output`→`success` mapping.

- [ ] **Step 5: Run the whole backend suite**

Run: `cargo test`
Expected: PASS. If anything outside `expand`/`validation` fails (e.g. `src/state.rs` or debug sandbox fixtures instantiate supernodes), fix the fixture wiring — mandatory ports must now be wired in every fixture policy.

- [ ] **Step 6: Update `SupernodeConfig` doc comment** in `src/config/gateway.rs:169-175`: replace "a fixed boundary: exactly one `input`, one `output`, and one `error` pseudo-node" with "a boundary of exactly one `input` and one `error` pseudo-node plus one or more `output` pseudo-nodes; each output node's id is an instance port name (`output` = the `success` port)". Run `cargo test --lib config` to confirm nothing broke.

- [ ] **Step 7: Commit**

```bash
git add src/graph/expand.rs src/config/gateway.rs
git commit -m "feat(graph): expand supernode instances with named output ports and mandatory wiring"
```

---

### Task 3: UI — derive instance ports from the definition

**Files:**
- Modify: `ui/src/policyGraph.ts` (new `supernodePortSpec`, extend `policyToNodes`/`policyToEdges`/`portKindFor`)
- Modify: `ui/src/components/GraphCanvas.tsx` (`findUnwiredPorts`, `initialNodes`/`initialEdges`, supernode-refresh effect, `handleAddSupernode`, `onConnect`)
- Test: `ui/src/policyGraph.test.ts`

**Interfaces:**
- Consumes: `Supernode`, `PortSpec`, `PortDecl` from `ui/src/types`; `resolveOutputs` from `ui/src/nodeKinds`.
- Produces: `export function supernodePortSpec(def: Supernode | undefined): PortSpec | undefined` in `policyGraph.ts` — port order: `success` (only if an `output`-id boundary exists), named ports in definition `nodes` order (kind `outcome`), then `error`. `policyToNodes(policy, onSelect, portSpecs, showPortNames, supernodes?: Supernode[])` and `policyToEdges(policy, portSpecs, supernodes?: Supernode[])` gain a trailing optional param. `portKindFor(sourceType, port, portSpecs, portsOverride?: PortSpec)` gains an optional override. Tasks 4-6 rely on these exact signatures.

- [ ] **Step 1: Write the failing tests** — add to `ui/src/policyGraph.test.ts` (match the file's existing fixture style; the assertions below are the contract):

```ts
import { supernodePortSpec, policyToEdges, policyToNodes } from './policyGraph';
import type { Supernode } from './types';

const gateDef: Supernode = {
  name: 'auth-gate',
  nodes: [
    { id: 'input', type: 'input', config: {} },
    { id: 'output', type: 'output', config: {} },
    { id: 'denied', type: 'output', config: {} },
    { id: 'error', type: 'error', config: {} },
    { id: 'auth', type: 'key-auth', config: {} },
  ],
  edges: [
    { from: 'input.out', to: 'auth.in' },
    { from: 'auth.success', to: 'output.in' },
    { from: 'auth.denied', to: 'denied.in' },
  ],
};

describe('supernodePortSpec', () => {
  it('derives success + named outcome + error, in order', () => {
    const spec = supernodePortSpec(gateDef)!;
    expect(spec.outputs.map((p) => [p.name, p.kind])).toEqual([
      ['success', 'success'],
      ['denied', 'outcome'],
      ['error', 'error'],
    ]);
  });

  it('omits success when no output-id boundary exists', () => {
    const namedOnly: Supernode = {
      ...gateDef,
      nodes: gateDef.nodes.filter((n) => n.id !== 'output'),
    };
    const spec = supernodePortSpec(namedOnly)!;
    expect(spec.outputs.map((p) => p.name)).toEqual(['denied', 'error']);
  });

  it('returns undefined for a missing definition', () => {
    expect(supernodePortSpec(undefined)).toBeUndefined();
  });
});

describe('policyToNodes/policyToEdges with supernode instances', () => {
  const policy = {
    name: 'p',
    nodes: [
      { id: 'listener', type: 'listener', config: {} },
      { id: 'gate', type: 'supernode', config: { name: 'auth-gate' } },
      { id: 'reject', type: 'error-handler', config: {} },
      { id: 'client', type: 'client', config: {} },
    ],
    edges: [
      { from: 'listener.out', to: 'gate.in' },
      { from: 'gate.success', to: 'client.in' },
      { from: 'gate.denied', to: 'reject.in' },
    ],
  };

  it('threads the derived port spec into instance node data', () => {
    const nodes = policyToNodes(policy, () => {}, {}, true, [gateDef]);
    const gate = nodes.find((n) => n.id === 'gate')!;
    const ports = (gate.data as { ports?: { outputs: { name: string }[] } }).ports;
    expect(ports?.outputs.map((p) => p.name)).toEqual(['success', 'denied', 'error']);
  });

  it('styles a named instance port as an outcome edge', () => {
    const edges = policyToEdges(policy, {}, [gateDef]);
    const denied = edges.find((e) => e.sourceHandle === 'denied')!;
    expect(denied.style?.stroke).toBe('var(--accent)');
    expect(denied.animated).toBe(false);
  });

  it('falls back to the catalog/default pair when the definition is missing', () => {
    const nodes = policyToNodes(policy, () => {}, {}, true, []);
    const gate = nodes.find((n) => n.id === 'gate')!;
    expect((gate.data as { ports?: unknown }).ports).toBeUndefined();
  });
});
```

- [ ] **Step 2: Run to verify failure**

Run: `cd ui && npx vitest run src/policyGraph.test.ts`
Expected: FAIL — `supernodePortSpec` doesn't exist; extra args are type errors.

- [ ] **Step 3: Implement in `policyGraph.ts`:**

```ts
/**
 * Derives the port spec a supernode INSTANCE exposes from its definition's
 * output boundary nodes — the UI mirror of the id↔port mapping in
 * src/graph/expand.rs::port_for_output_boundary: the boundary with id
 * `output` is the `success` port; any other `type: output` boundary is a
 * named outcome port; `error` is always present (optional wiring).
 * Returns undefined when the definition is unresolved so callers fall back
 * to the default success+error pair, matching today's dangling-ref render.
 */
export function supernodePortSpec(def: Supernode | undefined): PortSpec | undefined {
  if (!def) return undefined;
  const outputs: PortDecl[] = [];
  const outputNodes = def.nodes.filter((n) => n.type === 'output');
  if (outputNodes.some((n) => n.id === 'output')) {
    outputs.push({
      name: 'success',
      kind: 'success',
      description: `Exit through the 'output' boundary of '${def.name}'.`,
    });
  }
  for (const n of outputNodes) {
    if (n.id === 'output') continue;
    outputs.push({
      name: n.id,
      kind: 'outcome',
      description: `Exit through the '${n.id}' output boundary of '${def.name}'.`,
    });
  }
  outputs.push({
    name: 'error',
    kind: 'error',
    description: `Error exit of '${def.name}' (optional wiring).`,
  });
  return { input: `Request enters '${def.name}'.`, outputs };
}
```

Extend `portKindFor` with `portsOverride?: PortSpec` as the 4th param: `resolveOutputs(sourceType, portsOverride ?? portSpecs[sourceType])`. Extend `policyToNodes` and `policyToEdges` with a trailing `supernodes: Supernode[] = []` param; in both, for a node/source of type `supernode`, resolve `const def = supernodes.find((s) => s.name === node.config?.name)` and use `supernodePortSpec(def) ?? portSpecs[node.type]` for `ports` (nodes) / the `portKindFor` override (edges). Import `Supernode`, `PortSpec` types.

- [ ] **Step 4: Thread through `GraphCanvas.tsx`:**
  - `initialNodes`: `policyToNodes(policy, handleSelect, portSpecs, showPortNames, supernodes)`; `initialEdges`: `policyToEdges(policy, portSpecs, supernodes)` (add `supernodes` to both `useMemo` dep arrays).
  - The supernode-refresh `useEffect` (line ~300): alongside `supernodeDef`, also refresh ports: `ports: supernodePortSpec(refName ? supernodes.find((s) => s.name === refName) : undefined) ?? portSpecs['supernode'],`.
  - `handleAddSupernode`: `ports: supernodePortSpec(sn),` (replacing `portSpecs['supernode']`) and update the stale comment.
  - `onConnect`: pass the source node's own ports as override: `portKindFor(sourceType, connection.sourceHandle || 'success', portSpecs, (sourceNode?.data as unknown as PluginNodeData)?.ports)`.
  - `findUnwiredPorts(policy, portSpecs, supernodes)`: add the param; inside the loop, for `node.type === 'supernode'` use `supernodePortSpec(supernodes.find((s) => s.name === node.config?.name)) ?? portSpecs[node.type]` as the spec passed to `resolveOutputs`. Update the `handleSave` call site and dep array.

- [ ] **Step 5: Run UI tests, lint, and build**

Run: `cd ui && npm test && npm run lint && npm run build`
Expected: PASS (build catches signature drift in SupernodePreview, which calls the two converters without the new optional param — that stays valid).

- [ ] **Step 6: Commit**

```bash
git add ui/src/policyGraph.ts ui/src/policyGraph.test.ts ui/src/components/GraphCanvas.tsx
git commit -m "feat(ui): derive supernode instance ports from the definition's output boundaries"
```

---

### Task 4: UI — add/rename output-port boundaries in the supernode editor

**Files:**
- Modify: `ui/src/components/PluginDrawer.tsx` (new optional `onAddOutputPort` prop + section)
- Modify: `ui/src/components/GraphCanvas.tsx` (output-port name dialog, add + rename handlers)
- Modify: `ui/src/components/NodeInspector.tsx` (editable Node ID for output boundaries in supernode mode)
- Test: `ui/src/portNameValidation.test.ts` (new) — create the validator in `ui/src/portNameValidation.ts`

**Interfaces:**
- Consumes: `supernodePortSpec` contract from Task 3 (renames/additions flow into instance ports automatically once saved); `Dialog`/`DialogButton`/`DialogField` from `ui/src/components/Dialog.tsx` (same usage as App.tsx's create-supernode dialog).
- Produces: `export function validatePortName(name: string, takenIds: string[], selfId?: string): string | null` in `ui/src/portNameValidation.ts` (null = valid, string = error message); `NodeInspector` prop `onRenameNode?: (nodeId: string) => void` (opens GraphCanvas's rename dialog — validation lives in the dialog, one path for add and rename); `PluginDrawer` prop `onAddOutputPort?: () => void`. Task 6 does not depend on these.

- [ ] **Step 1: Write the failing validator tests** — `ui/src/portNameValidation.test.ts`:

```ts
import { describe, expect, it } from 'vitest';
import { validatePortName } from './portNameValidation';

describe('validatePortName', () => {
  it('accepts a fresh kebab name', () => {
    expect(validatePortName('denied', ['input', 'output', 'error'])).toBeNull();
  });
  it('rejects reserved ids', () => {
    for (const r of ['input', 'error', 'in', 'out', 'success']) {
      expect(validatePortName(r, [])).toMatch(/reserved/);
    }
  });
  it('rejects empty, slash, and duplicate ids', () => {
    expect(validatePortName('', [])).toMatch(/required/i);
    expect(validatePortName('a/b', [])).toMatch(/'\/'/);
    expect(validatePortName('denied', ['denied'])).toMatch(/already exists/);
  });
  it('allows keeping your own id on rename', () => {
    expect(validatePortName('denied', ['denied', 'output'], 'denied')).toBeNull();
  });
});
```

- [ ] **Step 2: Run to verify failure**

Run: `cd ui && npx vitest run src/portNameValidation.test.ts`
Expected: FAIL — module not found.

- [ ] **Step 3: Implement `ui/src/portNameValidation.ts`:**

```ts
/**
 * Validates an output-boundary id (= the instance port name it exposes).
 * Mirror of the reserved list in src/graph/validation.rs::RESERVED_OUTPUT_IDS.
 */
const RESERVED = ['input', 'error', 'in', 'out', 'success'];

export function validatePortName(
  name: string,
  takenIds: string[],
  selfId?: string
): string | null {
  if (!name.trim()) return 'A port name is required';
  if (RESERVED.includes(name)) return `'${name}' is a reserved port name`;
  if (name.includes('/')) return "Port names must not contain '/'";
  if (takenIds.some((id) => id === name && id !== selfId))
    return `A node named '${name}' already exists`;
  return null;
}
```

Run the test: PASS.

- [ ] **Step 4: PluginDrawer section.** Add `onAddOutputPort?: () => void` to `PluginDrawerProps`. Render, above the "Supernodes" section, only when the prop is set (GraphCanvas passes it only in supernode mode) and either not searching or `'output port'.includes(q)`:

```tsx
{onAddOutputPort && (!searching || 'output port'.includes(q)) && (
  <>
    <div className="eyebrow px-1 pb-2">Boundary</div>
    <NodeRow
      onClick={onAddOutputPort}
      color={getPluginMeta('output').color}
      icon={(() => { const I = getPluginMeta('output').icon; return <I size={15} strokeWidth={1.75} />; })()}
      title="Output port"
      subtitle="Named exit — becomes a port on every instance"
    />
  </>
)}
```

(Adjust `nothingMatches` so this section counts as a match.)

- [ ] **Step 5: GraphCanvas add + rename.** Add state `const [portDialog, setPortDialog] = useState<{ mode: 'add' } | { mode: 'rename'; nodeId: string } | null>(null);` plus `portName`/`portError` string state. `handleAddOutputPort` opens `{ mode: 'add' }`. On submit:

```ts
const submitPortDialog = () => {
  if (!portDialog) return;
  const name = portName.trim();
  const err = validatePortName(
    name,
    nodes.map((n) => n.id),
    portDialog.mode === 'rename' ? portDialog.nodeId : undefined
  );
  if (err) { setPortError(err); return; }
  if (portDialog.mode === 'add') {
    setNodes((nds) => [...nds, {
      id: name,
      type: 'pluginNode',
      position: { x: 300, y: 200 + nds.length * 80 },
      data: {
        label: name, pluginType: 'output', config: {},
        ports: undefined, onSelect: handleSelect, showPortNames,
      } satisfies PluginNodeData,
    }]);
    setSelectedNodeId(name);
  } else {
    const oldId = portDialog.nodeId;
    setNodes((nds) => nds.map((n) =>
      n.id === oldId ? { ...n, id: name, data: { ...n.data, label: name } } : n
    ));
    setEdges((eds) => eds.map((e) => ({
      ...e,
      source: e.source === oldId ? name : e.source,
      target: e.target === oldId ? name : e.target,
    })));
    setSelectedNodeId(name);
  }
  setPortDialog(null);
};
```

Render a `Dialog` (import from `./Dialog`, same pattern as App.tsx lines 707-727) titled "Output port name" with one `DialogField` (value `portName`, onChange clears `portError`), an inline error line when `portError` is set, and Create/Rename + Cancel buttons. Pass `onAddOutputPort={kind === 'supernode' ? handleAddOutputPort : undefined}` to `PluginDrawer`, and `onRenameNode={kind === 'supernode' ? handleRenameOutputPort : undefined}` to `NodeInspector`, where `handleRenameOutputPort = (nodeId: string) => { setPortName(nodeId); setPortError(null); setPortDialog({ mode: 'rename', nodeId }); }` — the inspector only needs to *open* the dialog, so simplify the NodeInspector prop to `onRenameNode?: (nodeId: string) => void`.

- [ ] **Step 6: NodeInspector rename affordance.** Add `onRenameNode?: (nodeId: string) => void` to the props. In the Node ID block (line ~327), when `kind === 'supernode' && data.pluginType === 'output' && onRenameNode` render a small "Rename" button next to the read-only input that calls `onRenameNode(node.id)`; otherwise leave the block untouched. (Keeping the input read-only and routing renames through the dialog gives one validation path.)

- [ ] **Step 7: Verify manually-typed flows compile and tests pass**

Run: `cd ui && npm test && npm run lint && npm run build`
Expected: PASS.

- [ ] **Step 8: Commit**

```bash
git add ui/src/portNameValidation.ts ui/src/portNameValidation.test.ts ui/src/components/PluginDrawer.tsx ui/src/components/GraphCanvas.tsx ui/src/components/NodeInspector.tsx
git commit -m "feat(ui): add and rename named output-port boundaries in the supernode editor"
```

---

### Task 5: UI — pure extraction helper

**Files:**
- Create: `ui/src/extractSupernode.ts`
- Test: `ui/src/extractSupernode.test.ts`

**Interfaces:**
- Consumes: `Policy`, `PolicyNode`, `PolicyEdge`, `Supernode` from `ui/src/types`; `splitEdge` from `ui/src/policyGraph.ts`.
- Produces (Task 6 depends on these exact shapes):

```ts
export interface ExtractionResult {
  /** The new definition to create via the Admin API. */
  definition: Supernode;
  /** The policy with the selection replaced by one wired instance node. */
  policy: Policy;
  /** Id of the inserted instance node. */
  instanceId: string;
}
/** Throws Error(<user-facing message>) when the selection is ineligible. */
export function extractSupernode(
  policy: Policy,
  selectedIds: string[],
  name: string
): ExtractionResult;
```

- [ ] **Step 1: Write the failing tests** — `ui/src/extractSupernode.test.ts`:

```ts
import { describe, expect, it } from 'vitest';
import { extractSupernode } from './extractSupernode';
import type { Policy } from './types';

/** listener -> auth -> rl -> upstream -> client, with auth.denied and both
 *  error ports exiting to a shared handler. auth+rl get selected. */
function fixture(): Policy {
  return {
    name: 'p',
    nodes: [
      { id: 'listener', type: 'listener', config: {}, position: { x: 0, y: 100 } },
      { id: 'auth', type: 'key-auth', config: { header: 'apikey' }, position: { x: 250, y: 100 } },
      { id: 'rl', type: 'rate-limit', config: {}, position: { x: 500, y: 100 } },
      { id: 'up', type: 'upstream', config: {}, position: { x: 750, y: 100 } },
      { id: 'eh', type: 'error-handler', config: {}, position: { x: 500, y: 300 } },
      { id: 'client', type: 'client', config: {}, position: { x: 1000, y: 100 } },
    ],
    edges: [
      { from: 'listener.out', to: 'auth.in' },
      { from: 'auth.success', to: 'rl.in' },
      { from: 'auth.denied', to: 'eh.in' },
      { from: 'auth.error', to: 'eh.in' },
      { from: 'rl.limited', to: 'eh.in' },
      { from: 'rl.success', to: 'up.in' },
      { from: 'up.success', to: 'client.in' },
      { from: 'eh.success', to: 'client.in' },
    ],
  };
}

describe('extractSupernode', () => {
  it('builds a definition with input/entry, per-exit output boundaries, and error', () => {
    const { definition } = extractSupernode(fixture(), ['auth', 'rl'], 'guard');
    expect(definition.name).toBe('guard');
    const byId = Object.fromEntries(definition.nodes.map((n) => [n.id, n.type]));
    expect(byId['input']).toBe('input');
    expect(byId['error']).toBe('error');
    expect(byId['output']).toBe('output'); // rl.success exit -> success port
    expect(byId['denied']).toBe('output'); // auth.denied exit
    expect(byId['limited']).toBe('output'); // rl.limited exit
    expect(byId['auth']).toBe('key-auth');
    expect(byId['rl']).toBe('rate-limit');
    const edgeSet = definition.edges.map((e) => `${e.from}->${e.to}`).sort();
    expect(edgeSet).toEqual([
      'auth.denied->denied.in',
      'auth.error->error.in',
      'auth.success->rl.in',
      'input.out->auth.in',
      'rl.limited->limited.in',
      'rl.success->output.in',
    ]);
  });

  it('rewrites the policy around one wired instance node', () => {
    const { policy, instanceId } = extractSupernode(fixture(), ['auth', 'rl'], 'guard');
    const inst = policy.nodes.find((n) => n.id === instanceId)!;
    expect(inst.type).toBe('supernode');
    expect(inst.config).toEqual({ name: 'guard' });
    expect(policy.nodes.map((n) => n.id).sort()).toEqual(
      ['client', 'eh', instanceId, 'listener', 'up'].sort()
    );
    const edgeSet = policy.edges.map((e) => `${e.from}->${e.to}`).sort();
    expect(edgeSet).toEqual(
      [
        `listener.out->${instanceId}.in`,
        `${instanceId}.success->up.in`,
        `${instanceId}.denied->eh.in`,
        `${instanceId}.limited->eh.in`,
        `${instanceId}.error->eh.in`,
        'up.success->client.in',
        'eh.success->client.in',
      ].sort()
    );
  });

  it('preserves configs, config_ref, and positions into the definition', () => {
    const p = fixture();
    p.nodes[1].config_ref = 'shared-auth';
    const { definition } = extractSupernode(p, ['auth', 'rl'], 'guard');
    const auth = definition.nodes.find((n) => n.id === 'auth')!;
    expect(auth.config).toEqual({ header: 'apikey' });
    expect(auth.config_ref).toBe('shared-auth');
    expect(auth.position).toEqual({ x: 250, y: 100 });
  });

  it('dedupes clashing port names with numeric suffixes', () => {
    const p = fixture();
    // second node with its own `denied` exit
    p.nodes.push({ id: 'auth2', type: 'key-auth', config: {}, position: { x: 300, y: 200 } });
    p.edges = p.edges.filter((e) => e.from !== 'auth.success');
    p.edges.push({ from: 'auth.success', to: 'auth2.in' });
    p.edges.push({ from: 'auth2.success', to: 'rl.in' });
    p.edges.push({ from: 'auth2.denied', to: 'eh.in' });
    const { definition } = extractSupernode(p, ['auth', 'auth2', 'rl'], 'guard');
    const outputs = definition.nodes.filter((n) => n.type === 'output').map((n) => n.id).sort();
    expect(outputs).toEqual(['denied', 'denied-2', 'limited', 'output']);
  });

  it('rejects selections containing listener/client/supernode nodes', () => {
    expect(() => extractSupernode(fixture(), ['listener', 'auth'], 'x')).toThrow(/listener/);
  });

  it('rejects selections with no inbound edge or split entry', () => {
    // up+eh: inbound edges target both `up` (from rl) and `eh` (from auth/rl)
    expect(() => extractSupernode(fixture(), ['up', 'eh'], 'x')).toThrow(/single entry/i);
  });

  it('rejects conflicting outer error targets', () => {
    const p = fixture();
    p.nodes.push({ id: 'eh2', type: 'error-handler', config: {}, position: { x: 700, y: 300 } });
    // rl's error goes somewhere other than auth's error target
    p.edges.push({ from: 'rl.error', to: 'eh2.in' });
    p.edges.push({ from: 'eh2.success', to: 'client.in' });
    expect(() => extractSupernode(p, ['auth', 'rl'], 'x')).toThrow(/error exits/i);
  });

  it('rejects selections whose exits are all error edges', () => {
    const p: Policy = {
      name: 'p',
      nodes: [
        { id: 'listener', type: 'listener', config: {} },
        { id: 'a', type: 'request-validation', config: {} },
        { id: 'client', type: 'client', config: {} },
      ],
      // only an error edge leaves the selection
      edges: [
        { from: 'listener.out', to: 'a.in' },
        { from: 'a.error', to: 'client.in' },
      ],
    };
    expect(() => extractSupernode(p, ['a'], 'x')).toThrow(/non-error exit/i);
  });
});
```

- [ ] **Step 2: Run to verify failure**

Run: `cd ui && npx vitest run src/extractSupernode.test.ts`
Expected: FAIL — module not found.

- [ ] **Step 3: Implement `ui/src/extractSupernode.ts`.** Algorithm (spec §5):

```ts
import type { Policy, PolicyEdge, PolicyNode, Supernode } from './types';
import { splitEdge } from './policyGraph';

const FORBIDDEN_TYPES = ['listener', 'client', 'supernode'];
const RESERVED = ['input', 'output', 'error', 'in', 'out', 'success'];

export interface ExtractionResult {
  definition: Supernode;
  policy: Policy;
  instanceId: string;
}

export function extractSupernode(
  policy: Policy,
  selectedIds: string[],
  name: string
): ExtractionResult {
  const selected = new Set(selectedIds);
  const selectedNodes = policy.nodes.filter((n) => selected.has(n.id));

  for (const n of selectedNodes) {
    if (FORBIDDEN_TYPES.includes(n.type)) {
      throw new Error(
        `Cannot extract '${n.id}': ${n.type} nodes cannot live inside a supernode`
      );
    }
  }

  // Classify edges relative to the selection.
  const inner: PolicyEdge[] = [];
  const inbound: PolicyEdge[] = [];
  const outNonError: PolicyEdge[] = [];
  const outError: PolicyEdge[] = [];
  const outside: PolicyEdge[] = [];
  for (const e of policy.edges) {
    const [from, fromPort] = splitEdge(e.from);
    const [to] = splitEdge(e.to);
    const fromIn = selected.has(from);
    const toIn = selected.has(to);
    if (fromIn && toIn) inner.push(e);
    else if (!fromIn && toIn) inbound.push(e);
    else if (fromIn && !toIn) (fromPort === 'error' ? outError : outNonError).push(e);
    else outside.push(e);
  }

  // Single entry node.
  const entryTargets = new Set(inbound.map((e) => splitEdge(e.to)[0]));
  if (entryTargets.size !== 1) {
    throw new Error(
      entryTargets.size === 0
        ? 'The selection needs an incoming edge to become the single entry point'
        : `The selection must have a single entry node — edges enter at: ${[...entryTargets].join(', ')}`
    );
  }
  const entryId = [...entryTargets][0];

  if (outNonError.length === 0) {
    throw new Error('The selection needs at least one non-error exit edge');
  }

  // One error exit target at most.
  const errorTargets = new Set(outError.map((e) => e.to));
  if (errorTargets.size > 1) {
    throw new Error(
      `All error exits must share one target (an instance has a single error port) — found: ${[...errorTargets].join(', ')}`
    );
  }

  // Output boundaries: one per non-error exit edge, named after its source
  // port (`success`/`out` -> the `output` boundary = instance port `success`).
  const takenIds = new Set(selectedNodes.map((n) => n.id));
  const uniquify = (base: string): string => {
    let candidate = base;
    let i = 2;
    while (takenIds.has(candidate) || (candidate !== 'output' && RESERVED.includes(candidate))) {
      candidate = `${base}-${i++}`;
    }
    takenIds.add(candidate);
    return candidate;
  };
  const exits = outNonError.map((e) => {
    const [, fromPort] = splitEdge(e.from);
    const normalized = fromPort === 'out' ? 'success' : fromPort;
    const boundaryId = uniquify(normalized === 'success' ? 'output' : normalized);
    return { edge: e, boundaryId, port: boundaryId === 'output' ? 'success' : boundaryId };
  });

  // Geometry for the boundary pseudo-nodes.
  const pos = (n: PolicyNode) => n.position ?? { x: 0, y: 0 };
  const xs = selectedNodes.map((n) => pos(n).x);
  const ys = selectedNodes.map((n) => pos(n).y);
  const minX = Math.min(...xs), maxX = Math.max(...xs);
  const minY = Math.min(...ys), maxY = Math.max(...ys);
  const entryY = pos(selectedNodes.find((n) => n.id === entryId)!).y;

  const definition: Supernode = {
    name,
    nodes: [
      { id: 'input', type: 'input', config: {}, position: { x: minX - 250, y: entryY } },
      ...exits.map((x, i) => ({
        id: x.boundaryId, type: 'output', config: {},
        position: { x: maxX + 250, y: minY + i * 100 },
      })),
      { id: 'error', type: 'error', config: {}, position: { x: maxX + 250, y: maxY + 180 } },
      ...selectedNodes.map((n) => ({ ...n })),
    ],
    edges: [
      { from: 'input.out', to: `${entryId}.in` },
      ...inner,
      ...exits.map((x) => ({ from: x.edge.from, to: `${x.boundaryId}.in` })),
      ...outError.map((e) => ({ from: e.from, to: 'error.in' })),
    ],
  };

  // Rewritten policy: selection replaced by one instance node.
  const remaining = policy.nodes.filter((n) => !selected.has(n.id));
  let instanceId = name;
  while (remaining.some((n) => n.id === instanceId) || instanceId.includes('/')) {
    instanceId = `${name}-${Math.floor(Math.random() * 36 ** 4).toString(36)}`;
  }
  const centroid = {
    x: Math.round(xs.reduce((a, b) => a + b, 0) / xs.length),
    y: Math.round(ys.reduce((a, b) => a + b, 0) / ys.length),
  };
  const instance: PolicyNode = {
    id: instanceId,
    type: 'supernode',
    config: { name },
    position: centroid,
  };

  const newEdges: PolicyEdge[] = [
    ...outside,
    ...inbound.map((e) => ({ from: e.from, to: `${instanceId}.in` })),
    ...exits.map((x) => ({ from: `${instanceId}.${x.port}`, to: x.edge.to })),
    ...(outError.length > 0
      ? [{ from: `${instanceId}.error`, to: outError[0].to }]
      : []),
  ];

  return {
    definition,
    policy: { ...policy, nodes: [...remaining, instance], edges: newEdges },
    instanceId,
  };
}
```

Note: dedupe of multiple error exits to the SAME target collapses naturally (`outError[0].to`); duplicate success exits produce `output`, `output-2`, ... via `uniquify`.

- [ ] **Step 4: Run tests**

Run: `cd ui && npx vitest run src/extractSupernode.test.ts`
Expected: PASS. Fix the implementation (not the tests) on mismatch, unless a test itself contradicts the spec.

- [ ] **Step 5: Commit**

```bash
git add ui/src/extractSupernode.ts ui/src/extractSupernode.test.ts
git commit -m "feat(ui): pure extract-selection-to-supernode helper"
```

---

### Task 6: UI — wire extraction into the editor (toolbar, palette, context menu)

**Files:**
- Modify: `ui/src/components/GraphCanvas.tsx` (selection tracking, toolbar button, context menu, name dialog, editor action)
- Modify: `ui/src/commands.ts` (palette entry)
- Modify: `ui/src/App.tsx` (create-definition handler prop)

**Interfaces:**
- Consumes: `extractSupernode`/`ExtractionResult` (Task 5), `policyToNodes`/`policyToEdges` with the `supernodes` param (Task 3), `useRegisterEditorAction` (existing), `Dialog` components.
- Produces: GraphCanvas prop `onCreateSupernodeDef?: (sn: Supernode) => Promise<boolean>`; palette command id `extract-supernode`.

- [ ] **Step 1: App handler.** In `App.tsx`, next to `submitCreateSupernode`:

```ts
const handleCreateSupernodeDef = useCallback(async (sn: Supernode): Promise<boolean> => {
  try {
    await api.updateSupernode(sn.name, sn);
    await loadData();
    setToast({ tone: 'success', title: 'Supernode created', message: sn.name });
    return true;
  } catch (e) {
    setToast({ tone: 'error', title: 'Failed to create supernode', message: `${e}` });
    return false;
  }
}, [loadData]);
```

Pass `onCreateSupernodeDef={handleCreateSupernodeDef}` to the `<GraphCanvas>` (policy editor instance).

- [ ] **Step 2: GraphCanvas — selection + eligibility.**

```ts
const selectedNodes = nodes.filter((n) => n.selected);
const extractEligible =
  kind === 'policy' &&
  !!onCreateSupernodeDef &&
  selectedNodes.length >= 2 &&
  selectedNodes.every(
    (n) => !['listener', 'client', 'supernode'].includes(
      (n.data as unknown as PluginNodeData).pluginType
    )
  );
```

State: `const [extractDialogOpen, setExtractDialogOpen] = useState(false);` plus `extractName`/`extractError` strings. Submit handler:

```ts
const submitExtract = async () => {
  const name = extractName.trim();
  if (!name) { setExtractError('A supernode name is required'); return; }
  if (supernodes.some((s) => s.name === name)) {
    setExtractError(`Supernode '${name}' already exists`); return;
  }
  if (!policy || !onCreateSupernodeDef) return;
  let result: ExtractionResult;
  try {
    result = extractSupernode(
      nodesToPolicy(policy.name, nodes, edges, policy.error_handler),
      selectedNodes.map((n) => n.id),
      name
    );
  } catch (e) {
    setExtractError(e instanceof Error ? e.message : `${e}`);
    return;
  }
  setExtractDialogOpen(false);
  if (!(await onCreateSupernodeDef(result.definition))) return;
  // Rebuild canvas state from the rewritten policy; include the fresh
  // definition so the instance renders its derived ports immediately.
  const defs = [...supernodes, result.definition];
  setNodes(policyToNodes(result.policy, handleSelect, portSpecs, showPortNames, defs));
  setEdges(policyToEdges(result.policy, portSpecs, defs));
  setSelectedNodeId(result.instanceId);
};
```

The trigger handler (shared by all three entry points):

```ts
const handleExtract = useCallback(() => {
  if (!extractEligible) {
    onSaveWarning?.(
      'Extract selection',
      'Select two or more nodes (no listener/client/supernode) to extract.'
    );
    return;
  }
  setExtractName('');
  setExtractError(null);
  setExtractDialogOpen(true);
}, [extractEligible, onSaveWarning]);
```

Register the editor action next to the existing ones (above the early return): `useRegisterEditorAction('extract-supernode', handleExtract);`.

- [ ] **Step 3: Toolbar button.** In the floating toolbar, after "Add Node", render only when `extractEligible`:

```tsx
{extractEligible && (
  <button
    onClick={handleExtract}
    style={toolbarButtonStyle('var(--surface-input)')}
    onMouseEnter={(e) => (e.currentTarget.style.filter = 'brightness(1.08)')}
    onMouseLeave={(e) => (e.currentTarget.style.filter = 'none')}
  >
    <Boxes size={13} />
    Extract Supernode
  </button>
)}
```

(Import `Boxes` from lucide-react; give the button `color: 'var(--text-primary)', border: '1px solid var(--border)'` like Add Node.)

- [ ] **Step 4: Context menu.** State `const [ctxMenu, setCtxMenu] = useState<{ x: number; y: number } | null>(null);`. On the `<ReactFlow>` element add:

```tsx
onSelectionContextMenu={(e) => { e.preventDefault(); setCtxMenu({ x: e.clientX, y: e.clientY }); }}
onNodeContextMenu={(e, node) => {
  if (!node.selected) return;
  e.preventDefault();
  setCtxMenu({ x: e.clientX, y: e.clientY });
}}
onPaneClick={() => { setSelectedNodeId(null); setSelectedEdgeId(null); setCtxMenu(null); }}
```

Render (fixed-position, closes on click):

```tsx
{ctxMenu && (
  <div
    style={{
      position: 'fixed', left: ctxMenu.x, top: ctxMenu.y, zIndex: 100,
      background: 'var(--surface)', border: '1px solid var(--border)',
      borderRadius: 'var(--radius-sm)', boxShadow: 'var(--shadow-md)', padding: 4,
    }}
    onMouseLeave={() => setCtxMenu(null)}
  >
    <button
      onClick={() => { setCtxMenu(null); handleExtract(); }}
      disabled={!extractEligible}
      style={{
        display: 'block', padding: '6px 12px', fontSize: 'var(--text-sm)',
        color: extractEligible ? 'var(--text-primary)' : 'var(--text-muted)',
        background: 'transparent', width: '100%', textAlign: 'left',
      }}
    >
      Extract selection as supernode…
    </button>
  </div>
)}
```

- [ ] **Step 5: Name dialog.** Same `Dialog` pattern as Task 4's port dialog: title "Extract selection as supernode", one `DialogField` labeled "Supernode name" (placeholder `auth-guard`), inline `extractError` line, Cancel + "Extract" buttons calling `submitExtract`.

- [ ] **Step 6: Palette entry.** In `ui/src/commands.ts` after `save-graph`:

```ts
{
  id: 'extract-supernode',
  title: 'Extract selection as supernode',
  // Same editorOpen guard as add-plugin above; eligibility (2+ nodes
  // selected, policy mode) is checked by the canvas handler, which
  // explains itself via a toast when the selection doesn't qualify.
  when: (c) => c.editorOpen && c.hasEditorAction('extract-supernode'),
  run: (c) => c.invokeEditorAction('extract-supernode'),
},
```

- [ ] **Step 7: Verify**

Run: `cd ui && npm test && npm run lint && npm run build`
Expected: PASS.

- [ ] **Step 8: Commit**

```bash
git add ui/src/components/GraphCanvas.tsx ui/src/commands.ts ui/src/App.tsx
git commit -m "feat(ui): extract a multi-node selection into a supernode from the policy editor"
```

---

### Task 7: End-to-end coverage

**Files:**
- Modify: `e2e/tests/supernodes.spec.ts` (extend — read its existing helpers/fixtures first and follow them exactly)
- Modify: `e2e/E2E_TESTBOOK.md` (catalog entries)

**Interfaces:**
- Consumes: the running feature from Tasks 1-6; the suite's own gateway-boot fixtures (it boots the release binary itself).
- Produces: two scenarios, ids continuing the testbook's existing SN-numbering convention.

- [ ] **Step 1: Read `e2e/tests/supernodes.spec.ts` and `e2e/E2E_TESTBOOK.md`** to pick up the harness helpers (gateway boot, admin API helpers, UI selectors) and the exact scenario-id format used there. Reuse them; do not invent new helpers.

- [ ] **Step 2: Data-plane scenario — named port routed end-to-end.** Via the Admin API, create a supernode `header-gate` wrapping a `condition` node checking a request header, with `condition.true -> output.in` and `condition.false -> blocked.in` (`blocked` a `type: output` boundary), and a policy `gate-policy` with edges `gate.success -> up.in` (echo upstream) and `gate.blocked -> deny.in` where `deny` is a node that produces a distinguishable response (follow whatever the existing supernode scenarios use for a deny path — e.g. an `error-handler` or `response-rewrite` returning a fixed status). Assert:
  - a request WITH the header reaches the echo backend (status/body from echo);
  - a request WITHOUT the header gets the deny response;
  - saving the policy with `gate.blocked` unwired fails, and the Admin API error message contains `output port 'blocked' of supernode instance` (mandatory wiring surfaces to the API caller).

- [ ] **Step 3: UI scenario — extraction flow.** Playwright: open a policy that has ≥2 adjacent middle nodes (reuse or seed one via the harness), box-select the two middle nodes (`page.keyboard.down('Shift')` + drag, or click+shift-click the two nodes), click the toolbar "Extract Supernode" button, type a name into the dialog, confirm. Assert:
  - the new supernode appears in the sidebar library list;
  - the canvas now shows one node whose label is `⬡ <name>`;
  - clicking Save Policy succeeds (no error toast);
  - a data-plane request through the route still round-trips to the echo backend.

- [ ] **Step 4: Testbook entries.** Add one row/section per scenario to `e2e/E2E_TESTBOOK.md`, following its existing structure (id, title, preconditions, steps, expected), e.g. `E2E-SN-04 Named output ports route end-to-end` and `E2E-SN-05 Extract selection as supernode`. Match the numbering to whatever ids already exist in the file.

- [ ] **Step 5: Run the suite**

Run: `cargo build --release && cd e2e && npm install && npx playwright install chromium && npm test`
Expected: PASS, including all pre-existing scenarios (regression check on the expansion changes).

- [ ] **Step 6: Commit**

```bash
git add e2e/tests/supernodes.spec.ts e2e/E2E_TESTBOOK.md
git commit -m "test(e2e): named supernode ports end-to-end and editor extraction flow"
```

---

### Task 8: Documentation + final verification

**Files:**
- Modify: `website/docs/concepts/supernodes.md`
- Modify: `website/docs/guides/web-ui.md`
- Modify: `CLAUDE.md` (Supernodes bullet in "Core features" / the UI paragraph under "Not Yet Implemented")

**Interfaces:**
- Consumes: everything above.
- Produces: shipped docs; a green full build.

- [ ] **Step 1: `website/docs/concepts/supernodes.md`.** Add a "Named output ports" section: any number of `type: output` boundary nodes; node id = instance port name; id `output` = the `success` port (alias `out`); reserved ids `input`, `error`, `in`, `out`, `success`; every output-derived port is mandatory-wired in policies while `error` stays optional. Include the spec's `auth-gate` YAML example (definition + policy edges wiring `gate.success` and `gate.denied`). Update any wording that still says "exactly one output".

- [ ] **Step 2: `website/docs/guides/web-ui.md`.** Document: the supernode editor's "Output port" palette entry and the rename flow (inspector → Rename); the policy editor's instance nodes showing one row per definition port; extraction (multi-select 2+ nodes → toolbar button / right-click → "Extract selection as supernode…" / Ctrl+K command), what gets auto-derived (entry, per-exit ports, single error), and the ineligibility rules (no listener/client/supernode in the selection, single entry, one error target).

- [ ] **Step 3: `CLAUDE.md`.** In the Supernodes core-features bullet, note named output ports (`output` boundary nodes = instance ports). In the UI paragraph, mention the extraction command. Keep both edits to one sentence each.

- [ ] **Step 4: Full verification (superpowers:verification-before-completion)**

Run, in order, and confirm each is green before claiming done:
```bash
cargo test
cargo build --release
cd ui && npm test && npm run lint && npm run build && cd ..
cd e2e && npm test && cd ..
cd website && npm run build && cd ..
```

- [ ] **Step 5: Commit docs**

```bash
git add website/docs/concepts/supernodes.md website/docs/guides/web-ui.md CLAUDE.md
git commit -m "docs: named supernode output ports and editor extraction"
```

- [ ] **Step 6: Hand off for delivery.** Do NOT push or open a PR yet: per the delivery workflow, report completion and wait for Francesco's go-ahead, then push `feature/supernode-named-ports` and open a PR to `develop` (PR body per repo conventions; no Co-Authored-By).
