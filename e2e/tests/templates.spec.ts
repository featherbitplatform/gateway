/**
 * Universal `{{path}}` template rendering, the field-scoped suggestion
 * popover (full context vs. env-only), and the env-var name catalog.
 * See E2E_TESTBOOK.md ("Universal templates").
 *
 * Idioms reused from var-suggestions.spec.ts (openRoute, the debug trigger
 * header, PUT-policy/POST-route reset-then-seed) and editor.spec.ts (closing
 * the inspector before Save Policy is clickable).
 */
import {test, expect, type Page} from '@playwright/test';

import {adminApi, dataPlane, deleteRouteIfPresent, waitForDataPlane} from '../helpers/admin';
import {ENV_CANARY_NAME, ENV_CANARY_VALUE} from '../playwright.config';

/** Header that opts a single request into tracing (see debug.spec.ts). */
const DEBUG_HEADER = {'x-featherbit-debug': '1'};

/** Opens a route's policy on the canvas and waits for the graph to render (see editor.spec.ts). */
async function openRoute(page: Page, route: string) {
  await page.goto('/');
  await page.getByText(route, {exact: true}).click();
  await page.waitForSelector('.react-flow__node');
}

test.describe('Universal templates', () => {
  test('E2E-TPL-01: {{request.method}} renders while a literal $ passes through untouched', async () => {
    const api = await adminApi();
    await deleteRouteIfPresent(api, 'tpl-render-api');
    await api.delete('/api/policies/tpl-render-policy');

    // listener -> proxy-rewrite (add_headers with a {{path}} ref alongside a
    // literal $) -> echo backend -> client. add_headers only ever runs the
    // {{...}} template engine (Template::parse), never legacy $-interpolation
    // (src/plugins/native/proxy_rewrite.rs), so "$keep" must reach the
    // upstream byte-for-byte while "{{request.method}}" resolves.
    const policyRes = await api.put('/api/policies/tpl-render-policy', {
      data: {
        name: 'tpl-render-policy',
        nodes: [
          {id: 'listener', type: 'listener', config: {}},
          {
            id: 'rewrite',
            type: 'proxy-rewrite',
            config: {
              phase: 'request',
              add_headers: [{name: 'x-tpl', value: 'm={{request.method}} $keep'}],
            },
          },
          {
            id: 'echo-backend',
            type: 'upstream',
            config: {targets: [{host: '127.0.0.1', port: 3010}]},
          },
          {id: 'client', type: 'client'},
        ],
        edges: [
          {from: 'listener.out', to: 'rewrite.in'},
          {from: 'rewrite.success', to: 'echo-backend.in'},
          {from: 'echo-backend.success', to: 'client.in'},
        ],
      },
    });
    expect(policyRes.ok(), `policy save failed: ${policyRes.status()} ${await policyRes.text()}`).toBeTruthy();

    const routeRes = await api.post('/api/routes', {
      data: {name: 'tpl-render-api', match: {path: '/tpl-render/*', methods: ['GET']}, policy: 'tpl-render-policy'},
    });
    expect(routeRes.ok(), `route save failed: ${routeRes.status()} ${await routeRes.text()}`).toBeTruthy();

    const traffic = await dataPlane();
    const {body} = await waitForDataPlane(traffic, '/tpl-render/ping', (status, text) => {
      if (status !== 200) return false;
      try {
        return (JSON.parse(text) as {headers: Record<string, string>}).headers?.['x-tpl'] !== undefined;
      } catch {
        return false;
      }
    });
    const echo = JSON.parse(body) as {headers: Record<string, string>};
    // Rendered: {{request.method}} -> GET. Untouched: the literal $keep, byte-for-byte.
    expect(echo.headers['x-tpl']).toBe('m=GET $keep');

    await traffic.dispose();
    await api.delete('/api/routes/tpl-render-api');
    await api.delete('/api/policies/tpl-render-policy');
    await api.dispose();
  });

  test('E2E-TPL-02: the add_headers value field offers live {{path}} suggestions and saves', async ({page}) => {
    const api = await adminApi();
    await deleteRouteIfPresent(api, 'tpl-editor-api');
    await api.delete('/api/policies/tpl-editor-policy');
    await api.delete('/api/debug/traces');

    const policyRes = await api.put('/api/policies/tpl-editor-policy', {
      data: {
        name: 'tpl-editor-policy',
        nodes: [
          {id: 'listener', type: 'listener', config: {}},
          {
            id: 'rewrite',
            type: 'proxy-rewrite',
            config: {phase: 'request', add_headers: [{name: 'x-hdr', value: ''}]},
          },
          {
            id: 'echo-backend',
            type: 'upstream',
            config: {targets: [{host: '127.0.0.1', port: 3010}]},
          },
          {id: 'client', type: 'client'},
        ],
        edges: [
          {from: 'listener.out', to: 'rewrite.in'},
          {from: 'rewrite.success', to: 'echo-backend.in'},
          {from: 'echo-backend.success', to: 'client.in'},
        ],
      },
    });
    expect(policyRes.ok(), `policy save failed: ${policyRes.status()} ${await policyRes.text()}`).toBeTruthy();

    const routeRes = await api.post('/api/routes', {
      data: {name: 'tpl-editor-api', match: {path: '/tpl-editor/*', methods: ['GET']}, policy: 'tpl-editor-policy'},
    });
    expect(routeRes.ok(), `route save failed: ${routeRes.status()} ${await routeRes.text()}`).toBeTruthy();

    // A traced GET so `rewrite`'s predecessor snapshot (the listener's
    // initial capture) has a real request.method to preview.
    const traffic = await dataPlane();
    const traced = await traffic.get('/tpl-editor/ping', {headers: DEBUG_HEADER});
    expect(traced.status()).toBe(200);
    await traffic.dispose();

    await openRoute(page, 'tpl-editor-api');
    await page.locator('.react-flow__node', {hasText: 'rewrite'}).first().click();

    // The add_headers "Value" sub-field: template: 'full', so it offers
    // request/response/message context, not just env.* -- unlike E2E-TPL-03's
    // env-only field.
    const valueField = page.getByPlaceholder('$uuid');
    await expect(valueField).toBeVisible();
    await valueField.click();
    await valueField.fill('{{re');
    await valueField.press('End');

    const popover = page.getByTestId('var-popover');
    await expect(popover).toBeVisible();
    await expect(popover.getByText('request.method', {exact: true})).toBeVisible();
    // Live value from the traced GET's predecessor snapshot.
    await expect(popover.getByText('GET', {exact: true})).toBeVisible();

    // "{{re" alone also matches request.path/response.*/etc. -- narrow the
    // filter to the one row this scenario cares about before hitting Enter,
    // so insertion is deterministic regardless of the catalog's row order.
    await valueField.fill('{{request.meth');
    await valueField.press('End');
    await expect(popover.getByText('request.method', {exact: true})).toBeVisible();
    await valueField.press('Enter');
    await expect(valueField).toHaveValue('{{request.method}}');

    // The inspector overlay sits on top of the toolbar's Save Policy button.
    await page.getByRole('button', {name: 'Close'}).click();
    await page.getByRole('button', {name: 'Save Policy'}).click();

    const saved = (await (await api.get('/api/policies/tpl-editor-policy')).json()) as {
      nodes: {id: string; config?: {add_headers?: {name: string; value: string}[]}}[];
    };
    const rewrite = saved.nodes.find((n) => n.id === 'rewrite')!;
    expect(rewrite.config?.add_headers?.[0]?.value).toBe('{{request.method}}');

    await api.delete('/api/routes/tpl-editor-api');
    await api.delete('/api/policies/tpl-editor-policy');
    await api.dispose();
  });

  test('E2E-TPL-03: an env-only field offers only {{env.*}} suggestions, filterable by name', async ({page}) => {
    const api = await adminApi();
    await deleteRouteIfPresent(api, 'tpl-env-api');
    await api.delete('/api/policies/tpl-env-policy');

    const policyRes = await api.put('/api/policies/tpl-env-policy', {
      data: {
        name: 'tpl-env-policy',
        nodes: [
          {id: 'listener', type: 'listener', config: {}},
          {
            id: 'blocker',
            type: 'uri-blocker',
            config: {block_rules: ['^/tpl-env-blocked/']},
          },
          {
            id: 'echo-backend',
            type: 'upstream',
            config: {targets: [{host: '127.0.0.1', port: 3010}]},
          },
          {id: 'client', type: 'client'},
        ],
        edges: [
          {from: 'listener.out', to: 'blocker.in'},
          {from: 'blocker.success', to: 'echo-backend.in'},
          {from: 'echo-backend.success', to: 'client.in'},
        ],
      },
    });
    expect(policyRes.ok(), `policy save failed: ${policyRes.status()} ${await policyRes.text()}`).toBeTruthy();

    const routeRes = await api.post('/api/routes', {
      data: {name: 'tpl-env-api', match: {path: '/tpl-env/*', methods: ['GET']}, policy: 'tpl-env-policy'},
    });
    expect(routeRes.ok(), `route save failed: ${routeRes.status()} ${await routeRes.text()}`).toBeTruthy();

    await openRoute(page, 'tpl-env-api');
    await page.locator('.react-flow__node', {hasText: 'blocker'}).first().click();

    // block_rules' item field is `template: 'env-only'`.
    const ruleField = page.getByPlaceholder('^/admin/');
    await expect(ruleField).toBeVisible();
    await ruleField.click();
    await ruleField.fill('{{');
    await ruleField.press('End');

    const popover = page.getByTestId('var-popover');
    await expect(popover).toBeVisible();
    // env.LOG_LEVEL is set explicitly on the gateway's own launch env
    // (playwright.config.ts) -- a name every run is guaranteed to see.
    await expect(popover.getByText('env.LOG_LEVEL', {exact: true})).toBeVisible();
    // Only env.* is on offer: no request/response/message rows leak in.
    await expect(popover.getByText('request.method', {exact: true})).toHaveCount(0);
    await expect(popover.getByText(/^request\./)).toHaveCount(0);

    await ruleField.fill('{{env.LOG');
    await ruleField.press('End');
    await expect(popover).toBeVisible();
    await expect(popover.getByText('env.LOG_LEVEL', {exact: true})).toBeVisible();
    // A real env name that does not match the "LOG" filter must drop out.
    await expect(popover.getByText('env.ECHO_HOST', {exact: true})).toHaveCount(0);

    await api.delete('/api/routes/tpl-env-api');
    await api.delete('/api/policies/tpl-env-policy');
    await api.dispose();
  });

  test('E2E-TPL-04: GET /api/env-vars lists sorted names and never values', async () => {
    const api = await adminApi();
    const res = await api.get('/api/env-vars');
    expect(res.status()).toBe(200);

    const bodyText = await res.text();
    const body = JSON.parse(bodyText) as {names: string[]};
    const names = body.names;
    expect(Array.isArray(names)).toBeTruthy();
    expect(names.length).toBeGreaterThan(0);

    const sorted = [...names].sort();
    expect(names).toEqual(sorted);

    // The canary name (set only on the gateway's own launch env) is visible...
    expect(names).toContain(ENV_CANARY_NAME);
    // ...but its value never is: names only, never values. (Not asserting
    // the body has no bare "=" at all -- Windows has real env var *names*
    // containing one, e.g. the per-drive cwd tracker "=C:".)
    expect(bodyText).not.toContain(ENV_CANARY_VALUE);

    await api.dispose();
  });
});
