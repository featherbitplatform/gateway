/**
 * Shared plugin config scenarios. See E2E_TESTBOOK.md ("Plugin configs").
 */
import {test, expect} from '@playwright/test';

import {adminApi, dataPlane, deleteRouteIfPresent} from '../helpers/admin';

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
});
