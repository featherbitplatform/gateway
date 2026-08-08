# Port Row Labels in the Node Editor — Design

**Date:** 2026-08-08
**Status:** Approved (Francesco, 2026-08-08)
**Motivation:** Port names on canvas nodes are only discoverable via hover
tooltips (labels currently render only for nodes with >2 outputs, as floating
text outside the node edge). Port names should be always-visible.

## Decision

Render ports as **rows inside the node body** (standard node-editor pattern),
replacing the floating external labels and the `>2 outputs` threshold
entirely.

## Rendering

In `ui/src/components/PluginNode.tsx`:

- A ports section renders below the body (and `configRef` line, when
  present): one row per declared output from `resolveOutputs`, in
  registry order.
- Each row: `position: relative`, fixed row height (~18px), port name
  right-aligned in `--text-2xs` mono `--text-muted`; the row's `Handle`
  (type `source`, `Position.Right`) sits inside the row at `top: 50%`.
  @xyflow/react anchors edges from DOM layout, so handle alignment follows
  the row automatically regardless of header/body height.
- The `in` label renders left-aligned on the first row; the input `Handle`
  (type `target`, `Position.Left`) aligns to that row. Terminal-like nodes
  (`client`, and `output`/`error` boundary pseudo-nodes) render only the
  `in` row. Entry-like nodes (`listener`, `input` pseudo-node) render their
  single `success` row and no `in`.
- Unchanged: handle ids (= port names), `title` tooltips (name — description),
  kind colors (success `--success`, outcome `--accent`, error `--error`),
  edge serialization, and the shared `nodeKinds.ts` classification.

## Knock-ons

- Nodes grow ~18px per port. Verify the auto-layout spacing for positionless
  policies (`GraphCanvas` layout constants) still avoids vertical overlap
  with 3–4-output nodes; adjust spacing if needed.
- `E2E-UI-15`'s edge-click geometry workaround (bounding-box midpoint) is
  layout-dependent by its own admission — re-run and adjust if the taller
  nodes move the edge path.
- Regenerate the website screenshots (`website/screenshots/capture.mjs`)
  and remove the "(This capture predates the outcome ports…)" caption
  disclaimers in `website/docs/concepts/error-handling.md` and
  `website/docs/concepts/policies-and-graphs.md` — the new captures will
  show the labeled outcome-port rows, closing the follow-up noted on PR #13.

## Testing

- `cd ui && npm run build` (typecheck + build), then `cargo build` to embed.
- Full e2e suite (`cargo build --release && cd e2e && npm test`) — the UI
  specs (E2E-UI-15, E2E-SN-02, editor-roundtrip) exercise the editor against
  the real binary; handle-id/title assertions must pass unchanged.
- Fresh screenshots double as the visual evidence.

## Non-goals

- No changes to port semantics, catalog API, engine, or edge format.
- No per-port label styling beyond the muted name (kind color stays on the
  handle dot).
- No input-port renaming or multi-input support.
