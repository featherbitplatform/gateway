/**
 * Supernode scenarios. See E2E_TESTBOOK.md ("Supernodes").
 */
import {test, expect} from '@playwright/test';

import {adminApi, dataPlane, deleteRouteIfPresent, waitForDataPlane} from '../helpers/admin';

/** Header that opts a single request into debug tracing (see debug.spec.ts). */
const DEBUG_HEADER = {'x-featherbit-debug': '1'};

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

    // Traced: the expanded node ids are namespaced `<instance>/<inner>` (compile-time
    // inlining), so a step for the supernode's inner upstream node must show up as
    // `sec/up`, not as a synthetic `supernode` node.
    const traced = await dp.get('/sn-e2e/hello', {headers: DEBUG_HEADER});
    expect(traced.status()).toBe(200);
    const traceId = traced.headers()['x-featherbit-trace-id'];
    expect(traceId, 'traced response should carry the trace id').toBeTruthy();

    const trace = (await (await api.get(`/api/debug/traces/${traceId}`)).json()) as {
      steps: {node_id: string}[];
    };
    expect(trace.steps.some((s) => s.node_id.startsWith('sec/'))).toBeTruthy();

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

  /**
   * Regression guard for a bug in the editor's client-side unwired-port
   * save warning (Task 12): `output`/`error` are the supernode boundary
   * pseudo-nodes and have no catalog entry, so a naive port-spec lookup
   * fell back to the default success+error pair and wrongly demanded an
   * outgoing edge from `output.success`/`error.success` -- ports those
   * terminal nodes never have and (per src/graph/validation.rs)
   * `validate_supernode` never lets you wire. Opening any valid, freshly
   * saved supernode and clicking Save must show no such warning.
   */
  test('E2E-SN-02: saving a fresh supernode in the editor shows no unwired-port warning', async ({page}) => {
    const api = await adminApi();
    await api.delete('/api/supernodes/e2e-editor-check');
    const sn = {...(await echoSupernode(api)), name: 'e2e-editor-check'};
    expect((await api.put('/api/supernodes/e2e-editor-check', {data: sn})).ok()).toBeTruthy();

    await page.goto('/');
    await page.getByText('e2e-editor-check', {exact: true}).click();
    await page.waitForSelector('.react-flow__node');
    await page.getByRole('button', {name: 'Save Supernode'}).click();

    // The success toast confirms the save actually completed; absence of the
    // warning text confirms no bogus unwired-port complaint was raised.
    await expect(page.getByText('Supernode saved')).toBeVisible();
    await expect(page.getByText('Unwired ports')).toHaveCount(0);

    await api.delete('/api/supernodes/e2e-editor-check');
    await api.dispose();
  });

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

    // Adding the node auto-opens the inspector, which overlaps the new
    // node's chevron at this canvas position; dismiss it via empty pane
    // corner before expanding (test-only timing fix, not a product step).
    await page.locator('.react-flow__pane').first().click({position: {x: 5, y: 5}});

    // The new node lands off the initial fitView's frame (only fitted once,
    // on mount, over the original 2 nodes) and can end up under the minimap
    // or attribution watermark; re-fit the view so its chevron is clickable
    // (test-only timing fix, not a product step).
    await page.getByRole('button', {name: 'Fit View'}).click();

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

  /**
   * Named output ports (Task 1-6): a supernode may declare more than one
   * `type: output` boundary. Boundary id `output` becomes instance port
   * `success`; any other boundary id (here `blocked`) becomes an
   * instance port of that same name, and -- like `success` -- it is
   * mandatory-wired: expansion must fail if a policy leaves it dangling,
   * and that failure must surface through the Admin API on policy save.
   */
  test('E2E-SN-05: named output ports route end-to-end, and unwired ports reject at save', async () => {
    const api = await adminApi();
    await deleteRouteIfPresent(api, 'gate-route');
    await api.delete('/api/policies/gate-policy');
    await api.delete('/api/supernodes/header-gate');

    // `cond` branches on header presence: `true` exits through the `output`
    // boundary (-> instance port `success`), `false` through the second
    // `type: output` boundary `blocked` (-> instance port `blocked`). A
    // definition must always declare an `error` boundary node, but -- unlike
    // `output`-derived ports -- it is never mandatory-wired, so it stays
    // unconnected here. A second `type: error` boundary, `oops`, is declared
    // with no inner edges at all, proving error-kind instance ports stay
    // optional-wiring even when there's more than one.
    const headerGate = {
      name: 'header-gate',
      nodes: [
        {id: 'input', type: 'input', config: {}},
        {id: 'output', type: 'output', config: {}},
        {id: 'blocked', type: 'output', config: {}},
        {id: 'error', type: 'error', config: {}},
        {id: 'oops', type: 'error', config: {}},
        {id: 'cond', type: 'condition', config: {conditions: [['http_x_e2e_gate', 'present']]}},
      ],
      edges: [
        {from: 'input.out', to: 'cond.in'},
        {from: 'cond.true', to: 'output.in'},
        {from: 'cond.false', to: 'blocked.in'},
      ],
    };
    expect((await api.put('/api/supernodes/header-gate', {data: headerGate})).ok()).toBeTruthy();

    // Reuse the seeded echo upstream's live config, same as echoSupernode() above.
    const echoPolicy = (await (await api.get('/api/policies/echo-policy')).json()) as {
      nodes: {id: string; type: string; config: Record<string, unknown>}[];
    };
    const upstreamConfig = echoPolicy.nodes.find((n) => n.type === 'upstream')!.config;

    const wiredPolicy = {
      name: 'gate-policy',
      nodes: [
        {id: 'listener', type: 'listener', config: {}},
        {id: 'gate', type: 'supernode', config: {name: 'header-gate'}},
        {id: 'up', type: 'upstream', config: upstreamConfig},
        {id: 'deny', type: 'response-rewrite', config: {status_code: 403, body: '{"error":"blocked"}'}},
        {id: 'client', type: 'client', config: {}},
      ],
      edges: [
        {from: 'listener.out', to: 'gate.in'},
        {from: 'gate.success', to: 'up.in'},
        {from: 'gate.blocked', to: 'deny.in'},
        {from: 'up.success', to: 'client.in'},
        {from: 'deny.success', to: 'client.in'},
      ],
    };
    expect((await api.put('/api/policies/gate-policy', {data: wiredPolicy})).ok()).toBeTruthy();
    expect(
      (
        await api.post('/api/routes', {
          data: {name: 'gate-route', match: {path: '/gate/*', methods: ['GET']}, policy: 'gate-policy'},
        })
      ).ok(),
    ).toBeTruthy();

    const dp = await dataPlane();

    // With the header: cond.true -> output -> gate.success -> the echo backend.
    const withHeader = await dp.get('/gate/ping', {headers: {'x-e2e-gate': '1'}});
    expect(withHeader.status()).toBe(200);
    const echo = (await withHeader.json()) as {path: string};
    expect(echo.path).toBe('/gate/ping');

    // Without the header: cond.false -> blocked -> gate.blocked -> deny's fixed 403.
    const withoutHeader = await dp.get('/gate/ping');
    expect(withoutHeader.status()).toBe(403);
    expect(await withoutHeader.text()).toContain('blocked');

    // Leaving `gate.blocked` unwired is a hard rejection at save time, naming
    // the instance and port (src/graph/expand.rs's mandatory-port check).
    const unwiredPolicy = {
      ...wiredPolicy,
      edges: wiredPolicy.edges.filter((e) => e.from !== 'gate.blocked'),
    };
    const rejected = await api.put('/api/policies/gate-policy', {data: unwiredPolicy});
    expect(rejected.ok()).toBeFalsy();
    expect(rejected.status()).toBeGreaterThanOrEqual(400);
    expect(rejected.status()).toBeLessThan(500);
    const body = await rejected.text();
    expect(body).toContain("output port 'blocked' of supernode instance");
    expect(body).toContain("add an edge from 'gate.blocked'");

    // Last-good guarantee: the rejected save never swapped the route table.
    const stillGood = await dp.get('/gate/ping', {headers: {'x-e2e-gate': '1'}});
    expect(stillGood.status()).toBe(200);

    await dp.dispose();
    await api.delete('/api/routes/gate-route');
    await api.delete('/api/policies/gate-policy');
    await api.delete('/api/supernodes/header-gate');
    await api.dispose();
  });

  /**
   * Editor extraction flow (Task 6): select two adjacent middle nodes on the
   * policy canvas, "Extract Supernode" from the toolbar, name it, confirm.
   * Multi-select here uses click + Ctrl-click rather than a box-select drag
   * -- @xyflow/react's default `multiSelectionKeyCode` is `Control` on
   * non-mac platforms (the suite runs on ubuntu-latest in CI), and a
   * click-based selection is far more reliable under Playwright than
   * reproducing a rubber-band drag over ~10px node hit areas.
   */
  test('E2E-SN-06: extract two selected nodes into a new supernode from the editor', async ({page}) => {
    const api = await adminApi();
    const snName = 'e2e-extracted-mid';
    await deleteRouteIfPresent(api, 'sn-extract-route');
    await api.delete('/api/policies/sn-extract-policy');
    await api.delete(`/api/supernodes/${snName}`);

    const echoPolicy = (await (await api.get('/api/policies/echo-policy')).json()) as {
      nodes: {id: string; type: string; config: Record<string, unknown>}[];
    };
    const upstreamConfig = echoPolicy.nodes.find((n) => n.type === 'upstream')!.config;

    // listener -> mid1 -> mid2 -> up -> client: mid1/mid2 are the two
    // adjacent, extraction-eligible middle nodes (no listener/client/
    // supernode in the selection).
    const policy = {
      name: 'sn-extract-policy',
      nodes: [
        {id: 'listener', type: 'listener', config: {}},
        {id: 'mid1', type: 'proxy-rewrite', config: {add_headers: {'x-mid1': '1'}}},
        {id: 'mid2', type: 'proxy-rewrite', config: {add_headers: {'x-mid2': '1'}}},
        {id: 'up', type: 'upstream', config: upstreamConfig},
        {id: 'client', type: 'client', config: {}},
      ],
      edges: [
        {from: 'listener.out', to: 'mid1.in'},
        {from: 'mid1.success', to: 'mid2.in'},
        {from: 'mid2.success', to: 'up.in'},
        {from: 'up.success', to: 'client.in'},
      ],
    };
    expect((await api.put('/api/policies/sn-extract-policy', {data: policy})).ok()).toBeTruthy();
    expect(
      (
        await api.post('/api/routes', {
          data: {
            name: 'sn-extract-route',
            match: {path: '/sn-extract/*', methods: ['GET']},
            policy: 'sn-extract-policy',
          },
        })
      ).ok(),
    ).toBeTruthy();

    await page.goto('/');
    await page.getByText('sn-extract-route', {exact: true}).click();
    await page.waitForSelector('.react-flow__node');

    // Select mid1, then Ctrl-click mid2 to add it to the selection. Each
    // click also opens the inspector (PluginNode's onSelect), which sits in
    // a right-hand panel overlapping the top-right toolbar and would
    // intercept the click below; close it via its own Close button, which
    // only clears the inspector's local selectedNodeId, not the canvas's
    // multi-node ReactFlow selection (unlike a pane click, which clears
    // both -- see the empty-pane click further down, used once the
    // selection no longer matters).
    await page.locator('.react-flow__node', {hasText: 'mid1'}).first().click();
    await page.locator('.react-flow__node', {hasText: 'mid2'}).first().click({modifiers: ['Control']});
    await page.getByRole('button', {name: 'Close', exact: true}).click();

    await page.getByRole('button', {name: 'Extract Supernode'}).click();
    const dialog = page.getByRole('dialog', {name: 'Extract selection as supernode'});
    await expect(dialog).toBeVisible();
    await dialog.locator('input').fill(snName);
    await dialog.getByRole('button', {name: 'Extract', exact: true}).click();

    // Extraction replaces the selection with one instance node labeled
    // `⬡ <name>` and auto-selects it, which opens the inspector -- close it
    // (empty-pane click, same test-only timing fix as E2E-SN-04) so the
    // instance's own name text in the inspector doesn't collide with the
    // sidebar library row below. The "Supernode created" success toast also
    // echoes the bare name as its message; dismiss it too so exactly one
    // element matches the exact-text sidebar assertion.
    await expect(page.getByText(`⬡ ${snName}`, {exact: true})).toBeVisible();
    await page.getByRole('button', {name: 'Dismiss'}).click();
    await page.locator('.react-flow__pane').first().click({position: {x: 5, y: 5}});
    await expect(page.getByText(snName, {exact: true})).toBeVisible();

    await page.getByRole('button', {name: 'Save Policy'}).click();
    await expect(page.getByText('Policy saved')).toBeVisible();
    await expect(page.getByText('Failed to save policy')).toHaveCount(0);

    const dp = await dataPlane();
    const res = await waitForDataPlane(dp, '/sn-extract/ping', (status) => status === 200);
    expect(res.status).toBe(200);
    const echo = JSON.parse(res.body) as {path: string};
    expect(echo.path).toBe('/sn-extract/ping');
    await dp.dispose();

    await api.delete('/api/routes/sn-extract-route');
    await api.delete('/api/policies/sn-extract-policy');
    await api.delete(`/api/supernodes/${snName}`);
    await api.dispose();
  });
});
