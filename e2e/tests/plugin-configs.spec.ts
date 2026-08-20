/**
 * Shared plugin config scenarios. See E2E_TESTBOOK.md ("Plugin configs").
 */
import {test, expect, type Page} from '@playwright/test';

import {adminApi, dataPlane, deleteRouteIfPresent} from '../helpers/admin';

/** Opens a route's policy on the canvas and waits for the graph to render. */
async function openRoute(page: Page, route: string) {
  await page.goto('/');
  await page.getByText(route, {exact: true}).click();
  await page.waitForSelector('.react-flow__node');
}

const DEF = (body: string) => ({
  name: 'e2e-shared-mock',
  type: 'mocking',
  config: {response_status: 200, response_example: body, content_type: 'text/plain'},
});

test.describe('Plugin configs', () => {
  test('E2E-PC-01: shared config CRUD, one-edit-updates-all, supernode inheritance, delete protection', async () => {
    const api = await adminApi();
    for (const r of ['pc-a', 'pc-b']) await deleteRouteIfPresent(api, r);
    for (const p of ['pc-a-policy', 'pc-b-policy']) await api.delete(`/api/policies/${p}`);
    await api.delete('/api/supernodes/pc-wrap');
    await api.delete('/api/plugin-configs/e2e-shared-mock');

    // Shared config + a supernode whose inner node references it.
    expect((await api.put('/api/plugin-configs/e2e-shared-mock', {data: DEF('v1')})).ok()).toBeTruthy();
    expect(
      (
        await api.put('/api/supernodes/pc-wrap', {
          data: {
            name: 'pc-wrap',
            nodes: [
              {id: 'input', type: 'input', config: {}},
              {id: 'output', type: 'output', config: {}},
              {id: 'error', type: 'error', config: {}},
              {id: 'mock', type: 'mocking', config_ref: 'e2e-shared-mock', config: {}},
            ],
            edges: [
              {from: 'input.out', to: 'mock.in'},
              {from: 'mock.success', to: 'output.in'},
            ],
          },
        })
      ).ok(),
    ).toBeTruthy();

    // Route A: direct reference with a local override (status 201).
    // Route B: reference via the supernode.
    expect(
      (
        await api.put('/api/policies/pc-a-policy', {
          data: {
            name: 'pc-a-policy',
            nodes: [
              {id: 'listener', type: 'listener', config: {}},
              {id: 'mock', type: 'mocking', config_ref: 'e2e-shared-mock', config: {response_status: 201}},
              {id: 'client', type: 'client', config: {}},
            ],
            edges: [
              {from: 'listener.out', to: 'mock.in'},
              {from: 'mock.success', to: 'client.in'},
            ],
          },
        })
      ).ok(),
    ).toBeTruthy();
    expect(
      (
        await api.put('/api/policies/pc-b-policy', {
          data: {
            name: 'pc-b-policy',
            nodes: [
              {id: 'listener', type: 'listener', config: {}},
              {id: 'sn', type: 'supernode', config: {name: 'pc-wrap'}},
              {id: 'client', type: 'client', config: {}},
            ],
            edges: [
              {from: 'listener.out', to: 'sn.in'},
              {from: 'sn.success', to: 'client.in'},
            ],
          },
        })
      ).ok(),
    ).toBeTruthy();
    for (const [route, policy] of [['pc-a', 'pc-a-policy'], ['pc-b', 'pc-b-policy']] as const) {
      expect(
        (
          await api.post('/api/routes', {
            data: {name: route, match: {path: `/${route}/*`, methods: ['GET']}, policy},
          })
        ).ok(),
      ).toBeTruthy();
    }

    const dp = await dataPlane();
    // v1 everywhere; route A's local override wins on status only.
    let a = await dp.get('/pc-a/x');
    expect(a.status()).toBe(201);
    expect(await a.text()).toBe('v1');
    let b = await dp.get('/pc-b/x');
    expect(b.status()).toBe(200);
    expect(await b.text()).toBe('v1');

    // ONE edit to the shared config -> both routes change.
    expect((await api.put('/api/plugin-configs/e2e-shared-mock', {data: DEF('v2')})).ok()).toBeTruthy();
    a = await dp.get('/pc-a/x');
    expect(a.status()).toBe(201); // local override still wins
    expect(await a.text()).toBe('v2');
    b = await dp.get('/pc-b/x');
    expect(await b.text()).toBe('v2');

    // Export keeps the reference form: config_ref present, body text only in the def.
    const yaml = await (await api.get('/api/config/export')).text();
    expect(yaml).toContain('plugin_configs:');
    expect(yaml).toContain('config_ref: e2e-shared-mock');
    expect(yaml.split('v2').length - 1).toBe(1); // materialized copies would duplicate it

    // Delete protection, then teardown order matters: consumers first.
    expect((await api.delete('/api/plugin-configs/e2e-shared-mock')).status()).toBe(400);
    for (const r of ['pc-a', 'pc-b']) await api.delete(`/api/routes/${r}`);
    for (const p of ['pc-a-policy', 'pc-b-policy']) await api.delete(`/api/policies/${p}`);
    // Still referenced by the supernode definition:
    expect((await api.delete('/api/plugin-configs/e2e-shared-mock')).status()).toBe(400);
    await api.delete('/api/supernodes/pc-wrap');
    expect((await api.delete('/api/plugin-configs/e2e-shared-mock')).ok()).toBeTruthy();

    await dp.dispose();
    await api.dispose();
  });

  test('E2E-PC-02: inspector shows inherited values inline, highlights overrides, auto-drops equal edits', async ({page}) => {
    const api = await adminApi();
    await deleteRouteIfPresent(api, 'pc-inh');
    await api.delete('/api/policies/pc-inh-policy');
    await api.delete('/api/plugin-configs/e2e-inh-mock');

    expect(
      (
        await api.put('/api/plugin-configs/e2e-inh-mock', {
          data: {
            name: 'e2e-inh-mock',
            type: 'mocking',
            config: {response_status: 200, response_example: 'inh-body', content_type: 'text/plain'},
          },
        })
      ).ok(),
    ).toBeTruthy();
    expect(
      (
        await api.put('/api/policies/pc-inh-policy', {
          data: {
            name: 'pc-inh-policy',
            nodes: [
              {id: 'listener', type: 'listener', config: {}},
              {id: 'mock', type: 'mocking', config_ref: 'e2e-inh-mock', config: {response_status: 418}},
              {id: 'client', type: 'client', config: {}},
            ],
            edges: [
              {from: 'listener.out', to: 'mock.in'},
              {from: 'mock.success', to: 'client.in'},
            ],
          },
        })
      ).ok(),
    ).toBeTruthy();
    expect(
      (
        await api.post('/api/routes', {
          data: {name: 'pc-inh', match: {path: '/pc-inh/*', methods: ['GET']}, policy: 'pc-inh-policy'},
        })
      ).ok(),
    ).toBeTruthy();

    await openRoute(page, 'pc-inh');
    await page.locator('.react-flow__node', {hasText: 'mock'}).first().click();

    // The local override (418) is shown and flagged; the inherited body is
    // shown inline in its field (not just in a read-only JSON blob) and not flagged.
    await expect(page.locator('input[value="418"]')).toBeVisible();
    await expect(page.getByText('overrides shared', {exact: true})).toBeVisible();
    await expect(page.locator('textarea')).toHaveValue('inh-body');
    await expect(page.getByText('added', {exact: true})).toHaveCount(0);

    // Editing the override back to the inherited value clears the flag...
    await page.locator('input[value="418"]').fill('200');
    await expect(page.getByText('overrides shared', {exact: true})).toHaveCount(0);

    // ...while adding a key the shared config does not set flags it as added.
    await page.getByRole('switch').click();
    await expect(page.getByText('added', {exact: true})).toBeVisible();

    // The auto-dropped key is gone from the persisted node config.
    await page.getByRole('button', {name: 'Save Policy'}).click();
    await expect
      .poll(async () => {
        const policy = (await (await api.get('/api/policies/pc-inh-policy')).json()) as {
          nodes: {id: string; config: Record<string, unknown>; config_ref?: string}[];
        };
        return policy.nodes.find((n) => n.id === 'mock')?.config;
      })
      .toEqual({with_mock_header: false});

    await api.delete('/api/routes/pc-inh');
    await api.delete('/api/policies/pc-inh-policy');
    await api.delete('/api/plugin-configs/e2e-inh-mock');
    await api.dispose();
  });

  test('E2E-PC-03: a configured node is saved as a shared config and re-linked to it', async ({page}) => {
    const api = await adminApi();
    await deleteRouteIfPresent(api, 'pc-ext');
    await api.delete('/api/policies/pc-ext-policy');
    await api.delete('/api/plugin-configs/e2e-extracted');

    expect(
      (
        await api.put('/api/policies/pc-ext-policy', {
          data: {
            name: 'pc-ext-policy',
            nodes: [
              {id: 'listener', type: 'listener', config: {}},
              {
                id: 'mock',
                type: 'mocking',
                config: {response_status: 201, response_example: 'ext-body', content_type: 'text/plain'},
              },
              {id: 'client', type: 'client', config: {}},
            ],
            edges: [
              {from: 'listener.out', to: 'mock.in'},
              {from: 'mock.success', to: 'client.in'},
            ],
          },
        })
      ).ok(),
    ).toBeTruthy();
    expect(
      (
        await api.post('/api/routes', {
          data: {name: 'pc-ext', match: {path: '/pc-ext/*', methods: ['GET']}, policy: 'pc-ext-policy'},
        })
      ).ok(),
    ).toBeTruthy();

    await openRoute(page, 'pc-ext');
    await page.locator('.react-flow__node', {hasText: 'mock'}).first().click();

    await page.getByRole('button', {name: 'Save as shared config'}).click();
    await page.getByPlaceholder('my-shared-config').fill('e2e-extracted');
    await page.getByRole('button', {name: 'Save shared config'}).click();

    // The shared config now exists with the node's effective config...
    await expect
      .poll(async () => {
        const res = await api.get('/api/plugin-configs/e2e-extracted');
        return res.ok() ? ((await res.json()) as {config: Record<string, unknown>}).config : null;
      })
      .toEqual({response_status: 201, response_example: 'ext-body', content_type: 'text/plain'});

    // ...and the node switched to referencing it, with nothing left local.
    await expect(page.locator('select').first()).toHaveValue('e2e-extracted');
    await expect(page.getByText('overrides shared', {exact: true})).toHaveCount(0);
    await page.getByRole('button', {name: 'Save Policy'}).click();
    await expect
      .poll(async () => {
        const policy = (await (await api.get('/api/policies/pc-ext-policy')).json()) as {
          nodes: {id: string; config: Record<string, unknown>; config_ref?: string}[];
        };
        const mock = policy.nodes.find((n) => n.id === 'mock');
        return {ref: mock?.config_ref, config: mock?.config};
      })
      .toEqual({ref: 'e2e-extracted', config: {}});

    // The route's behavior is unchanged after the extraction.
    const dp = await dataPlane();
    const res = await dp.get('/pc-ext/x');
    expect(res.status()).toBe(201);
    expect(await res.text()).toBe('ext-body');

    // A duplicate name is rejected client-side (the PUT is an upsert).
    await page.getByRole('button', {name: 'Save as shared config'}).click();
    await page.getByPlaceholder('my-shared-config').fill('e2e-extracted');
    await page.getByRole('button', {name: 'Save shared config'}).click();
    await expect(page.getByText(/already exists/i)).toBeVisible();

    await dp.dispose();
    await api.delete('/api/routes/pc-ext');
    await api.delete('/api/policies/pc-ext-policy');
    await api.delete('/api/plugin-configs/e2e-extracted');
    await api.dispose();
  });
});
