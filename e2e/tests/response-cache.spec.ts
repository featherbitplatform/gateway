/**
 * Response cache scenarios (`proxy-cache` with `policy: redis`). See
 * E2E_TESTBOOK.md ("Response cache").
 *
 * The gated Rust tests already cover the redis backend and the fail-open
 * degradation in isolation; what only this level can show is that a real
 * route serves a genuine MISS from its mock "upstream", a genuine HIT from
 * redis on the next request, and -- the property two production instances
 * actually rely on -- that a *second*, differently-configured policy sharing
 * the same `store` and cache key sees the first policy's cached entry.
 */
import {expect, request, test} from '@playwright/test';

import {adminApi} from '../helpers/admin';
import {GATEWAY_URL} from '../playwright.config';

const STORE = 'e2e-redis';
const CACHE_STATUS_HEADER = 'featherbit-cache-status';

/** Scopes every cache key to one run, so a rerun never inherits a stale entry. */
const RUN = `run-${Date.now()}-${Math.floor(Math.random() * 1e6)}`;

/**
 * Both policies below use this same `id` (the cache's namespace) and the
 * same `cache_key` template -- deriving from the request header only, not
 * from path or method -- so a request through either route lands on the
 * same redis key. That shared key, not anything about the routes
 * themselves, is what E2E-CACHE-02 exercises.
 */
const CACHE_ID = 'e2e-cache-shared';
const CACHE_KEY = ['{{request.headers.x-probe}}'];

type Policy = {name: string; nodes: unknown[]; edges: unknown[]};

/** One lookup/store pair over the shared redis cache, backed by a mock "upstream". */
const cachePolicy = (name: string, backendBody: string): Policy => ({
  name,
  nodes: [
    {id: 'listener', type: 'listener'},
    {
      id: 'lookup',
      type: 'proxy-cache',
      config: {phase: 'lookup', id: CACHE_ID, policy: 'redis', store: STORE, cache_key: CACHE_KEY},
    },
    {
      id: 'backend',
      type: 'mocking',
      config: {response_status: 200, response_example: backendBody},
    },
    {
      id: 'store',
      type: 'proxy-cache',
      config: {
        phase: 'store',
        id: CACHE_ID,
        policy: 'redis',
        store: STORE,
        cache_key: CACHE_KEY,
        cache_ttl: 60,
      },
    },
    {id: 'client', type: 'client'},
  ],
  edges: [
    {from: 'listener.out', to: 'lookup.in'},
    {from: 'lookup.success', to: 'backend.in'},
    {from: 'lookup.hit', to: 'client.in'},
    {from: 'backend.success', to: 'store.in'},
    {from: 'store.success', to: 'client.in'},
    {from: 'store.hit', to: 'client.in'}, // the store node never hits, but the port is still mandatory wiring
  ],
});

const policies: Policy[] = [
  cachePolicy('e2e-cache-a', 'from-a'),
  cachePolicy('e2e-cache-b', 'from-b'),
];

const routes = [
  {name: 'e2e-cache-a', match: {path: '/e2e-cache/a'}, policy: 'e2e-cache-a'},
  {name: 'e2e-cache-b', match: {path: '/e2e-cache/b'}, policy: 'e2e-cache-b'},
];

test.describe('Response cache', () => {
  test.skip(!process.env.FEATHERBIT_TEST_REDIS_URL, 'FEATHERBIT_TEST_REDIS_URL not set');

  test.beforeAll(async () => {
    const api = await adminApi();
    for (const p of policies) {
      const res = await api.put(`/api/policies/${p.name}`, {data: p});
      expect(res.ok(), `${p.name}: ${await res.text()}`).toBeTruthy();
    }
    for (const r of routes) {
      // Routes are created with POST (PUT replaces an existing one in place
      // and 404s otherwise); a stale route from an interrupted run would
      // otherwise collide, so clear it first.
      await api.delete(`/api/routes/${r.name}`);
      const res = await api.post('/api/routes', {data: r});
      expect(res.ok(), `${r.name}: ${await res.text()}`).toBeTruthy();
    }
    await api.dispose();
  });

  test.afterAll(async () => {
    const api = await adminApi();
    for (const r of routes) await api.delete(`/api/routes/${r.name}`);
    for (const p of policies) await api.delete(`/api/policies/${p.name}`);
    await api.dispose();
  });

  test('E2E-CACHE-01: a redis-backed route serves a MISS then a HIT', async () => {
    const traffic = await request.newContext({baseURL: GATEWAY_URL});
    const probe = {'x-probe': `${RUN}-01`};

    const first = await traffic.get('/e2e-cache/a', {headers: probe});
    expect(first.status()).toBe(200);
    expect(first.headers()[CACHE_STATUS_HEADER]).toBe('MISS');
    expect(await first.text()).toBe('from-a');

    // Same key: this time the lookup node finds the entry the store node
    // wrote after the first request, and short-circuits before the mock
    // "upstream" is asked at all.
    const second = await traffic.get('/e2e-cache/a', {headers: probe});
    expect(second.status()).toBe(200);
    expect(second.headers()[CACHE_STATUS_HEADER]).toBe('HIT');
    expect(await second.text()).toBe('from-a');

    await traffic.dispose();
  });

  test('E2E-CACHE-02: a second policy sharing the store and cache key also gets a HIT', async () => {
    const traffic = await request.newContext({baseURL: GATEWAY_URL});
    const probe = {'x-probe': `${RUN}-02`};

    // Populates the shared cache entry via policy A.
    const seed = await traffic.get('/e2e-cache/a', {headers: probe});
    expect(seed.headers()[CACHE_STATUS_HEADER]).toBe('MISS');
    expect(await seed.text()).toBe('from-a');

    // Policy B has its own route, its own node ids, and its own mock
    // "upstream" body ('from-b') -- but the same proxy-cache `id` and
    // `cache_key`, over the same `store`. If the two policies did not
    // actually share one cache namespace, this would MISS and return
    // 'from-b'.
    const other = await traffic.get('/e2e-cache/b', {headers: probe});
    expect(other.status()).toBe(200);
    expect(other.headers()[CACHE_STATUS_HEADER]).toBe('HIT');
    expect(await other.text()).toBe('from-a');

    await traffic.dispose();
  });

  test('E2E-CACHE-03: policy: redis without store fails validation naming what is missing', async () => {
    const api = await adminApi();

    const res = await api.post('/api/policies/validate', {
      data: {
        name: 'e2e-cache-missing-store',
        nodes: [
          {id: 'listener', type: 'listener'},
          {
            id: 'lookup',
            type: 'proxy-cache',
            config: {phase: 'lookup', id: 'x', policy: 'redis'}, // no `store`
          },
          {id: 'client', type: 'client'},
        ],
        edges: [
          {from: 'listener.out', to: 'lookup.in'},
          {from: 'lookup.success', to: 'client.in'},
          {from: 'lookup.hit', to: 'client.in'},
        ],
      },
    });

    expect(res.ok(), await res.text()).toBeTruthy();
    const body = (await res.json()) as {valid: boolean; errors: string[]};
    expect(body.valid).toBeFalsy();
    expect(
      body.errors.some((e) => e.includes('store')),
      JSON.stringify(body.errors),
    ).toBeTruthy();

    await api.dispose();
  });
});
