/**
 * Admin API scenarios. See E2E_TESTBOOK.md ("Admin API").
 */
import {test, expect, request} from '@playwright/test';

import {ADMIN_URL} from '../playwright.config';
import {adminApi, dataPlane, deleteRouteIfPresent} from '../helpers/admin';

/**
 * A genuinely anonymous request.
 *
 * Playwright's APIRequestContext inherits `httpCredentials` from the config's
 * `use` block and will transparently answer a 401 challenge with them -- so a
 * context created without credentials still ends up authenticated, and an
 * "unauthenticated" assertion silently passes against a 200. Node's fetch sends
 * nothing and answers no challenge, which is what these scenarios actually mean.
 */
const anonymous = (path: string) => fetch(`${ADMIN_URL}${path}`, {redirect: 'manual'});

test.describe('Admin API', () => {
  test('E2E-API-01: unauthenticated requests are challenged', async () => {
    const res = await anonymous('/api/routes');

    expect(res.status).toBe(401);
    expect(res.headers.get('www-authenticate')).toContain('Basic');
  });

  test('E2E-API-02: wrong credentials are rejected', async () => {
    const bad = await request.newContext({
      baseURL: ADMIN_URL,
      httpCredentials: {username: 'admin', password: 'not-the-password'},
    });
    expect((await bad.get('/api/routes')).status()).toBe(401);
    await bad.dispose();
  });

  test('E2E-API-03: lists the seeded routes', async () => {
    const api = await adminApi();
    const res = await api.get('/api/routes');
    expect(res.status()).toBe(200);

    const names = ((await res.json()) as {name: string}[]).map((r) => r.name);
    expect(names).toEqual(expect.arrayContaining(['echo-api', 'secure-api', 'dead-api']));
    await api.dispose();
  });

  /**
   * Regression guard. The catalog was hardcoded to 14 node types and never
   * updated when the APISIX plugins landed, so the editor could not add the
   * other 72 -- they existed, worked in YAML, and were simply unreachable from
   * the UI. Assert on plugins from across the catalog, not just a count, so a
   * truncated list fails loudly.
   */
  test('E2E-API-04: the plugin catalog advertises every node type', async () => {
    const api = await adminApi();
    const body = (await (await api.get('/api/plugins')).json()) as {
      plugins: {type: string; description: string}[];
    };
    const types = body.plugins.map((p) => p.type);

    expect(types).toEqual(
      expect.arrayContaining([
        'listener',
        'client',
        'upstream',
        'proxy-cache',
        'hmac-auth',
        'traffic-split',
        'opentelemetry',
        'aws-lambda',
        'clickhouse-logger',
        'script',
      ]),
    );
    // The original hardcoded catalog had 14 entries; the factory registers 86.
    expect(types.length).toBeGreaterThan(80);
    expect(new Set(types).size).toBe(types.length); // no duplicates
    await api.dispose();
  });

  test('E2E-API-05: creates a route', async () => {
    const api = await adminApi();
    await deleteRouteIfPresent(api, 'tmp-created');

    const res = await api.post('/api/routes', {
      data: {
        name: 'tmp-created',
        match: {path: '/tmp-created/*', methods: ['GET']},
        policy: 'echo-policy',
      },
    });
    expect(res.ok()).toBeTruthy();

    const names = ((await (await api.get('/api/routes')).json()) as {name: string}[]).map((r) => r.name);
    expect(names).toContain('tmp-created');

    await api.delete('/api/routes/tmp-created');
    await api.dispose();
  });

  test('E2E-API-06: updates a route in place', async () => {
    const api = await adminApi();
    await deleteRouteIfPresent(api, 'tmp-updated');
    await api.post('/api/routes', {
      data: {name: 'tmp-updated', match: {path: '/before/*', methods: ['GET']}, policy: 'echo-policy'},
    });

    const res = await api.put('/api/routes/tmp-updated', {
      data: {name: 'tmp-updated', match: {path: '/after/*', methods: ['GET']}, policy: 'echo-policy'},
    });
    expect(res.ok()).toBeTruthy();

    const route = (await (await api.get('/api/routes/tmp-updated')).json()) as {
      match: {path: string};
    };
    expect(route.match.path).toBe('/after/*');

    await api.delete('/api/routes/tmp-updated');
    await api.dispose();
  });

  test('E2E-API-07: deletes a route, and its path stops routing', async () => {
    const api = await adminApi();
    const traffic = await dataPlane();
    await deleteRouteIfPresent(api, 'tmp-doomed');

    await api.post('/api/routes', {
      data: {name: 'tmp-doomed', match: {path: '/doomed/*', methods: ['GET']}, policy: 'echo-policy'},
    });
    expect((await traffic.get('/doomed/hi')).status()).toBe(200);

    expect((await api.delete('/api/routes/tmp-doomed')).ok()).toBeTruthy();

    // The route table is recompiled on delete, so the path stops matching.
    const after = await traffic.get('/doomed/hi');
    expect(after.status()).toBe(404);

    await api.dispose();
    await traffic.dispose();
  });

  test('E2E-API-08: a route referencing an unknown policy is rejected', async () => {
    const api = await adminApi();
    const res = await api.post('/api/routes', {
      data: {name: 'tmp-bogus', match: {path: '/bogus/*', methods: ['GET']}, policy: 'no-such-policy'},
    });

    expect(res.ok()).toBeFalsy();
    expect(res.status()).toBeGreaterThanOrEqual(400);
    expect(res.status()).toBeLessThan(500);

    const names = ((await (await api.get('/api/routes')).json()) as {name: string}[]).map((r) => r.name);
    expect(names).not.toContain('tmp-bogus');
    await api.dispose();
  });

  /**
   * The last-good guarantee: a policy that fails to compile must be rejected
   * *before* anything is swapped, so the previous graph keeps serving traffic.
   */
  test('E2E-API-09: an invalid policy is rejected and the last good one keeps serving', async () => {
    const api = await adminApi();
    const traffic = await dataPlane();

    const res = await api.put('/api/policies/echo-policy', {
      data: {
        name: 'echo-policy',
        nodes: [
          {id: 'listener', type: 'listener'},
          {id: 'client', type: 'client'},
        ],
        // Edge to a node that does not exist -> must fail compilation.
        edges: [
          {from: 'listener.out', to: 'ghost-node.in'},
          {from: 'ghost-node.success', to: 'client.in'},
        ],
      },
    });
    expect(res.ok()).toBeFalsy();

    // Still serving on the pre-existing policy.
    const live = await traffic.get('/api/still-alive');
    expect(live.status()).toBe(200);

    await api.dispose();
    await traffic.dispose();
  });

  test('E2E-API-10: health, readiness and metrics endpoints', async () => {
    const api = await adminApi();

    expect((await api.get('/healthz')).status()).toBe(200);
    expect((await api.get('/readyz')).status()).toBe(200);

    const metrics = await api.get('/metrics');
    expect(metrics.status()).toBe(200);
    expect(await metrics.text()).toContain('# TYPE');

    await api.dispose();
  });

  test('E2E-API-12: config export returns the live config as YAML', async () => {
    const api = await adminApi();

    const res = await api.get('/api/config/export');
    expect(res.status()).toBe(200);
    expect(res.headers()['content-type']).toContain('text/yaml');

    const yaml = await res.text();
    // The export is the gateway.yaml equivalent of the running config: it names
    // the routes and policies the data plane is serving, and must round-trip
    // back through the same admin API. A seeded route name is enough to prove
    // it is the real config and not an empty document.
    expect(yaml).toContain('routes:');
    expect(yaml).toContain('policies:');
    expect(yaml).toContain('echo-api');

    // ...and it must be behind auth, like the rest of /api/*.
    expect((await anonymous('/api/config/export')).status).toBe(401);

    await api.dispose();
  });

  test('E2E-API-11: UI assets are served without auth, unlike /api/*', async () => {
    const ui = await anonymous('/');
    expect(ui.status).toBe(200);
    expect(await ui.text()).toContain('<div id="root">');

    // ...while the API behind it still demands credentials.
    expect((await anonymous('/api/routes')).status).toBe(401);
  });
});
