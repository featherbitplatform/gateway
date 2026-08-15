# Supernode Expand/Collapse Preview Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** In the route policy editor, a supernode instance expands in place to a read-only, zoomed-out preview of its inner graph, and folds back to the collapsed node.

**Architecture:** The Policy→ReactFlow conversion helpers move out of `GraphCanvas.tsx` into a shared `policyGraph.ts` module. A new `SupernodePreview` component renders a nested, fully inert ReactFlow instance from a `Supernode` definition (same `nodes`/`edges` shape as a `Policy`). `PluginNode` grows a header chevron and hosts the preview panel; `GraphCanvas` owns expansion state as a `Set<string>` and decorates supernode nodes with the resolved definition, so nothing ever leaks into the saved policy.

**Tech Stack:** React 19 + `@xyflow/react` 12 (ReactFlow), TypeScript, vitest (unit), Playwright (e2e), lucide-react icons.

**Spec:** `docs/superpowers/specs/2026-08-13-supernode-expand-preview-design.md`

## Global Constraints

- Work on branch `feature/supernode-expand-preview` (already created, stacked on `feature/named-output-ports` — the design depends on that branch's port-spec UI code).
- Conventional Commits (`feat:`, `fix:`, `docs:`, `refactor:`, `test:`); **no** Co-Authored-By trailer.
- After every task: `cd ui && npm run build && npm run lint && npm test` all green.
- The e2e suite runs the **release binary**, which embeds `ui/dist` as it finds it. Before any e2e run: `cd ui && npm run build`, then `cargo build --release` from the repo root, then `cd e2e && npx playwright test …`. (One-time e2e setup if missing: `cd e2e && npm install && npx playwright install chromium`.)
- Exact user-facing strings (copy verbatim, tests assert on them):
  - aria-labels: `Expand supernode preview` / `Collapse supernode preview`
  - test ids: `supernode-preview` (on the preview panel in PluginNode), `plugin-drawer` (on PluginDrawer's root)
  - not-found copy: `supernode '<name>' not found` (single quotes around the name) and `no supernode selected`
- Preview panel: 480×320 px; expanded node `zIndex: 1000` (collapsed: 0).
- JSDoc every new exported symbol, matching the module-doc-comment style of the surrounding files.
- After the final code task, run `graphify update .` to refresh the knowledge graph (AST-only, no API cost).

---

### Task 1: Extract `policyGraph.ts` with vitest coverage

**Files:**
- Create: `ui/src/policyGraph.ts`
- Create: `ui/src/policyGraph.test.ts`
- Modify: `ui/src/components/GraphCanvas.tsx` (delete moved code, import it back)
- Modify: `docs/superpowers/specs/2026-08-13-supernode-expand-preview-design.md` (testing-section correction)

**Interfaces:**
- Consumes: existing `GraphCanvas.tsx` internals (verbatim move).
- Produces (later tasks rely on these exact exports from `ui/src/policyGraph.ts`):
  - `export const PORT_STROKE: Record<PortDecl['kind'], string>`
  - `export function portKindFor(sourceType: string | undefined, port: string, portSpecs: PortSpecLookup): PortDecl['kind']`
  - `export function splitEdge(endpoint: string): [string, string]`
  - `export function policyToNodes(policy: Policy, onSelect: (id: string) => void, portSpecs: PortSpecLookup, showPortNames: boolean): Node[]`
  - `export function policyToEdges(policy: Policy, portSpecs: PortSpecLookup): Edge[]`

- [ ] **Step 1: Write the failing unit test**

Create `ui/src/policyGraph.test.ts`:

```ts
import { describe, expect, test } from 'vitest';

import { policyToEdges, policyToNodes, splitEdge } from './policyGraph';
import type { PortSpecLookup } from './portSpecs';
import type { Policy } from './types';

const noop = () => {};

/** Minimal catalog lookup: one type with a declared outcome port. */
const SPECS: PortSpecLookup = {
  cors: {
    input: 'in',
    outputs: [
      { name: 'success', kind: 'success', description: '' },
      { name: 'preflight', kind: 'outcome', description: '' },
      { name: 'error', kind: 'error', description: '' },
    ],
  },
};

/** input → cors, with cors's outcome and error ports both feeding a supernode instance. */
const policy = (): Policy => ({
  name: 'p',
  nodes: [
    { id: 'input', type: 'input', config: {} },
    { id: 'cors', type: 'cors', config: {} },
    { id: 'sn', type: 'supernode', config: { name: 'auth-check' }, position: { x: 40, y: 60 } },
  ],
  edges: [
    { from: 'input.out', to: 'cors.in' },
    { from: 'cors.preflight', to: 'sn.in' },
    { from: 'cors.error', to: 'sn.in' },
  ],
});

describe('splitEdge', () => {
  test('splits node and port on the last dot', () => {
    expect(splitEdge('up.success')).toEqual(['up', 'success']);
  });
  test('node ids containing dots stay intact', () => {
    expect(splitEdge('a.b.error')).toEqual(['a.b', 'error']);
  });
  test('no dot defaults to the out port', () => {
    expect(splitEdge('listener')).toEqual(['listener', 'out']);
  });
});

describe('policyToNodes', () => {
  test('labels a supernode instance with its definition name', () => {
    const nodes = policyToNodes(policy(), noop, SPECS, true);
    expect(nodes.find((n) => n.id === 'sn')?.data.label).toBe('⬡ auth-check');
  });
  test('saved positions win over auto-layout', () => {
    const nodes = policyToNodes(policy(), noop, SPECS, true);
    expect(nodes.find((n) => n.id === 'sn')?.position).toEqual({ x: 40, y: 60 });
  });
  test('auto-layout places the input entry node at the first column', () => {
    const nodes = policyToNodes(policy(), noop, SPECS, true);
    expect(nodes.find((n) => n.id === 'input')?.position).toEqual({ x: 0, y: 150 });
  });
});

describe('policyToEdges', () => {
  test("normalizes an 'out' source port to the success handle", () => {
    const edges = policyToEdges(policy(), SPECS);
    expect(edges[0].sourceHandle).toBe('success');
    expect(edges[0].style?.stroke).toBe('var(--success)');
  });
  test('colors a declared outcome-port edge with the accent stroke, not animated', () => {
    const edges = policyToEdges(policy(), SPECS);
    expect(edges[1].style?.stroke).toBe('var(--accent)');
    expect(edges[1].animated).toBe(false);
  });
  test('error-port edges animate with the error stroke', () => {
    const edges = policyToEdges(policy(), SPECS);
    expect(edges[2].animated).toBe(true);
    expect(edges[2].style?.stroke).toBe('var(--error)');
  });
});
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cd ui && npx vitest run src/policyGraph.test.ts`
Expected: FAIL — `Cannot find module './policyGraph'` (or equivalent resolve error).

- [ ] **Step 3: Create `ui/src/policyGraph.ts` by moving code out of GraphCanvas**

Move the following **verbatim** (bodies unchanged) from `ui/src/components/GraphCanvas.tsx` into the new module, adding `export` to each:

- `PORT_STROKE` (GraphCanvas.tsx lines 45–50)
- `portKindFor` (lines 52–77, keep its doc comment)
- `policyToNodes` (lines 136–219, keep its doc comment)
- `policyToEdges` (lines 221–263, keep its doc comment)
- `splitEdge` (lines 265–282, keep its doc comment)

New module header and imports (note the **type-only** PluginNodeData import — `policyGraph` will later sit under a `PluginNode → SupernodePreview → policyGraph` import chain, and a value import here would close a runtime cycle):

```ts
/**
 * Policy ⇄ ReactFlow conversion helpers shared by the policy editor canvas
 * (GraphCanvas) and the read-only supernode preview (SupernodePreview).
 *
 * A gateway {@link Policy} — and a {@link Supernode}, which shares the same
 * nodes/edges shape — converts to ReactFlow nodes/edges with `node_id.port`
 * endpoints split per src/graph/engine.rs::parse_edge_endpoint, saved
 * positions honored, auto-layout for never-saved graphs, and edge styling
 * driven by each source port's declared kind.
 *
 * @module policyGraph
 */
import { MarkerType, type Edge, type Node } from '@xyflow/react';

import type { PluginNodeData } from './components/PluginNode';
import { resolveOutputs } from './nodeKinds';
import type { PortSpecLookup } from './portSpecs';
import type { PortDecl, Policy } from './types';
```

- [ ] **Step 4: Update GraphCanvas to import the moved helpers**

In `ui/src/components/GraphCanvas.tsx`:
- Delete the five moved declarations.
- Add: `import { PORT_STROKE, policyToEdges, policyToNodes, portKindFor } from '../policyGraph';`
- Keep `MarkerType` (still used by `onConnect`) and `resolveOutputs` (still used by `findUnwiredPorts`). Remove any import the build then flags as unused (e.g. `PortDecl` if nothing else references it).
- Everything else (`findUnwiredPorts`, `nodesToPolicy`, all handlers) stays put.

- [ ] **Step 5: Run the unit tests to verify they pass**

Run: `cd ui && npm test`
Expected: PASS — new `policyGraph.test.ts` suites plus the existing `connectionRules.test.ts`.

- [ ] **Step 6: Verify build and lint**

Run: `cd ui && npm run build && npm run lint`
Expected: both exit 0.

- [ ] **Step 7: Correct the spec's testing note**

In `docs/superpowers/specs/2026-08-13-supernode-expand-preview-design.md`, replace the sentence:

> The UI has no unit-test harness; the established pattern is the Playwright e2e suite.

with:

> Pure logic modules get vitest unit tests (the established `connectionRules.test.ts` pattern — the extracted `policyGraph.ts` is covered this way); UI behavior is covered by the Playwright e2e suite.

- [ ] **Step 8: Commit**

```bash
git add ui/src/policyGraph.ts ui/src/policyGraph.test.ts ui/src/components/GraphCanvas.tsx
git commit -m "refactor(ui): extract policy<->ReactFlow conversion into policyGraph.ts"
git add docs/superpowers/specs/2026-08-13-supernode-expand-preview-design.md
git commit -m "docs: correct spec testing note (vitest harness exists)"
```

---

### Task 2: `SupernodePreview` component

**Files:**
- Create: `ui/src/components/SupernodePreview.tsx`

**Interfaces:**
- Consumes (from Task 1): `policyToNodes`, `policyToEdges` from `../policyGraph`.
- Produces (Task 3 relies on this exact contract):
  - `export function SupernodePreview({ name, supernode, portSpecs }: SupernodePreviewProps)` with `SupernodePreviewProps = { name?: string; supernode?: Supernode; portSpecs: PortSpecLookup }`
  - `supernode === undefined` renders the inline not-found state using the exact copy from Global Constraints.

There is no DOM-level unit harness (vitest runs node-env logic tests only); this component's behavior is verified by Task 3's e2e tests. This task's gate is compile + lint.

- [ ] **Step 1: Write the component**

Create `ui/src/components/SupernodePreview.tsx`:

```tsx
/**
 * Read-only, zoomed-out rendering of a supernode's inner graph, hosted
 * inside an expanded supernode card on the policy canvas (see PluginNode).
 *
 * A second, miniature ReactFlow instance (its own provider) fed by the same
 * Policy→ReactFlow conversion as the main canvas — a {@link Supernode}
 * shares the Policy nodes/edges shape, and the shared auto-layout already
 * treats the `input` boundary pseudo-node as the entry. Every interaction
 * flag is off; the host panel in PluginNode carries the `nowheel nopan
 * nodrag` classes that keep preview events from panning or zooming the
 * outer canvas.
 *
 * @module components/SupernodePreview
 */
import { useMemo } from 'react';
import { Background, ReactFlow, ReactFlowProvider } from '@xyflow/react';

import { policyToEdges, policyToNodes } from '../policyGraph';
import type { PortSpecLookup } from '../portSpecs';
import type { Supernode } from '../types';
import { PluginNode } from './PluginNode';

/**
 * Nested-instance node registry; inner nodes reuse the main canvas renderer.
 * PluginNode ↔ SupernodePreview is a module cycle (the preview renders the
 * same node component that hosts it); PluginNode is a hoisted function
 * declaration, so referencing it at module scope here is initialization-safe.
 */
const nodeTypes = { pluginNode: PluginNode };

/** Inner nodes are inert; the conversion still requires a select callback. */
const noSelect = () => {};

/** Props for {@link SupernodePreview}. */
interface SupernodePreviewProps {
  /** Referenced definition name (the instance's `config.name`), for the not-found message. */
  name?: string;
  /** Resolved definition; undefined renders the inline not-found state instead of a graph. */
  supernode?: Supernode;
  /** Catalog port-spec lookup driving the inner nodes' handles and edge colors. */
  portSpecs: PortSpecLookup;
}

/**
 * Renders the inner graph zoomed out to fit the host panel, or an inline
 * error when the reference does not resolve (stale/missing definition —
 * the editor keeps working; the server rejects such a save anyway).
 */
export function SupernodePreview({ name, supernode, portSpecs }: SupernodePreviewProps) {
  const graph = useMemo(() => {
    if (!supernode) return null;
    const asPolicy = { name: supernode.name, nodes: supernode.nodes, edges: supernode.edges };
    return {
      // Port labels off: at thumbnail scale they are noise. Individual
      // interaction flags off on every node, belt-and-braces alongside the
      // instance-wide flags below.
      nodes: policyToNodes(asPolicy, noSelect, portSpecs, false).map((n) => ({
        ...n,
        draggable: false,
        connectable: false,
        selectable: false,
      })),
      edges: policyToEdges(asPolicy, portSpecs),
    };
  }, [supernode, portSpecs]);

  if (!graph) {
    return (
      <div
        className="flex items-center justify-center h-full"
        style={{
          fontFamily: 'var(--font-mono)',
          fontSize: 'var(--text-xs)',
          color: 'var(--text-muted)',
          padding: 12,
          textAlign: 'center',
        }}
      >
        {name ? `supernode '${name}' not found` : 'no supernode selected'}
      </div>
    );
  }

  return (
    <ReactFlowProvider>
      <ReactFlow
        nodes={graph.nodes}
        edges={graph.edges}
        nodeTypes={nodeTypes}
        fitView
        fitViewOptions={{ padding: 0.15 }}
        minZoom={0.05}
        nodesDraggable={false}
        nodesConnectable={false}
        nodesFocusable={false}
        edgesFocusable={false}
        elementsSelectable={false}
        panOnDrag={false}
        panOnScroll={false}
        zoomOnScroll={false}
        zoomOnPinch={false}
        zoomOnDoubleClick={false}
        preventScrolling={false}
      >
        <Background gap={20} size={1} color="var(--grid-dot)" />
      </ReactFlow>
    </ReactFlowProvider>
  );
}
```

- [ ] **Step 2: Verify build, lint, and unit tests**

Run: `cd ui && npm run build && npm run lint && npm test`
Expected: all exit 0 (the component is not imported anywhere yet; exported symbols are exempt from unused-code lints).

- [ ] **Step 3: Commit**

```bash
git add ui/src/components/SupernodePreview.tsx
git commit -m "feat(ui): read-only nested-flow preview component for supernodes"
```

---

### Task 3: Wire expand/fold into PluginNode + GraphCanvas, driven by e2e

**Files:**
- Modify: `ui/src/components/PluginNode.tsx`
- Modify: `ui/src/components/GraphCanvas.tsx`
- Modify: `ui/src/components/PluginDrawer.tsx` (one-line testid)
- Test: `e2e/tests/supernodes.spec.ts`
- Modify: `e2e/E2E_TESTBOOK.md`

**Interfaces:**
- Consumes (Task 2): `SupernodePreview` from `./SupernodePreview`.
- Produces: `PluginNodeData` gains `supernodeDef?: Supernode`, `portSpecs?: PortSpecLookup`, `expanded?: boolean`, `onToggleExpand?: (nodeId: string) => void`.

- [ ] **Step 1: Write the failing e2e tests**

Append inside the `test.describe('Supernodes', …)` block of `e2e/tests/supernodes.spec.ts`:

```ts
test('E2E-SN-03: expand and fold a supernode preview on the policy canvas', async ({page}) => {
  const api = await adminApi();
  await deleteRouteIfPresent(api, 'sn-preview-route');
  await api.delete('/api/policies/sn-preview-policy');
  await api.delete('/api/supernodes/e2e-preview-sn');

  const sn = {...(await echoSupernode(api)), name: 'e2e-preview-sn'};
  expect((await api.put('/api/supernodes/e2e-preview-sn', {data: sn})).ok()).toBeTruthy();
  const policy = {
    name: 'sn-preview-policy',
    nodes: [
      {id: 'listener', type: 'listener', config: {}},
      {id: 'sec', type: 'supernode', config: {name: 'e2e-preview-sn'}},
      {id: 'client', type: 'client', config: {}},
    ],
    edges: [
      {from: 'listener.out', to: 'sec.in'},
      {from: 'sec.success', to: 'client.in'},
    ],
  };
  expect((await api.put('/api/policies/sn-preview-policy', {data: policy})).ok()).toBeTruthy();
  expect(
    (
      await api.post('/api/routes', {
        data: {name: 'sn-preview-route', match: {path: '/sn-preview/*', methods: ['GET']}, policy: 'sn-preview-policy'},
      })
    ).ok(),
  ).toBeTruthy();

  await page.goto('/');
  await page.getByText('sn-preview-route', {exact: true}).click();
  await page.waitForSelector('.react-flow__node');

  // The outer canvas's own nodes: direct children of the FIRST nodes
  // container in DOM order — the nested preview instance adds its own,
  // later container, so this locator stays outer-only after expansion.
  const outerNodes = page.locator('.react-flow__nodes').first().locator('> .react-flow__node');
  await expect(outerNodes).toHaveCount(3);

  // Expand: the preview appears with the definition's inner graph.
  // ('input'/'output' appear twice inside a preview node — type header +
  // id body — so .first() disambiguates; 'up' has a distinct header.)
  await page.getByRole('button', {name: 'Expand supernode preview'}).click();
  const preview = page.getByTestId('supernode-preview');
  await expect(preview).toBeVisible();
  await expect(preview.getByText('input', {exact: true}).first()).toBeVisible();
  await expect(preview.getByText('up', {exact: true})).toBeVisible();
  await expect(preview.getByText('output', {exact: true}).first()).toBeVisible();

  // The outer canvas gained nothing: expansion is render-only.
  await expect(outerNodes).toHaveCount(3);

  // Saving while expanded round-trips the policy unchanged.
  await page.getByRole('button', {name: 'Save Policy'}).click();
  await expect(page.getByText('Policy saved')).toBeVisible();
  const saved = (await (await api.get('/api/policies/sn-preview-policy')).json()) as {
    nodes: {id: string; type: string; config: Record<string, unknown>}[];
    edges: {from: string; to: string}[];
  };
  expect(saved.nodes.map((n) => n.id).sort()).toEqual(['client', 'listener', 'sec']);
  expect(saved.nodes.find((n) => n.id === 'sec')!.config).toEqual({name: 'e2e-preview-sn'});
  expect(saved.edges).toHaveLength(2);

  // Fold restores the collapsed card.
  await page.getByRole('button', {name: 'Collapse supernode preview'}).click();
  await expect(preview).toHaveCount(0);

  await api.delete('/api/routes/sn-preview-route');
  await api.delete('/api/policies/sn-preview-policy');
  await api.delete('/api/supernodes/e2e-preview-sn');
  await api.dispose();
});

test('E2E-SN-04: deleting the definition flips an expanded preview to not-found', async ({page}) => {
  const api = await adminApi();
  await deleteRouteIfPresent(api, 'sn-orphan-route');
  await api.delete('/api/policies/sn-orphan-policy');
  await api.delete('/api/supernodes/e2e-preview-orphan');

  const sn = {...(await echoSupernode(api)), name: 'e2e-preview-orphan'};
  expect((await api.put('/api/supernodes/e2e-preview-orphan', {data: sn})).ok()).toBeTruthy();

  // A plain policy (no supernode) opens the canvas; the instance is added
  // in-editor and never saved — the only way a stale reference can arise,
  // since delete protection rejects deleting a referenced definition.
  const policy = {
    name: 'sn-orphan-policy',
    nodes: [
      {id: 'listener', type: 'listener', config: {}},
      {id: 'client', type: 'client', config: {}},
    ],
    edges: [{from: 'listener.out', to: 'client.in'}],
  };
  expect((await api.put('/api/policies/sn-orphan-policy', {data: policy})).ok()).toBeTruthy();
  expect(
    (
      await api.post('/api/routes', {
        data: {name: 'sn-orphan-route', match: {path: '/sn-orphan/*', methods: ['GET']}, policy: 'sn-orphan-policy'},
      })
    ).ok(),
  ).toBeTruthy();

  await page.goto('/');
  await page.getByText('sn-orphan-route', {exact: true}).click();
  await page.waitForSelector('.react-flow__node');

  // Add the supernode instance from the drawer (unsaved). The testid scope
  // matters: the sidebar library lists the same name.
  await page.getByRole('button', {name: 'Add Node'}).click();
  await page.getByTestId('plugin-drawer').getByText('e2e-preview-orphan', {exact: true}).click();

  // Expand: the definition renders.
  await page.getByRole('button', {name: 'Expand supernode preview'}).click();
  const preview = page.getByTestId('supernode-preview');
  await expect(preview.getByText('up', {exact: true})).toBeVisible();

  // Close the inspector (it also shows the definition name, which would
  // make the sidebar-row text ambiguous) by clicking empty pane corner.
  await page.locator('.react-flow__pane').first().click({position: {x: 5, y: 5}});

  // Delete the definition from the sidebar library (hover reveals the X),
  // confirming in the dialog.
  await page.getByText('e2e-preview-orphan', {exact: true}).hover();
  await page.getByRole('button', {name: 'Delete supernode e2e-preview-orphan'}).click();
  await page
    .getByRole('dialog', {name: 'Delete supernode'})
    .getByRole('button', {name: 'Delete', exact: true})
    .click();

  // The library refetch rewrites the node's resolved definition; the open
  // preview flips to the inline not-found state.
  await expect(preview.getByText("supernode 'e2e-preview-orphan' not found")).toBeVisible();

  await api.delete('/api/routes/sn-orphan-route');
  await api.delete('/api/policies/sn-orphan-policy');
  await api.dispose();
});
```

- [ ] **Step 2: Run the new e2e tests to verify they fail**

```bash
cd ui && npm run build
cd .. && cargo build --release
cd e2e && npx playwright test tests/supernodes.spec.ts
```

Expected: E2E-SN-01/02 PASS; E2E-SN-03 and E2E-SN-04 FAIL on `getByRole('button', {name: 'Expand supernode preview'})` — the chevron does not exist yet.

- [ ] **Step 3: Extend `PluginNodeData` and render chevron + preview in PluginNode**

In `ui/src/components/PluginNode.tsx`:

Add imports:

```ts
import { ChevronDown, ChevronUp, Link2 } from 'lucide-react';   // replaces the bare Link2 import
import { SupernodePreview } from './SupernodePreview';
import type { PortSpecLookup } from '../portSpecs';
import type { PortDecl, PortSpec, Supernode } from '../types';  // replaces the existing types import
```

Add to the `PluginNodeData` interface (after `showPortNames`):

```ts
  /** Resolved supernode definition for `supernode` nodes; undefined = unresolved (stale/missing reference). */
  supernodeDef?: Supernode;
  /** Catalog port-spec lookup, threaded to the expanded preview's inner nodes (supernode nodes only). */
  portSpecs?: PortSpecLookup;
  /** Whether this supernode instance is expanded to its inline preview. */
  expanded?: boolean;
  /** Called with the node id when the expand/fold chevron is clicked (supernode nodes only). */
  onToggleExpand?: (nodeId: string) => void;
```

In the component body (after the `showNames` const):

```ts
  const isSupernode = nodeData.pluginType === 'supernode';
```

In the header div, after the type-name `<span>`:

```tsx
        {isSupernode && (
          <button
            onClick={(e) => {
              e.stopPropagation();
              nodeData.onToggleExpand?.(id);
            }}
            aria-label={nodeData.expanded ? 'Collapse supernode preview' : 'Expand supernode preview'}
            title={nodeData.expanded ? 'Fold preview' : 'Preview contents'}
            className="flex items-center justify-center"
            style={{ marginLeft: 'auto', width: 18, height: 18, color: '#fff', opacity: 0.9, background: 'transparent' }}
          >
            {nodeData.expanded ? <ChevronUp size={13} /> : <ChevronDown size={13} />}
          </button>
        )}
```

At the bottom of the card, immediately after the ports block (the `showNames ? … : …` expression) and before the closing outer `</div>`:

```tsx
      {/* Expanded supernode preview. Sits BELOW the port rows so the
          in/success/error handles barely move on expand — @xyflow re-anchors
          edges off DOM layout either way. nowheel/nopan/nodrag isolate
          preview events from the outer canvas; stopPropagation keeps a
          preview click from opening the inspector. */}
      {isSupernode && nodeData.expanded && (
        <div
          data-testid="supernode-preview"
          className="nowheel nopan nodrag"
          onClick={(e) => e.stopPropagation()}
          style={{
            width: 480,
            height: 320,
            borderTop: '1px solid var(--border)',
            borderRadius: '0 0 6px 6px',
            overflow: 'hidden',
            background: 'var(--bg-canvas)',
          }}
        >
          <SupernodePreview
            name={typeof nodeData.config?.name === 'string' ? nodeData.config.name : undefined}
            supernode={nodeData.supernodeDef}
            portSpecs={nodeData.portSpecs ?? {}}
          />
        </div>
      )}
```

- [ ] **Step 4: Wire expansion state and supernode decoration in GraphCanvas**

In `ui/src/components/GraphCanvas.tsx`:

After the `drawerOpen` state declaration:

```ts
  // Ids of supernode instances currently expanded to their inline preview.
  // Canvas-session-only by design: held here (not in anything nodesToPolicy
  // serializes), so expansion can never leak into the saved policy.
  const [expandedSupernodes, setExpandedSupernodes] = useState<Set<string>>(new Set());
  const handleToggleExpand = useCallback((nodeId: string) => {
    setExpandedSupernodes((prev) => {
      const next = new Set(prev);
      if (next.has(nodeId)) next.delete(nodeId);
      else next.add(nodeId);
      return next;
    });
  }, []);
```

After the existing `showPortNames` rewrite effect:

```ts
  // Supernode instances carry extra render-time data: the resolved
  // definition (kept fresh when the library refetches — same rewrite
  // pattern as showPortNames above), the port-spec lookup for the preview's
  // inner nodes, and the expand/fold state. zIndex floats an expanded card
  // above its neighbors.
  useEffect(() => {
    setNodes((nds) =>
      nds.map((n) => {
        const data = n.data as unknown as PluginNodeData;
        if (data.pluginType !== 'supernode') return n;
        const refName = typeof data.config?.name === 'string' ? data.config.name : undefined;
        const expanded = expandedSupernodes.has(n.id);
        return {
          ...n,
          zIndex: expanded ? 1000 : 0,
          data: {
            ...data,
            supernodeDef: refName ? supernodes.find((s) => s.name === refName) : undefined,
            portSpecs,
            expanded,
            onToggleExpand: handleToggleExpand,
          },
        };
      })
    );
  }, [supernodes, portSpecs, expandedSupernodes, handleToggleExpand, setNodes]);
```

In `handleAddSupernode`, extend the new node's `data` (the decoration effect above only fires on its deps, not on node adds):

```ts
      data: {
        label: `⬡ ${sn.name}`,
        pluginType: 'supernode',
        config: { name: sn.name },
        // 'supernode' has no catalog entry (it's not a src/plugins/mod.rs
        // type); portSpecs lookup misses and PluginNode falls back to the
        // default success+error pair, matching a supernode instance's fixed
        // output/error boundary exits (src/graph/expand.rs).
        ports: portSpecs['supernode'],
        onSelect: handleSelect,
        showPortNames,
        supernodeDef: sn,
        portSpecs,
        expanded: false,
        onToggleExpand: handleToggleExpand,
      } satisfies PluginNodeData,
```

In `handleDeleteNode`, drop the id from the expansion set (harmless for non-supernodes):

```ts
  const handleDeleteNode = (nodeId: string) => {
    setNodes((nds) => nds.filter((n) => n.id !== nodeId));
    setEdges((eds) => eds.filter((e) => e.source !== nodeId && e.target !== nodeId));
    setExpandedSupernodes((prev) => {
      if (!prev.has(nodeId)) return prev;
      const next = new Set(prev);
      next.delete(nodeId);
      return next;
    });
    setSelectedNodeId(null);
  };
```

- [ ] **Step 5: Add the drawer testid**

In `ui/src/components/PluginDrawer.tsx`, add `data-testid="plugin-drawer"` to the outermost element returned by the `PluginDrawer` component (the drawer panel div rendered when `isOpen`).

- [ ] **Step 6: Verify build, lint, unit tests**

Run: `cd ui && npm run build && npm run lint && npm test`
Expected: all exit 0.

- [ ] **Step 7: Rebuild and run the e2e suite to verify the new tests pass**

```bash
cd ui && npm run build
cd .. && cargo build --release
cd e2e && npx playwright test tests/supernodes.spec.ts
```

Expected: all four supernode scenarios PASS (SN-01 … SN-04).

- [ ] **Step 8: Add the testbook rows**

In `e2e/E2E_TESTBOOK.md`, append to the Supernodes table (after the E2E-SN-02 row):

```markdown
| E2E-SN-03 | Expand a supernode instance on the policy canvas, save while expanded, then fold | The in-place preview renders the definition's inner graph (`input`/`up`/`output` visible); the outer canvas keeps exactly its own 3 nodes (expansion is render-only); the saved policy round-trips unchanged — same node ids, `config: {name}` untouched, 2 edges — proving expansion never persists; folding removes the preview |
| E2E-SN-04 | Add an (unsaved) supernode instance, expand it, then delete the definition from the sidebar library | The preview first renders the definition, then flips to the inline `supernode '<name>' not found` state once the library refetch resolves the reference to nothing — the editor keeps working with a stale reference instead of crashing |
```

- [ ] **Step 9: Refresh the knowledge graph**

Run: `graphify update .` (from the repo root).

- [ ] **Step 10: Commit**

```bash
git add ui/src/components/PluginNode.tsx ui/src/components/GraphCanvas.tsx ui/src/components/PluginDrawer.tsx e2e/tests/supernodes.spec.ts e2e/E2E_TESTBOOK.md
git commit -m "feat(ui): expand a supernode in place to a read-only preview of its subgraph"
```

(If `graphify update` changed tracked files under `graphify-out/`, commit them separately as `chore: refresh knowledge graph`.)

---

## Verification checklist (after all tasks)

- `cd ui && npm run build && npm run lint && npm test` — green.
- `cargo build --release` then `cd e2e && npm test` — full suite green (the feature must not regress other UI specs; the palette/debug specs share the canvas).
- Manual smoke (optional): `docker compose up`, open the admin UI, expand/fold a supernode on a policy, confirm scroll inside the preview does not zoom the outer canvas.
