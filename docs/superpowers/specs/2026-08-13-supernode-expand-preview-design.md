# Supernode Expand/Collapse Preview — Design

**Date:** 2026-08-13
**Status:** Approved

## Summary

In the route policy editor, a supernode instance can be expanded in place to
show a zoomed-out, read-only preview of the subgraph it stands for, and folded
back to the ordinary collapsed node. The goal is glanceability: remember what
a supernode does without leaving the policy or opening the supernode editor.

## Decisions (from brainstorming)

- **Preview scope:** read-only glance. No pan/zoom inside the preview, no
  clicking inner nodes, no editing. Editing stays in the existing supernode
  library/editor.
- **Expansion model:** grow in place. The node enlarges in canvas space and
  floats above neighboring nodes/edges (raised z-index) without pushing them
  around. Its edges stay attached; it pans/zooms with the canvas.
- **Rendering approach:** nested read-only ReactFlow instance inside the
  expanded node (approach A), reusing the existing Policy→ReactFlow
  conversion for full visual fidelity. Rejected: hand-rolled SVG thumbnail
  (duplicated rendering logic, drift) and native subflow children (pollutes
  canvas state that round-trips into the saved policy).
- **Multiplicity / persistence:** multiple supernodes may be expanded at
  once; expansion state is per-canvas-session only — never persisted, never
  saved into the policy.

## UX

- Every node of type `supernode` in the **policy** view shows a small expand
  chevron in its header, next to the type name. (Supernode-definition
  canvases never contain supernode instances, so this is policy-view-only by
  construction.)
- Clicking the chevron grows the node in place to an expanded card
  (~480×320 canvas units) with a raised z-index and shadow, and swaps the
  chevron to a fold affordance. Clicking again restores the original size.
- The expanded card keeps its header, body (label / config_ref), and port
  rows in their current positions, and adds the preview panel **below**
  them — so the `in`/`success`/`error` handles barely move and edge geometry
  stays stable (@xyflow re-anchors edges off DOM layout).
- The preview panel renders the supernode's inner graph — including the
  `input`/`output`/`error` boundary pseudo-nodes — zoomed out to fit the
  panel, using the same node/edge visual language as the main canvas, with
  port-name labels hidden (noise at thumbnail scale).
- Nothing inside the preview is clickable, draggable, or connectable.
  Scroll/drag inside the preview is isolated from the outer canvas.
- Clicking the node's header/body still opens the normal inspector, exactly
  as today; only the chevron toggles expansion.

## Architecture

Three pieces:

### 1. Extract shared conversion helpers → `ui/src/policyGraph.ts`

`policyToNodes`, `policyToEdges`, and `splitEdge` move out of
`ui/src/components/GraphCanvas.tsx` into a new `ui/src/policyGraph.ts`
module; GraphCanvas imports them back. They already handle everything the
preview needs: `input` as the auto-layout entry node, saved `position`
values, and port-kind edge coloring via the catalog `PortSpecLookup`. A
`Supernode` has the same `nodes`/`edges` shape as a `Policy`, so the
helpers apply unchanged. This also trims the oversized GraphCanvas file.

### 2. New component `ui/src/components/SupernodePreview.tsx`

Props: the resolved `Supernode` definition and the `PortSpecLookup`.

- Converts the definition with the shared helpers and renders a nested
  `<ReactFlowProvider><ReactFlow …/></ReactFlowProvider>` with `fitView`.
- All interactivity flags off: `nodesDraggable={false}`,
  `nodesConnectable={false}`, `elementsSelectable={false}`,
  `panOnDrag={false}`, `zoomOnScroll={false}`, `zoomOnPinch={false}`,
  `zoomOnDoubleClick={false}`, no `MiniMap`/`Controls`.
- Container carries ReactFlow's `nowheel nopan nodrag` classes so events
  inside the preview don't pan/zoom/drag the outer canvas.
- Inner nodes render with the same `PluginNode` node type, with `onSelect`
  unset and `showPortNames: false`.

### 3. Wiring in `PluginNode` and `GraphCanvas`

`PluginNodeData` gains three optional fields:

- `supernodeDef?: Supernode` — the definition resolved from `config.name`
  (undefined when unresolved or not a supernode).
- `expanded?: boolean`
- `onToggleExpand?: (nodeId: string) => void`

`GraphCanvas`:

- Resolves `config.name` against its existing `supernodes` prop when
  building node data (`policyToNodes` gets the lookup threaded in).
- Owns expansion state as a `Set<string>` of node ids in component state.
  `nodesToPolicy` is untouched, so expansion can never leak into the saved
  policy.
- Sets a high `zIndex` on expanded nodes so they float above neighbors.

`PluginNode` renders the chevron only when `pluginType === 'supernode'`
(and, when expanded, the `SupernodePreview` panel or the error state).

## Edge cases and error handling

- **Stale or missing reference:** `config.name` matches no supernode in the
  library (deleted, renamed, or not yet configured). The chevron still
  shows; expanding reveals an inline message in the preview area
  ("supernode ‹name› not found" / "no supernode selected") instead of a
  graph. No crash, no blocked editing.
- **Definition refreshes:** node `data` is captured at conversion time, so
  when the `supernodes` prop refreshes, an effect rewrites `supernodeDef`
  on existing nodes — the same pattern the `showPortNames` effect already
  uses in GraphCanvas. An expanded preview reflects the latest fetched
  definition.
- **Nested supernodes:** if a definition somehow contains an inner node of
  type `supernode`, it renders as a normal collapsed node in the preview;
  no recursive expansion.
- **Deleting an expanded node:** its id is dropped from the expansion set
  along with the node.
- **Save round-trip:** untouched by construction; expansion state never
  enters node `data` fields that `nodesToPolicy` serializes.

## Testing

Pure logic modules get vitest unit tests (the established `connectionRules.test.ts`
pattern — the extracted `policyGraph.ts` is covered this way); UI behavior is
covered by the Playwright e2e suite.

- Extend `e2e/tests/supernodes.spec.ts` (+ a scenario row in
  `e2e/E2E_TESTBOOK.md`):
  - Create a supernode, place an instance in a policy, expand it; assert
    the inner node labels are visible in the preview and the outer canvas
    node count is unchanged.
  - Save the policy; assert the saved policy has no preview leakage (same
    nodes/edges as before expansion).
  - Fold; assert the preview is gone.
  - Stale-reference case: instance whose `config.name` doesn't resolve
    shows the inline error on expand.
- `cd ui && npm run build` and eslint stay green.

## Out of scope (YAGNI)

- Editing inside the expanded preview.
- Persisting expansion state (per browser or in the policy).
- Recursive expansion of nested supernodes.
- A command-palette action for expand/fold.
