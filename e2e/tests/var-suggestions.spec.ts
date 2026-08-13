/**
 * Context-var autocomplete scenarios: the `GET /api/vars` catalog, the
 * live-value popover (trace-derived, from the newest debug trace of the
 * node's policy), and the names-only fallback when no trace exists yet.
 * See E2E_TESTBOOK.md ("Var suggestions").
 */
import {test, expect, type Page} from '@playwright/test';

import {adminApi, dataPlane, deleteRouteIfPresent} from '../helpers/admin';

/** Header that opts a single request into tracing (see debug.spec.ts). */
const DEBUG_HEADER = {'x-featherbit-debug': '1'};

interface VarEntry {
  name: string;
  kind: string;
  family_source?: string;
}

/** Opens a route's policy on the canvas and waits for the graph to render (see editor.spec.ts). */
async function openRoute(page: Page, route: string) {
  await page.goto('/');
  await page.getByText(route, {exact: true}).click();
  await page.waitForSelector('.react-flow__node');
}

test.describe('Var suggestions', () => {
  test('E2E-VS-01: GET /api/vars returns the catalog', async () => {
    const api = await adminApi();
    const res = await api.get('/api/vars');
    expect(res.status()).toBe(200);

    const body = (await res.json()) as {vars: VarEntry[]};
    const vars = body.vars;
    expect(Array.isArray(vars)).toBeTruthy();

    expect(vars).toContainEqual(expect.objectContaining({name: 'uri', kind: 'static'}));
    expect(vars).toContainEqual(
      expect.objectContaining({name: 'http_*', kind: 'family', family_source: 'request_headers'}),
    );
    expect(vars.some((v) => v.name === 'sent_http_*')).toBeTruthy();
    expect(vars.some((v) => v.name === 'request_body')).toBeTruthy();

    // No duplicate names in the catalog.
    const names = vars.map((v) => v.name);
    expect(new Set(names).size).toBe(names.length);

    await api.dispose();
  });

  test('E2E-VS-02: live suggestions with values, insertion, and the legend', async ({page}) => {
    const api = await adminApi();
    await deleteRouteIfPresent(api, 'vs-route');
    await api.delete('/api/policies/vs-policy');
    await api.delete('/api/debug/traces');

    // listener -> mock(mocking, response_example 'hello') -> client.
    const policyRes = await api.put('/api/policies/vs-policy', {
      data: {
        name: 'vs-policy',
        nodes: [
          {id: 'listener', type: 'listener', config: {}},
          {id: 'mock', type: 'mocking', config: {response_status: 200, response_example: 'hello'}},
          {id: 'client', type: 'client', config: {}},
        ],
        edges: [
          {from: 'listener.out', to: 'mock.in'},
          {from: 'mock.success', to: 'client.in'},
        ],
      },
    });
    expect(policyRes.ok(), `policy save failed: ${policyRes.status()} ${await policyRes.text()}`).toBeTruthy();

    const routeRes = await api.post('/api/routes', {
      data: {name: 'vs-route', match: {path: '/vs/*', methods: ['GET']}, policy: 'vs-policy'},
    });
    expect(routeRes.ok(), `route save failed: ${routeRes.status()} ${await routeRes.text()}`).toBeTruthy();

    // Send a traced request carrying a distinctive header, so the popover can
    // offer it back as a discovered http_* family member with a live value.
    const traffic = await dataPlane();
    const traced = await traffic.get('/vs/ping', {
      headers: {...DEBUG_HEADER, 'x-vs-probe': 'live-value-42'},
    });
    expect(traced.status()).toBe(200);
    await traffic.dispose();

    await openRoute(page, 'vs-route');
    await page.locator('.react-flow__node', {hasText: 'mock'}).first().click();

    const responseBody = page.locator('textarea');
    await expect(responseBody).toBeVisible();
    await responseBody.click();
    await responseBody.fill('$http_x_vs');
    // Caret must be at the end of the typed token for the popover's token
    // detector to pick it up (VarInput re-derives the token from selectionStart).
    await responseBody.press('End');

    const popover = page.getByTestId('var-popover');
    await expect(popover).toBeVisible();
    // exact match: the name/value cells are leaf spans with no other text, so
    // this stays a single-element match (a substring match would also hit the
    // ancestor row/list containers, which is a Playwright strict-mode violation).
    const option = popover.getByText('http_x_vs_probe', {exact: true});
    await expect(option).toBeVisible();
    await expect(popover.getByText('live-value-42', {exact: true})).toBeVisible();

    await responseBody.press('Enter');
    await expect(responseBody).toHaveValue(/\$http_x_vs_probe/);

    // Open the legend from the node inspector's header button. The popover
    // footer *also* has a "Context vars reference" link with the same
    // accessible name, so disambiguate with the aria-label attribute, which
    // only the header button carries (the popover's is a plain text button).
    await page.locator('button[aria-label="Context vars reference"]').click();
    const legend = page.getByTestId('var-legend');
    await expect(legend).toBeVisible();
    // Legend v2 (Task 8+) reorganized around namespace sections (Request/
    // Response/Message & consumer/Client/Environment) with a flat Legacy
    // mapping table at the bottom, replacing the old "Families" heading.
    await expect(legend.getByText('Legacy $var mapping', {exact: true})).toBeVisible();
    // The dotted-keys rule survives (now in the intro paragraph): names with
    // a dot (e.g. message keys) need the ${...} brace form.
    await expect(legend.locator('p', {hasText: 'containing a dot'}).first()).toBeVisible();
    // The node inspector's own header also has a "Close" (X) button, so scope
    // to the legend dialog's footer Close button specifically.
    await page.getByRole('dialog').getByRole('button', {name: 'Close', exact: true}).click();
    await expect(legend).toHaveCount(0);

    await api.delete('/api/routes/vs-route');
    await api.delete('/api/policies/vs-policy');
    await api.dispose();
  });

  test('E2E-VS-03: names-only suggestions with no trace', async ({page}) => {
    const api = await adminApi();
    await deleteRouteIfPresent(api, 'vs-route');
    await api.delete('/api/policies/vs-policy');

    const policyRes = await api.put('/api/policies/vs-policy', {
      data: {
        name: 'vs-policy',
        nodes: [
          {id: 'listener', type: 'listener', config: {}},
          {id: 'mock', type: 'mocking', config: {response_status: 200, response_example: 'hello'}},
          {id: 'client', type: 'client', config: {}},
        ],
        edges: [
          {from: 'listener.out', to: 'mock.in'},
          {from: 'mock.success', to: 'client.in'},
        ],
      },
    });
    expect(policyRes.ok()).toBeTruthy();

    const routeRes = await api.post('/api/routes', {
      data: {name: 'vs-route', match: {path: '/vs/*', methods: ['GET']}, policy: 'vs-policy'},
    });
    expect(routeRes.ok()).toBeTruthy();

    // Clear the debug buffer so this policy has no trace to preview from.
    await api.delete('/api/debug/traces');

    await openRoute(page, 'vs-route');
    await page.locator('.react-flow__node', {hasText: 'mock'}).first().click();

    const responseBody = page.locator('textarea');
    await expect(responseBody).toBeVisible();
    await responseBody.click();
    await responseBody.fill('$');
    await responseBody.press('End');

    const popover = page.getByTestId('var-popover');
    await expect(popover).toBeVisible();
    // Rows render the bare catalog name (e.g. 'uri'); VarInput.insertionText adds the '$'.
    await expect(popover.getByText('uri', {exact: true})).toBeVisible();
    await expect(
      popover.getByText('No trace yet — send a request through this route', {exact: true}),
    ).toBeVisible();

    await api.delete('/api/routes/vs-route');
    await api.delete('/api/policies/vs-policy');
    await api.dispose();
  });
});
