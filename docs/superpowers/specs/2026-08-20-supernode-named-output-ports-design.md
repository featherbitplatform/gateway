# Supernode Named Output Ports + Extract-Selection-to-Supernode — Design

**Date:** 2026-08-20
**Status:** Approved for planning

## Problem

Node output ports moved from the old success/error pair to multiple named
outcome ports (`denied`, `limited`, `redirect`, …), but supernodes still
reflect the old paradigm: a definition has exactly one `output` boundary, so
an instance exposes exactly `success` (alias `out`) and `error`. Inner nodes
can use named outcome ports, but every non-error exit funnels into the single
`output` boundary and leaves the instance on `success`. A supernode wrapping
an auth check cannot expose `denied` as its own exit.

Separately, the editor has no way to turn an existing group of policy nodes
into a supernode — users must recreate the subgraph by hand in the supernode
editor.

## Goals

1. A supernode definition can declare an arbitrary set of named output
   ports; instances expose those ports in policies exactly like a plugin
   node's outcome ports.
2. In the policy editor, a multi-node selection can be extracted into a new
   supernode definition, with the selection replaced by a wired instance.

## Non-Goals (unchanged limitations)

- Nested supernodes (still rejected at expansion).
- Multiple input ports (exactly one `input` boundary, one edge from
  `input.out`).
- Named error ports (exactly one `error` boundary; instances keep a single
  optional `error` exit).

## 1. Config model (YAML)

A definition may declare **any number of `type: output` boundary nodes**.
Each output node's **id is the port name** the instance exposes, with one
backward-compatibility mapping: the output node with id `output` maps to the
instance port `success` (alias `out`), exactly as today.

```yaml
supernodes:
  - name: auth-gate
    nodes:
      - { id: input,   type: input }
      - { id: output,  type: output }   # -> instance port `success` (alias `out`)
      - { id: denied,  type: output }   # -> instance port `denied`
      - { id: error,   type: error }
      - { id: auth,    type: key-auth }
    edges:
      - { from: input.out,    to: auth.in }
      - { from: auth.success, to: output.in }
      - { from: auth.denied,  to: denied.in }

policies:
  - name: p
    nodes:
      - { id: gate, type: supernode, config: { name: auth-gate } }
      # ... listener, upstream, reject, client ...
    edges:
      - { from: gate.success, to: upstream.in }
      - { from: gate.denied,  to: reject.in }
```

Rules:

- `input` and `error` stay singular: exactly one node of each type, id equal
  to the type (unchanged).
- At least one `type: output` node is required, but a definition with only
  named outputs and no `output`-id node is legal — the instance then has no
  `success` port.
- Reserved output-node ids (rejected): `input`, `error`, `in`, `out`,
  `success`. These would collide with the fixed port names (`output` is the
  one special id, carrying the success mapping). Output ids must be unique
  and must not contain `/` (existing rule).
- `SupernodeConfig` (src/config/gateway.rs) needs **no schema change** — the
  boundary nodes already live in `nodes`. Only doc comments update.

## 2. Validation (`src/graph/validation.rs`)

`validate_supernode` changes:

- Replace "exactly one `output` node with id `output`" with: one or more
  `type: output` nodes, unique ids, no reserved ids (list above). The
  id-must-equal-type rule keeps applying to `input` and `error` only.
- Everything else is unchanged and applies to every output boundary: no
  outgoing edges from any output/error boundary, no incoming edges into
  `input`, every inner success/outcome port wired inside the definition,
  orphan rules (an unconnected output boundary is allowed, like today's
  unconnected `output`/`error`).

`validate_policy` is unchanged (it never sees boundary types outside
definitions).

## 3. Expansion (`src/graph/expand.rs`)

The `Splice` bookkeeping generalizes from the `success_to`/`error_to` pair to
per-port maps:

- **Recognized outer ports** on an instance: `error`, plus one port per
  output boundary in the definition (`output` ↔ `success`/`out`; any other
  boundary ↔ its id). An outer edge leaving the instance on any other port
  name is rejected (as today), and each port accepts at most one outer edge
  (duplicate rejection as today, including the `success` + `out` alias pair
  counting as duplicates).
- **Mandatory wiring:** every output-boundary-derived port of an instance
  must be wired in the policy, or expansion fails with an error naming the
  instance and port, e.g. `policy 'p': output port 'denied' of supernode
  instance 'gate' must be wired — add an edge from 'gate.denied'`. This
  matches the compile-time rule for plugin nodes (engine.rs) and replaces
  today's indirect failure (dropped exit edge surfacing later as a confusing
  unwired-inner-port compile error). `error` stays optional (policy
  catch-all / generic 500 takes over).
- **Splicing:** an inner edge into output boundary X is redirected to the
  target of the outer edge wired to X's port. With mandatory wiring, the
  drop-when-unwired path for output boundaries disappears; the drop path
  remains only for the optional `error` boundary.
- **Pass-through:** `input.out -> <some boundary>` resolves through that
  specific boundary's outer target (`pass_through_type` generalizes from
  `"output"|"error"` to the boundary node id). Chained pass-through
  resolution and cycle detection are unchanged in mechanism.
- **Black-box error rule unchanged:** inner nodes with unwired error ports
  get implicit error edges to the instance's outer error target when wired.

## 4. UI — editing definitions

- **Palette (supernode mode only):** a new "Output port" entry that can be
  dropped any number of times. Dropping one prompts for the port name (small
  dialog, same pattern as the create-supernode dialog), validated against
  the reserved-id list and existing node ids. `input` and `error` stay
  fixed/singular and are not in the palette.
- **Renaming:** the Node ID field in `NodeInspector` becomes editable for
  `type: output` boundary nodes in supernode mode (it is read-only for
  everything else, unchanged). Renaming rewrites the canvas edges that
  reference the old id — the id *is* the port name. Same validation as at
  drop time.
- **Instance ports derived from the definition:** on a policy canvas, a
  supernode instance's port rows come from the referenced definition
  (GraphCanvas already resolves `supernodeDef` into node data) instead of
  the hardcoded default success+error pair in `GraphCanvas.tsx`
  (`portSpecs['supernode']`). Order: `success` (only if an `output`-id
  boundary exists), then named ports in definition `nodes` order, then
  `error`. Named ports render as `outcome` kind (same styling as plugin
  outcome ports); a shared helper in `ui/src/policyGraph.ts` (or
  `nodeKinds.ts`) computes this port spec so `GraphCanvas`, `PluginNode`,
  and edge styling in `policyGraph.ts` all agree. When the referenced
  definition is missing (dangling name), fall back to the default pair as
  today.
- `SupernodePreview` needs no change (it renders the definition's nodes
  as-is; extra output boundaries just appear).

## 5. UI — extract selection to supernode

**Triggers** (all enabled only when the selection is 2+ eligible nodes, in
policy mode):

1. Command palette (Ctrl+K): "Extract selection as supernode…".
2. Toolbar button on the canvas.
3. Right-click context menu on a selected node (new lightweight canvas
   context menu; only this action for now).

Multi-select uses ReactFlow's existing shift-click / box-select.

**Eligibility & algorithm:**

1. The selection must not contain `listener`, `client`, or `supernode`
   nodes (definitions cannot nest these). Otherwise: error toast naming the
   offending node.
2. **Entry:** all external edges into the selection must target the same
   selected node. That node becomes the entry: `input.out -> entry.in`. Zero
   inbound edges, or inbound edges to two different selected nodes → error
   toast explaining the constraint.
3. **Named outputs:** each external non-error edge out of the selection
   (`n.port -> T`, T outside) becomes an output boundary:
   - port name = source port name; `success`/`out` exits map to the
     `output` boundary (instance port `success`);
   - duplicate names dedupe with numeric suffixes (`denied`, `denied-2`;
     a second success exit becomes `output-2`, exposed as port `output-2`);
   - inner edge `n.port -> <boundary>.in` is added to the definition;
   - outer edge `inst.<port> -> T` replaces the original in the policy.
4. **Error exits:** external `error` edges out of the selection collapse
   into the single `error` boundary (`n.error -> error.in`), and the policy
   gets `inst.error -> T`. If external error edges point at **different**
   outer targets, extraction is rejected with an error toast (an instance
   has one error exit).
5. Internal edges and node configs (including `config_ref`) are copied
   verbatim; node positions are preserved into the definition; `input`,
   output boundaries, and `error` get computed positions flanking the
   selection.
6. **Flow:** one dialog asking only the supernode name (reuse the existing
   create-supernode dialog, extended to accept seeded content). On confirm:
   the definition is created immediately via the Admin API
   (`api.updateSupernode`), then the canvas replaces the selected nodes with
   one wired instance node. The policy itself stays unsaved until the user
   saves, as with any canvas edit. If the API create fails, nothing on the
   canvas changes.

Undo note: the definition creation is a server-side effect; undoing the
canvas edit does not delete the definition (it remains in the library, which
is acceptable — supernodes are library items).

## 6. Testing

TDD throughout (superpowers:test-driven-development).

- **`src/graph/expand.rs`:** multi-output splice to distinct targets; named
  port on outer edge accepted and routed; unwired named port fails with the
  mandatory-wiring message; unwired `success` fails when an `output`-id
  boundary exists; definition without an `output`-id node rejects
  `inst.success`; pass-through via a named boundary (wired, and cycle
  detection through it); backward compat: existing single-output definitions
  and the `out` alias behave exactly as before.
- **`src/graph/validation.rs`:** multiple output boundaries accepted;
  reserved output ids rejected; duplicate output ids rejected; zero output
  nodes rejected; `input`/`error` singularity unchanged.
- **`ui/src/policyGraph.test.ts`:** instance port spec derived from a
  definition (ordering, success mapping, missing-definition fallback); edge
  styling for named instance ports.
- **Extraction unit tests** (pure helper in `ui/src`, e.g.
  `extractSupernode.ts` + test): entry detection, port synthesis + dedupe,
  error-exit collapse and the conflicting-error-targets rejection,
  eligibility rejections.
- **e2e (`e2e/E2E_TESTBOOK.md` + spec):** a policy wiring a named instance
  port end-to-end through the data plane; the editor extraction flow
  (select, extract, definition appears in sidebar, policy saves and
  serves).

## 7. Documentation

- `website/docs/concepts/supernodes.md` — named output ports, the
  `output`→`success` mapping, mandatory wiring.
- `website/docs/guides/web-ui.md` — output-port palette entry, rename,
  extraction flow.
- `CLAUDE.md` supernodes bullet and `src/graph/expand.rs` /
  `src/config/gateway.rs` doc comments.

## Compatibility

- Existing definitions (one `output`, one `error`) parse, validate, expand,
  and render identically. No stored-config migration.
- The Admin API surface is unchanged (definitions are the same shape; the
  new capability is purely which node sets validate).
