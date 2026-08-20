# Supernode Named Error Ports + Boundary-Port Editing — Design

**Date:** 2026-08-20
**Status:** Approved for planning
**Extends:** `2026-08-20-supernode-named-output-ports-design.md` (shipped on `feature/supernode-named-ports`, PR #27)

## Problem

The named-output-ports feature made output boundaries plural and nameable,
but left two gaps:

1. **Unused boundaries can't be pruned.** Extraction derives one output
   boundary per exit edge; after rewiring inside the definition some
   boundaries end up unused — yet each still creates a mandatory instance
   port that every policy must wire. The inspector hides its Delete Node
   button for boundary types (`FIXED_TYPES` in NodeInspector), so the only
   way to remove one is the undiscoverable keyboard delete.
2. **The error boundary is hard-fixed**: exactly one, id must be `error`,
   no create/rename/delete. Output ports got full management; error ports
   got none.

## Goals

1. Error boundaries become plural, nameable, creatable, and deletable —
   symmetric with output boundaries — with at least one required.
2. Output and error boundaries are deletable from the inspector (never
   below one of each kind); `input` stays fixed and singular.
3. Extraction stops rejecting selections whose error exits point at
   different targets: each distinct target gets its own error port.

## Non-Goals

- Multiple input ports.
- Changing extraction's black-box semantics.
- New e2e scenarios (E2E-SN-05 is extended instead; the existing suite is
  the regression net).

## 1. Config model

A definition may declare **any number of `type: error` boundary nodes**,
mirroring output boundaries: the node id is the error-kind instance port
name. The boundary with id `error` is the **default error port** — the
black-box rule (inner nodes with unwired error ports implicitly exit the
instance) targets it and only it. If no `error`-id boundary exists (all
renamed), there is no implicit wiring: unhandled inner errors fall through
to the policy catch-all at request time.

Rules:

- Exactly one `input` boundary, id `input` (unchanged).
- One or more `type: output` boundaries (unchanged) and now **one or more
  `type: error` boundaries**.
- Reserved ids are per kind:
  - output boundary ids must not be `input`, `error`, `in`, `out`,
    `success` (unchanged; `output` is the success-mapped special id);
  - error boundary ids must not be `input`, `output`, `in`, `out`,
    `success` (`error` is the legal default id — no aliasing: an error
    boundary's port name is always its id).
- Node ids stay globally unique in the definition (existing duplicate-id
  check), so the instance port namespace can't collide across kinds.
- Wiring paradigm unchanged: output-derived ports mandatory, **all
  error-kind ports optional**.
- Backward compatibility: every existing definition has exactly one error
  boundary with id `error` → identical behavior, no migration.

```yaml
supernodes:
  - name: auth-gate
    nodes:
      - { id: input,      type: input }
      - { id: output,     type: output }   # -> instance port `success`
      - { id: denied,     type: output }   # -> outcome port `denied`
      - { id: error,      type: error }    # -> error port `error` (default/black-box)
      - { id: auth-error, type: error }    # -> error port `auth-error`
      - { id: auth,       type: key-auth }
    edges:
      - { from: input.out,    to: auth.in }
      - { from: auth.success, to: output.in }
      - { from: auth.denied,  to: denied.in }
      - { from: auth.error,   to: auth-error.in }
```

## 2. Validation (`src/graph/validation.rs`)

- The `for ty in ["input", "error"]` exactly-one loop shrinks to `input`
  only. Error boundaries get the output treatment: one or more, plus a new
  `RESERVED_ERROR_IDS: ["input", "output", "in", "out", "success"]` check
  and an "at least one 'error' boundary node" error when none exist.
- Everything else already generalizes: duplicate-id check, per-boundary
  no-outgoing-edges rule (`exit_boundary_ids` is type-based), orphan
  exemption for boundaries.

## 3. Expansion (`src/graph/expand.rs`)

- **Outer-port lookup becomes id-based across both exit kinds**: port `p`
  (after `out`→`success` normalization) resolves to the `output`-id output
  boundary when `p == "success"`, otherwise to the output-or-error-typed
  boundary whose id equals `p`. The `error_node_id` by-type lookup is
  removed — an error boundary's id IS its port.
- **Black-box rule** reads `exit_to.get("error")`: only the `error`-id
  boundary can be wired under that key (reserved lists keep other kinds
  out), so no separate tracking field is needed. No `error`-id boundary →
  no implicit wiring.
- Unknown-port errors list all exposed ports: output-derived ports plus
  every error boundary id.
- Unwired error-kind boundaries keep dropping their exit edges
  (`resolve_target`'s `Ok(None)` path and the inner-splice drop comment
  update from "the error boundary" to "an error-kind boundary").
- Mandatory wiring, duplicate-edge rejection, pass-through resolution, and
  cycle detection are unchanged in mechanism (pass-through via a named
  error boundary already works through `exit_to`).

## 4. UI

- **`supernodePortSpec`** (ui/src/policyGraph.ts): replace the hardcoded
  trailing `error` port with one error-kind port per `type: error`
  boundary, in definition order (name = id, kind `error`). Ordering:
  `success` (if `output`-id exists), named outcome ports, then error
  ports.
- **`validatePortName`** (ui/src/portNameValidation.ts): gains a
  `kind: 'output' | 'error'` parameter selecting the per-kind reserved
  list; signature `validatePortName(name, takenIds, kind, selfId?)`.
- **Palette** (PluginDrawer): the `onAddOutputPort` prop generalizes to
  `onAddBoundaryPort(kind: 'output' | 'error')`; the Boundary section
  shows two rows — "Output port" and "Error port" (error row uses
  `getPluginMeta('error')` for its icon/color).
- **Name dialog** (GraphCanvas): `portDialog` state carries the kind for
  both add and rename; the add branch creates `{ id, type: <kind> }`.
- **Rename** (NodeInspector): the Rename button's gate extends from
  `pluginType === 'output'` to output **or** `error`. Renaming `error`
  away removes the default black-box exit — allowed, same spirit as
  renaming `output`.
- **Delete** (NodeInspector + GraphCanvas): the Delete Node button appears
  for `output`/`error` boundaries in supernode mode, but **disabled with
  an explanatory title when it is the last boundary of its kind** (the
  server's ≥1 validation stays the authority; keyboard delete can still
  bypass and the save then fails with the clear server message). New
  inspector prop `boundaryDeleteBlocked?: string` (undefined = deletable;
  a string = disabled, string is the tooltip), computed by GraphCanvas
  from the canvas nodes. `input`, `listener`, `client` stay undeletable
  (no button).

## 5. Extraction (`ui/src/extractSupernode.ts`)

Error exits are grouped **by outer target, in edge order**: the first
group maps to the default `error` boundary; each subsequent distinct
target gets its own error boundary named `error-2`, `error-3`, …
(via the existing `uniquify`, renamable afterwards). The rewritten policy
wires `inst.error`, `inst.error-2`, … to their targets. The "all error
exits must share one target" rejection is deleted. All derived error-kind
ports are wired by construction; the optional-wiring rule only matters for
ports users add later.

## 6. Testing

- **validation.rs**: plural error boundaries accepted; renamed-only error
  boundary accepted (no `error` id); zero error boundaries rejected;
  reserved error ids rejected; `input` singularity untouched.
- **expand.rs**: named error port routes to its own outer target while
  `error` routes to the default; black-box only follows the `error`-id
  boundary (and is absent when that boundary was renamed away); unknown
  port `error` when no `error`-id boundary exists (message lists the named
  error ports); unwired named error port drops its exit edges;
  backward-compat single-`error` definitions unchanged.
- **UI vitest**: `supernodePortSpec` multi-error derivation and ordering;
  `validatePortName` per-kind reserved lists; extraction grouping
  (two targets → `error` + `error-2`, same-target collapse unchanged,
  rejection removed).
- **e2e**: extend E2E-SN-05's definition with an extra error boundary
  (e.g. `oops`) left unwired in the policy — asserting the save succeeds
  (error-kind ports are optional) while the rest of the scenario is
  unchanged. Full suite re-run as regression.

## 7. Documentation

- `website/docs/concepts/supernodes.md` — error boundaries plural/named,
  the `error`-id default and its black-box role, per-kind reserved ids,
  at-least-one-of-each rule.
- `website/docs/guides/web-ui.md` — Error port palette entry, rename,
  boundary deletion (and the last-of-kind guard), extraction's
  per-target error ports (rejection removed).
- `CLAUDE.md` — one-clause updates where supernode ports are described.

## Compatibility

Existing definitions (single `error`-id boundary) validate, expand, and
render identically. Instances keep exposing the same `error` port. The
only removed behavior is the extraction rejection for multi-target error
exits, which becomes a success path.
