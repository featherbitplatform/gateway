# Supernode Named Error Ports + Boundary Editing Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Error boundaries become plural/nameable/creatable/deletable (mirroring output boundaries, with the `error` id as the black-box default), boundary ports get inspector delete with a last-of-kind guard, and extraction gives each distinct error target its own error port.

**Architecture:** Pure extension of the shipped named-output-ports feature on the same branch. Validation drops the exactly-one-`error` rule for a one-or-more + per-kind reserved-id rule; expansion's outer-port lookup becomes id-based across both exit kinds with `exit_to.get("error")` as the black-box target; the UI generalizes the existing output-port dialog/palette/rename machinery by a `kind` parameter and adds a guarded Delete button for boundaries.

**Tech Stack:** Rust, React + TypeScript + ReactFlow, Vitest, Playwright.

**Spec:** `docs/superpowers/specs/2026-08-20-supernode-error-ports-design.md` (extends `2026-08-20-supernode-named-output-ports-design.md`)

## Global Constraints

- Conventional Commits, **no Co-Authored-By trailer**.
- Work continues on the existing branch `feature/supernode-named-ports` (PR #27). Do not push; the controller pushes at the end.
- `git add` only the files each task names — never `git add -A` (the working tree has unrelated pre-existing changes: tests/oidc-test.yml, .png files, another spec doc).
- Per-kind reserved boundary ids (exact): output → `input, error, in, out, success`; error → `input, output, in, out, success`. Specials: `output` id ⇄ instance port `success`; `error` id ⇄ instance port `error` **and** the only black-box target.
- ≥1 output boundary, ≥1 error boundary, exactly one `input` (id `input`). Output-derived ports mandatory-wired; ALL error-kind ports optional.
- **Run `cargo fmt` before every Rust commit** (CI runs `cargo fmt --check`; this was missed once already). Rust: `cargo test`. UI: `cd ui && npm test && npm run lint && npm run build`.
- Backward compatibility: every existing test not explicitly updated below must keep passing unchanged.

---

### Task 1: Backend — plural named error boundaries

**Files:**
- Modify: `src/graph/validation.rs` (boundary rules ~lines 138-232 + doc comment + tests)
- Modify: `src/graph/expand.rs` (Splice/lookup/black-box ~lines 96-260, 404-426 + module doc + tests)
- Modify: `src/config/gateway.rs` (SupernodeConfig struct + `nodes` field doc comments only)

**Interfaces:**
- Consumes: existing `RESERVED_OUTPUT_IDS`, `port_for_output_boundary`, `Splice.exit_to`.
- Produces: `pub(crate) const RESERVED_ERROR_IDS: [&str; 5] = ["input", "output", "in", "out", "success"];` in validation.rs. Expansion contract for the UI (Tasks 2-3 mirror it): error boundary id = error-kind instance port name; black-box wiring exists only via the `error`-id boundary; unknown-port messages list output ports then error ports.

- [ ] **Step 1: Write the failing validation tests** (in `src/graph/validation.rs` tests module):

```rust
/// Named error ports: any number of `type: error` boundary nodes (spec §1-2).
#[test]
fn test_supernode_multiple_error_boundaries_accepted() {
    let mut sn = valid_supernode();
    sn.nodes.push(inner("auth-error", "error"));
    // `up.error` already exits via the default `error` boundary; the extra
    // named error boundary may stay unconnected (boundaries are orphan-exempt).
    assert_eq!(validate_supernode(&sn), Ok(()));
}

/// A definition whose only error boundary is renamed away from `error` is
/// legal — the instance then has no default black-box exit.
#[test]
fn test_supernode_renamed_only_error_boundary_accepted() {
    let mut sn = valid_supernode();
    sn.nodes
        .iter_mut()
        .find(|n| n.node_type == "error")
        .unwrap()
        .id = "oops".into();
    sn.edges
        .iter_mut()
        .find(|e| e.to == "error.in")
        .unwrap()
        .to = "oops.in".into();
    assert_eq!(validate_supernode(&sn), Ok(()));
}

#[test]
fn test_supernode_zero_error_boundaries_rejected() {
    let mut sn = valid_supernode();
    sn.nodes.retain(|n| n.node_type != "error");
    sn.edges.retain(|e| !e.to.starts_with("error."));
    let errors = validate_supernode(&sn).unwrap_err();
    assert!(
        errors
            .iter()
            .any(|e| e.contains("at least one 'error' boundary")),
        "{errors:?}"
    );
}

#[test]
fn test_supernode_reserved_error_ids_rejected() {
    for id in ["input", "output", "in", "out", "success"] {
        let mut sn = valid_supernode();
        sn.nodes.push(inner(id, "error"));
        let errors = validate_supernode(&sn).unwrap_err();
        assert!(
            errors.iter().any(|e| e.contains("reserved")),
            "id {id}: {errors:?}"
        );
    }
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test test_supernode_multiple_error_boundaries_accepted test_supernode_renamed_only_error_boundary_accepted test_supernode_zero_error_boundaries_rejected test_supernode_reserved_error_ids_rejected`
Expected: FAIL — multiple/renamed error boundaries trip "declares more than one 'error' node" / "must have id 'error'"; the zero test fails on message mismatch ("must declare an 'error' boundary node" vs "at least one").

- [ ] **Step 3: Implement validation.** Add next to `RESERVED_OUTPUT_IDS`:

```rust
/// Error-boundary ids that would collide with an instance's fixed port
/// names (spec §1). `error` is the one special id: it is both the port
/// name and the black-box default exit.
pub(crate) const RESERVED_ERROR_IDS: [&str; 5] = ["input", "output", "in", "out", "success"];
```

Shrink the `for ty in ["input", "error"]` loop to `input` only (keep the identical match arms, with `"input"` inlined for `ty`). Then add an error block mirroring the output block, directly after it:

```rust
// error: one or more; each id is an error-kind instance port name (`error`
// is the default the black-box rule targets), so reserved ids that collide
// with fixed port names are rejected.
let error_nodes: Vec<&crate::config::NodeConfig> = sn
    .nodes
    .iter()
    .filter(|n| n.node_type == "error")
    .collect();
if error_nodes.is_empty() {
    errors.push(format!(
        "Supernode '{}' must declare at least one 'error' boundary node",
        sn.name
    ));
}
for e in &error_nodes {
    if RESERVED_ERROR_IDS.contains(&e.id.as_str()) {
        errors.push(format!(
            "Supernode '{}': error boundary id '{}' is reserved — it would \
             collide with a fixed instance port name",
            sn.name, e.id
        ));
    }
}
```

Update the fn doc comment: "exactly one boundary node of type `input`, id `input`; one or more `output` boundaries (id = port name, `output` = the `success` port); one or more `error` boundaries (id = error-kind port name, `error` = the default the black-box rule targets)". The direction rule (`exit_boundary_ids`) and orphan exemption already cover plural errors — no change.

`test_supernode_missing_boundary_nodes` (removes the error node) keeps passing: the new message still contains `'error' boundary node`.

- [ ] **Step 4: Write the failing expansion tests** (in `src/graph/expand.rs` tests module):

```rust
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
        edges: vec![
            edge("input.out", "up.in"),
            edge("up.success", "output.in"),
        ],
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
```

- [ ] **Step 5: Run to verify failure**

Run: `cargo test --lib expand`
Expected: the new tests FAIL (`auth-error`/`oops` rejected as unknown ports by the current error-by-type lookup; validation Step 3 already allows the definitions).

- [ ] **Step 6: Implement expansion.**
  1. In `Splice`, delete the `error_node_id: Option<String>` field (and its construction). Update the `exit_to` doc: "Mandatory-wiring guarantees an entry for every output boundary; error-kind boundaries' entries are optional."
  2. Delete the `error_node_id` by-type lookup and add, next to `output_ids`:

```rust
let error_ids: Vec<String> = def
    .nodes
    .iter()
    .filter(|n| n.node_type == "error")
    .map(|n| n.id.clone())
    .collect();
```

  3. Replace the `boundary_id` computation in the outer-edge scan (the `if port == "error" { error_node_id.clone() } else { ... }` block) with the id-based lookup — note `port_for_output_boundary` keeps port `output` unknown (it maps the `output` id to `success`), and error ids ARE their ports:

```rust
let boundary_id = output_ids
    .iter()
    .find(|id| port_for_output_boundary(id) == port)
    .or_else(|| error_ids.iter().find(|id| id.as_str() == port))
    .cloned();
```

  4. In the unknown-port arm, replace `exposed.push("error");` with:

```rust
exposed.extend(error_ids.iter().map(|id| id.as_str()));
```

  5. Replace the black-box condition — `s.error_node_id.as_ref().and_then(|id| s.exit_to.get(id.as_str()))` becomes `s.exit_to.get("error")` — and update its comment: only the DEFAULT `error`-id boundary carries the black-box guarantee; reserved-id rules keep any other boundary kind off that key. (`exit_to` can only hold key `error` when the definition has an `error`-id error boundary and the policy wired it: output ids can't be `error`, and the lookup above inserts by boundary id.)
  6. Update `resolve_target`'s `None => return Ok(None)` comment from "an unwired error boundary" to "an unwired error-kind boundary", the inner-splice drop comment ("Unwired error-kind boundary: drop…"), and the module doc comment (multiple named error boundaries; `error` id = default/black-box).
  7. Update `SupernodeConfig` doc comments in `src/config/gateway.rs`: struct doc and the `nodes` field doc now say "one `input`, one or more `output`, and one or more `error` pseudo-nodes; each output/error node's id is an instance port name (`output` = the `success` port; `error` = the default error port)".

- [ ] **Step 7: Full suite + fmt**

Run: `cargo test && cargo fmt && cargo fmt --check`
Expected: all pass. No existing expand/validation test asserts the removed by-type error lookup (`error_pass_through`/`secured_call` fixtures all use id `error`); if anything else fails, fix the fixture and record it in the report.

- [ ] **Step 8: Commit**

```bash
git add src/graph/validation.rs src/graph/expand.rs src/config/gateway.rs
git commit -m "feat(graph): plural named error boundaries with default black-box error port"
```

---

### Task 2: UI — error-port editing parity + boundary delete

**Files:**
- Modify: `ui/src/policyGraph.ts` (`supernodePortSpec` ~lines 60-97)
- Modify: `ui/src/portNameValidation.ts` (per-kind reserved lists)
- Modify: `ui/src/components/GraphCanvas.tsx` (portDialog kind, boundary handlers ~lines 585-651, drawer/inspector props ~lines 920-960)
- Modify: `ui/src/components/PluginDrawer.tsx` (`onAddOutputPort` → `onAddBoundaryPort`, second row)
- Modify: `ui/src/components/NodeInspector.tsx` (rename gate ~line 354, delete section ~line 498, new prop)
- Test: `ui/src/policyGraph.test.ts`, `ui/src/portNameValidation.test.ts`

**Interfaces:**
- Consumes: Task 1's contract (error boundary id = port name, `error` = default).
- Produces: `validatePortName(name: string, takenIds: string[], kind: 'output' | 'error', selfId?: string): string | null`; PluginDrawer prop `onAddBoundaryPort?: (kind: 'output' | 'error') => void` (replaces `onAddOutputPort`); NodeInspector props `onRenameNode?: (nodeId: string) => void` (unchanged shape, now also fired for error boundaries) and `boundaryDeleteBlocked?: string` (undefined = boundary Delete enabled; string = disabled with this tooltip).

- [ ] **Step 1: Write the failing tests.** In `ui/src/portNameValidation.test.ts`, update every existing call for the new `kind` argument (existing cases use `'output'`) and add:

```ts
it('reserves per kind: error boundaries may not shadow output specials and vice versa', () => {
  expect(validatePortName('output', [], 'error')).toMatch(/reserved/);
  expect(validatePortName('error', [], 'output')).toMatch(/reserved/);
  // each kind's own special id is legal (dupes are caught by takenIds)
  expect(validatePortName('error', [], 'error')).toBeNull();
  expect(validatePortName('output', [], 'output')).toBeNull();
});
```

In `ui/src/policyGraph.test.ts` add to the `supernodePortSpec` describe (reusing `gateDef`):

```ts
it('derives one error-kind port per error boundary, default first per definition order', () => {
  const multiError: Supernode = {
    ...gateDef,
    nodes: [...gateDef.nodes, { id: 'auth-error', type: 'error', config: {} }],
  };
  const spec = supernodePortSpec(multiError)!;
  expect(spec.outputs.map((p) => [p.name, p.kind])).toEqual([
    ['success', 'success'],
    ['denied', 'outcome'],
    ['error', 'error'],
    ['auth-error', 'error'],
  ]);
});

it('renamed-only error boundary yields no `error` port', () => {
  const renamed: Supernode = {
    ...gateDef,
    nodes: gateDef.nodes.map((n) => (n.id === 'error' ? { ...n, id: 'oops' } : n)),
  };
  const spec = supernodePortSpec(renamed)!;
  expect(spec.outputs.map((p) => p.name)).toEqual(['success', 'denied', 'oops']);
});
```

Run: `cd ui && npx vitest run src/portNameValidation.test.ts src/policyGraph.test.ts` — expected FAIL (signature + hardcoded single error port).

- [ ] **Step 2: Implement `portNameValidation.ts`:**

```ts
/**
 * Validates a boundary id (= the instance port name it exposes), per kind.
 * Mirrors src/graph/validation.rs::RESERVED_OUTPUT_IDS / RESERVED_ERROR_IDS:
 * each kind's special id (`output` -> success, `error` -> default error) is
 * legal for its own kind and reserved for the other.
 */
const RESERVED: Record<'output' | 'error', string[]> = {
  output: ['input', 'error', 'in', 'out', 'success'],
  error: ['input', 'output', 'in', 'out', 'success'],
};

export function validatePortName(
  name: string,
  takenIds: string[],
  kind: 'output' | 'error',
  selfId?: string
): string | null {
  if (!name.trim()) return 'A port name is required';
  if (RESERVED[kind].includes(name)) return `'${name}' is a reserved port name`;
  if (name.includes('/')) return "Port names must not contain '/'";
  if (takenIds.some((id) => id === name && id !== selfId))
    return `A node named '${name}' already exists`;
  return null;
}
```

- [ ] **Step 3: Implement `supernodePortSpec`.** Replace the hardcoded trailing `error` push with one port per error boundary (definition order):

```ts
for (const n of def.nodes.filter((n) => n.type === 'error')) {
  outputs.push({
    name: n.id,
    kind: 'error',
    description:
      n.id === 'error'
        ? `Default error exit of '${def.name}' (optional; unhandled inner errors leave here).`
        : `Error exit through the '${n.id}' boundary of '${def.name}' (optional wiring).`,
  });
}
```

Update the fn doc comment accordingly (mirror now covers RESERVED_ERROR_IDS / the `error` default).

- [ ] **Step 4: GraphCanvas.** Extend the dialog state with a kind: `{ mode: 'add'; kind: 'output' | 'error' } | { mode: 'rename'; nodeId: string; kind: 'output' | 'error' } | null`. Rename `handleAddOutputPort` → `handleAddBoundaryPort(kind)` (sets the kind in state) and `handleRenameOutputPort` → `handleRenameBoundaryPort(nodeId)` — it reads the node's `pluginType` to fill the kind:

```ts
const handleRenameBoundaryPort = (nodeId: string) => {
  const n = nodes.find((n) => n.id === nodeId);
  const t = (n?.data as unknown as PluginNodeData | undefined)?.pluginType;
  if (t !== 'output' && t !== 'error') return;
  setPortName(nodeId);
  setPortError(null);
  setPortDialog({ mode: 'rename', nodeId, kind: t });
};
```

`submitPortDialog`: pass `portDialog.kind` to `validatePortName`, and the add branch creates the node with `pluginType: portDialog.kind`. Dialog title becomes `` `${portDialog?.kind === 'error' ? 'Error' : 'Output'} port name` ``. Prop pass-downs: `onAddBoundaryPort={kind === 'supernode' ? handleAddBoundaryPort : undefined}` to PluginDrawer; NodeInspector keeps `onRenameNode={kind === 'supernode' ? handleRenameBoundaryPort : undefined}` and gains:

```ts
// Boundary delete guard: the Delete button shows for output/error boundary
// nodes in supernode mode, but is disabled on the last one of its kind —
// validate_supernode requires at least one of each (server stays authority;
// keyboard delete can bypass and the save then fails with a clear message).
const boundaryDeleteBlocked = (() => {
  if (kind !== 'supernode' || !selectedNode) return undefined;
  const t = (selectedNode.data as unknown as PluginNodeData).pluginType;
  if (t !== 'output' && t !== 'error') return undefined;
  const count = nodes.filter(
    (n) => (n.data as unknown as PluginNodeData).pluginType === t
  ).length;
  return count <= 1 ? `A supernode needs at least one ${t} boundary` : undefined;
})();
```

passed as `boundaryDeleteBlocked={boundaryDeleteBlocked}`.

- [ ] **Step 5: PluginDrawer.** Replace `onAddOutputPort?: () => void` with `onAddBoundaryPort?: (kind: 'output' | 'error') => void` (prop doc updated). The Boundary section renders two `NodeRow`s — the existing Output row calling `onAddBoundaryPort('output')`, plus:

```tsx
<NodeRow
  onClick={() => onAddBoundaryPort('error')}
  color={getPluginMeta('error').color}
  icon={(() => { const I = getPluginMeta('error').icon; return <I size={15} strokeWidth={1.75} />; })()}
  title="Error port"
  subtitle="Named error exit — optional wiring on every instance"
/>
```

Search matching: the section (and each row) shows when the query matches `'output port'` or `'error port'` respectively (extend the existing `outputPortMatches` logic to per-row `outputPortMatches`/`errorPortMatches`, and the `nothingMatches` computation to account for both).

- [ ] **Step 6: NodeInspector.** (a) Rename gate: `data.pluginType === 'output'` becomes `(data.pluginType === 'output' || data.pluginType === 'error')`. (b) Add props `kind` is already present; add `boundaryDeleteBlocked?: string`. (c) Delete section: change the render condition from `!isFixed` to also show for supernode-mode boundaries, with the guard:

```tsx
const isBoundaryPort =
  kind === 'supernode' && (data.pluginType === 'output' || data.pluginType === 'error');
...
{(!isFixed || isBoundaryPort) && (
  <div style={{ padding: 16, borderTop: '1px solid var(--border)' }}>
    <button
      onClick={() => onDeleteNode(node.id)}
      disabled={isBoundaryPort && !!boundaryDeleteBlocked}
      title={isBoundaryPort ? boundaryDeleteBlocked : undefined}
      ...existing styles, plus opacity/cursor tweaks when disabled...
    >
      Delete Node
    </button>
  </div>
)}
```

(`input`/`listener`/`client` remain excluded: they are `isFixed` and not boundary ports.)

- [ ] **Step 7: Verify**

Run: `cd ui && npm test && npm run lint && npm run build`
Expected: PASS (build catches any missed `validatePortName`/prop call site).

- [ ] **Step 8: Commit**

```bash
git add ui/src/policyGraph.ts ui/src/policyGraph.test.ts ui/src/portNameValidation.ts ui/src/portNameValidation.test.ts ui/src/components/GraphCanvas.tsx ui/src/components/PluginDrawer.tsx ui/src/components/NodeInspector.tsx
git commit -m "feat(ui): error-port editing parity and guarded boundary deletion in the supernode editor"
```

---

### Task 3: Extraction — one error port per distinct target

**Files:**
- Modify: `ui/src/extractSupernode.ts` (error grouping, ~lines 77-83, 112-129, 148-155)
- Test: `ui/src/extractSupernode.test.ts`

**Interfaces:**
- Consumes/produces: `extractSupernode` signature and `ExtractionResult` unchanged. New behavior: error exits grouped by outer target; first group (edge order) → `error` boundary, later groups → `error-2`, `error-3`, … via the existing `uniquify` (whose RESERVED skip makes base `error` yield `error-2` automatically).

- [ ] **Step 1: Update/add tests.** In `ui/src/extractSupernode.test.ts`:
  1. Rewrite `rejects conflicting outer error targets` → `gives each distinct error target its own error port`: same fixture setup (add `eh2` + `rl.error -> eh2.in` + `eh2.success -> client.in`), then assert instead of throwing:

```ts
const { definition, policy: rewritten, instanceId } = extractSupernode(p, ['auth', 'rl'], 'guard');
const errorBoundaries = definition.nodes.filter((n) => n.type === 'error').map((n) => n.id).sort();
expect(errorBoundaries).toEqual(['error', 'error-2']);
const defEdges = definition.edges.map((e) => `${e.from}->${e.to}`);
expect(defEdges).toContain('auth.error->error.in');
expect(defEdges).toContain('rl.error->error-2.in');
const polEdges = rewritten.edges.map((e) => `${e.from}->${e.to}`);
expect(polEdges).toContain(`${instanceId}.error->eh.in`);
expect(polEdges).toContain(`${instanceId}.error-2->eh2.in`);
```

  2. Add a same-target collapse regression: the base fixture (auth.error and — add — `rl.error -> eh.in`, same target) still produces exactly one `error` boundary and one `inst.error -> eh.in` policy edge.
  3. The existing main-fixture tests keep passing unchanged (single error target).

Run: `cd ui && npx vitest run src/extractSupernode.test.ts` — expected FAIL (rejection still fires / grouping absent).

- [ ] **Step 2: Implement.** Delete the `errorTargets.size > 1` rejection block. After the `exits` computation (so output boundaries claim their names first in `takenIds`), group:

```ts
// Error exits group by outer target (edge order): the first group is the
// default `error` boundary (black-box exit), each further distinct target
// gets its own named error boundary (`error-2`, ... — renamable later).
// All derived error-kind ports are wired by construction; optional wiring
// only matters for ports added later in the editor.
const errorGroups: { target: string; boundaryId: string; edges: PolicyEdge[] }[] = [];
for (const e of outError) {
  let group = errorGroups.find((g) => g.target === e.to);
  if (!group) {
    group = {
      target: e.to,
      boundaryId: errorGroups.length === 0 ? 'error' : uniquify('error'),
      edges: [],
    };
    errorGroups.push(group);
  }
  group.edges.push(e);
}
```

Definition nodes: replace the single hardcoded error node with one per group (fall back to the plain default boundary when there are no error exits at all — validation requires ≥1):

```ts
...(errorGroups.length > 0 ? errorGroups : [{ target: '', boundaryId: 'error', edges: [] as PolicyEdge[] }]).map(
  (g, i) => ({
    id: g.boundaryId,
    type: 'error',
    config: {},
    position: { x: maxX + 250, y: maxY + 180 + i * 100 },
  })
),
```

Definition edges: replace the `outError.map(... to: 'error.in')` line with `...errorGroups.flatMap((g) => g.edges.map((e) => ({ from: e.from, to: \`${g.boundaryId}.in\` })))`. Policy edges: replace the `outError.length > 0 ? [{ from: \`${instanceId}.error\`, ...}] : []` block with `...errorGroups.map((g) => ({ from: \`${instanceId}.${g.boundaryId}\`, to: g.target }))`.

- [ ] **Step 3: Verify**

Run: `cd ui && npm test && npm run lint && npm run build`
Expected: PASS.

- [ ] **Step 4: Commit**

```bash
git add ui/src/extractSupernode.ts ui/src/extractSupernode.test.ts
git commit -m "feat(ui): per-target error ports in supernode extraction"
```

---

### Task 4: e2e extension, docs, full verification

**Files:**
- Modify: `e2e/tests/supernodes.spec.ts` (E2E-SN-05 definition), `e2e/E2E_TESTBOOK.md` (E2E-SN-05 row)
- Modify: `website/docs/concepts/supernodes.md`, `website/docs/guides/web-ui.md`, `CLAUDE.md`

**Interfaces:** consumes everything above; produces docs + a green full battery.

- [ ] **Step 1: e2e.** In E2E-SN-05's supernode definition (read the current test first), add an extra error boundary `{ id: 'oops', type: 'error' }` (position optional) with no edges into it, and leave `gate.oops` unwired in the policy. The scenario's existing assertions all still hold; this additionally proves error-kind ports are optional at save time. Update the E2E-SN-05 testbook row's steps/expected accordingly (one clause).
- [ ] **Step 2: Docs.**
  - `website/docs/concepts/supernodes.md`: error boundaries are plural and named (id = port name), `error` id = default port + the black-box target (renaming it away removes implicit error wiring), per-kind reserved ids, at least one boundary of each exit kind; update the boundary-nodes table row for `error` mirroring the `output` row's "any number" treatment.
  - `website/docs/guides/web-ui.md`: "Error port" palette entry, rename for error boundaries, boundary deletion with the last-of-kind guard, extraction's per-target error ports (the shared-target rejection is gone).
  - `CLAUDE.md`: update the supernodes clause to mention named error ports (one clause; keep it surgical).
- [ ] **Step 3: Full verification (superpowers:verification-before-completion)** — run ALL, confirm green BEFORE committing, paste tails in the report:

```bash
cargo test
cargo fmt --check
cargo build --release
cd ui && npm test && npm run lint && npm run build && cd ..
cd e2e && npm test && cd ..
cd website && npm run build && cd ..
```

(Release build BEFORE e2e — the UI is embedded via rust-embed.)
- [ ] **Step 4: Commit**

```bash
git add e2e/tests/supernodes.spec.ts e2e/E2E_TESTBOOK.md website/docs/concepts/supernodes.md website/docs/guides/web-ui.md CLAUDE.md
git commit -m "docs+test: named error ports, boundary deletion, and per-target extraction errors"
```

- [ ] **Step 5: Hand off.** Do NOT push — the controller pushes to the existing PR #27 after review.
