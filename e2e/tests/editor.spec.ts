/**
 * Web-UI scenarios, and the full-loop tests that close browser -> admin API ->
 * recompile -> hot-swap -> live traffic. See E2E_TESTBOOK.md.
 */
import {test, expect, type Page} from '@playwright/test';

import {adminApi, dataPlane, deleteRouteIfPresent, waitForDataPlane} from '../helpers/admin';

/** Opens a route's policy on the canvas and waits for the graph to render. */
async function openRoute(page: Page, route: string) {
  await page.goto('/');
  await page.getByText(route, {exact: true}).click();
  await page.waitForSelector('.react-flow__node');
}

test.describe('Web UI', () => {
  test('E2E-UI-01: the editor loads and lists the seeded routes', async ({page}) => {
    await page.goto('/');

    await expect(page.getByText('featherbit')).toBeVisible();
    await expect(page.getByText('echo-api', {exact: true})).toBeVisible();
    await expect(page.getByText('secure-api', {exact: true})).toBeVisible();
    await expect(page.getByText('dead-api', {exact: true})).toBeVisible();
    // The sidebar status line reports the route count from GET /api/status.
    await expect(page.getByText(/routes/i).first()).toBeVisible();
  });

  test('E2E-UI-02: selecting a route renders the policy declared in YAML', async ({page}) => {
    await openRoute(page, 'echo-api');

    // echo-policy: listener, cors, strip-prefix, echo-backend, error-handler, client.
    await expect(page.locator('.react-flow__node')).toHaveCount(6);
    for (const id of ['listener', 'cors', 'strip-prefix', 'echo-backend', 'error-handler', 'client']) {
      await expect(page.locator('.react-flow__node', {hasText: id}).first()).toBeVisible();
    }
    // Edges are rendered too -- the graph is wired, not just a pile of nodes.
    expect(await page.locator('.react-flow__edge').count()).toBeGreaterThan(0);
  });

  /**
   * Regression guard, the UI half of E2E-API-04: the palette is what a user
   * actually reaches for, and it offered only 14 of 86 plugins.
   */
  test('E2E-UI-03: the palette offers the full plugin catalog', async ({page}) => {
    await openRoute(page, 'echo-api');
    await page.getByRole('button', {name: 'Add Node'}).click();

    // The drawer groups plugins under the docs categories (collapsed by
    // default), so assert the category headers are present...
    for (const category of ['Traffic control', 'Authentication & consumers', 'Serverless & FaaS']) {
      await expect(page.getByText(category, {exact: true})).toBeVisible();
    }

    // ...and that the quick-search surfaces plugins that only exist post-APISIX
    // port (absent from the old 14-type catalog), which the search force-expands.
    const search = page.getByPlaceholder(/Search plugins/);
    for (const plugin of ['proxy-cache', 'hmac-auth', 'traffic-split']) {
      await search.fill(plugin);
      await expect(page.getByText(plugin, {exact: true})).toBeVisible();
    }
  });

  test('E2E-UI-04: a node can be added from the drawer via search', async ({page}) => {
    await openRoute(page, 'echo-api');
    const before = await page.locator('.react-flow__node').count();

    await page.getByRole('button', {name: 'Add Node'}).click();
    // Search narrows the palette, then the match is clicked to add the node.
    await page.getByPlaceholder(/Search plugins/).fill('request-id');
    await page.getByText('request-id', {exact: true}).click();

    await expect(page.locator('.react-flow__node')).toHaveCount(before + 1);
  });

  test('E2E-UI-14: drawer categories expand to reveal their plugins', async ({page}) => {
    await openRoute(page, 'echo-api');
    await page.getByRole('button', {name: 'Add Node'}).click();

    // Collapsed by default: a plugin under a category is not yet shown.
    await expect(page.getByText('rate-limit', {exact: true})).toHaveCount(0);

    // Expanding its category reveals it; collapsing hides it again.
    await page.getByText('Traffic control', {exact: true}).click();
    await expect(page.getByText('rate-limit', {exact: true})).toBeVisible();
    await page.getByText('Traffic control', {exact: true}).click();
    await expect(page.getByText('rate-limit', {exact: true})).toHaveCount(0);
  });

  test('E2E-UI-05: clicking a node opens its config form', async ({page}) => {
    await openRoute(page, 'secure-api');
    await page.locator('.react-flow__node', {hasText: 'api-key'}).first().click();

    // key-auth's schema-driven form: the header name and the valid keys.
    await expect(page.getByText('Node ID')).toBeVisible();
    await expect(page.getByText('Header name')).toBeVisible();
    await expect(page.locator('input[value="x-api-key"]')).toBeVisible();
  });

  test('E2E-UI-06: a route created in the UI appears in the API', async ({page}) => {
    const api = await adminApi();
    await deleteRouteIfPresent(api, 'ui-made');

    await page.goto('/');
    await page.getByRole('button', {name: 'New'}).click();
    await page.getByPlaceholder('echo-api').fill('ui-made');
    await page.getByPlaceholder('/api/*').fill('/ui-made/*');
    await page.getByRole('button', {name: 'Create route'}).click();

    await expect(page.getByText('ui-made', {exact: true})).toBeVisible();

    const names = ((await (await api.get('/api/routes')).json()) as {name: string}[]).map((r) => r.name);
    expect(names).toContain('ui-made');

    await api.delete('/api/routes/ui-made');
    await api.dispose();
  });

  test('E2E-UI-07: a route deleted in the UI is gone from the API', async ({page}) => {
    const api = await adminApi();
    await deleteRouteIfPresent(api, 'ui-doomed');
    const created = await api.post('/api/routes', {
      data: {name: 'ui-doomed', match: {path: '/ui-doomed/*', methods: ['GET']}, policy: 'echo-policy'},
    });
    expect(created.ok(), `route creation failed: ${created.status()} ${await created.text()}`).toBeTruthy();

    const seeded = ((await (await api.get('/api/routes')).json()) as {name: string}[]).map((r) => r.name);
    expect(seeded, 'route must exist before the UI can delete it').toContain('ui-doomed');

    await page.goto('/');
    const row = page.getByText('ui-doomed', {exact: true});
    await expect(row).toBeVisible();

    await row.hover(); // the delete button is only revealed on hover
    await page.getByRole('button', {name: 'Delete route ui-doomed'}).click();
    // Deletion is behind a confirmation dialog.
    await page.getByRole('button', {name: 'Delete', exact: true}).click();

    // Assert on the row's own delete button, not on the text: the success toast's
    // message is the bare route name, so a getByText('ui-doomed') would match the
    // toast and report the row as still present long after it is gone.
    await expect(page.getByRole('button', {name: 'Delete route ui-doomed'})).toHaveCount(0);

    const names = ((await (await api.get('/api/routes')).json()) as {name: string}[]).map((r) => r.name);
    expect(names).not.toContain('ui-doomed');
    await api.dispose();
  });

  test('E2E-UI-08: the theme toggle persists across a reload', async ({page}) => {
    // The toggle lives in the canvas toolbar, which only exists once a policy is
    // on the canvas -- with no route selected the canvas is an empty-state panel.
    await openRoute(page, 'echo-api');

    // Dark is the default, and the attribute is absent for it (see ThemeToggle).
    const theme = () => page.evaluate(() => document.documentElement.dataset.theme ?? 'dark');
    const before = await theme();

    await page.getByRole('button', {name: /Switch to (light|dark) mode/}).click();
    await expect.poll(theme).not.toBe(before);
    const after = await theme();

    // Persisted to localStorage, so it survives a reload.
    await page.reload();
    expect(await theme()).toBe(after);
  });
});

test.describe('The loop: browser -> admin API -> hot-swap -> live traffic', () => {
  /**
   * The scenario that justifies this suite. Nothing else asserts that editing a
   * policy in the browser changes what the gateway does to real requests.
   */
  test('E2E-LOOP-01: an inspector edit saved in the UI changes live traffic', async ({page}) => {
    const traffic = await dataPlane();
    const api = await adminApi();

    // Before: the dead upstream renders the error-handler's configured 502.
    expect((await traffic.get('/dead/before')).status()).toBe(502);

    await openRoute(page, 'dead-api');
    await page.locator('.react-flow__node', {hasText: 'error-handler'}).first().click();

    // Retune the handler's status code through the inspector form.
    const statusField = page.locator('input[value="502"]');
    await expect(statusField).toBeVisible();
    await statusField.fill('599');

    // The inspector is an overlay pinned to the right edge, and it sits on top of
    // the toolbar that holds Save Policy -- so the button is unclickable until the
    // panel is closed. The edit is already committed to canvas state on change, so
    // closing does not discard it.
    await page.getByRole('button', {name: 'Close'}).click();
    await page.getByRole('button', {name: 'Save Policy'}).click();

    // After: the same request now gets the new status -- validated, recompiled
    // and hot-swapped, with no restart.
    const after = await waitForDataPlane(traffic, '/dead/after', (status) => status === 599);
    expect(after.status).toBe(599);

    // Restore, so the fixture's behavior is not left mutated for other specs.
    const policy = (await (await api.get('/api/policies/dead-policy')).json()) as {
      nodes: {id: string; config?: Record<string, unknown>}[];
    };
    const handler = policy.nodes.find((n) => n.id === 'error-handler')!;
    handler.config!.status_code = 502;
    await api.put('/api/policies/dead-policy', {data: policy});
    await waitForDataPlane(traffic, '/dead/restored', (status) => status === 502);

    await traffic.dispose();
    await api.dispose();
  });

  /**
   * The other half of the guarantee: a bad save must not take traffic down.
   */
  test('E2E-LOOP-02: an invalid policy is refused and traffic keeps flowing', async () => {
    const api = await adminApi();
    const traffic = await dataPlane();

    expect((await traffic.get('/api/before')).status()).toBe(200);

    // A policy whose graph cannot compile (edge into a node that isn't there).
    const res = await api.put('/api/policies/echo-policy', {
      data: {
        name: 'echo-policy',
        nodes: [
          {id: 'listener', type: 'listener'},
          {id: 'client', type: 'client'},
        ],
        edges: [{from: 'listener.out', to: 'nowhere.in'}],
      },
    });
    expect(res.ok()).toBeFalsy();

    // The last good graph is still serving -- the rejected config never swapped in.
    const after = await traffic.get('/api/after');
    expect(after.status()).toBe(200);
    expect(((await after.json()) as {path: string}).path).toBe('/after');

    await api.dispose();
    await traffic.dispose();
  });
});
