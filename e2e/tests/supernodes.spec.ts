/**
 * Supernode scenarios. See E2E_TESTBOOK.md ("Supernodes").
 */
import {test, expect} from '@playwright/test';

import {adminApi, dataPlane, deleteRouteIfPresent} from '../helpers/admin';

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
});
