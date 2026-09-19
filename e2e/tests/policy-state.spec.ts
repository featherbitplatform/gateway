/**
 * Policy state scenarios (`store-get` / `store-set` / `store-incr` /
 * `store-delete`). See E2E_TESTBOOK.md ("Policy state").
 *
 * These drive the nodes the way an operator does -- through a real route on
 * the data plane -- rather than by calling them directly. The gated Rust
 * tests in `src/plugins/util/store_kv.rs` already cover each node against a
 * live redis in isolation; what only this level can show is that the nodes
 * compose: that `miss` is a wired branch a request actually takes, that a
 * counter survives between requests, and that its TTL resets the window.
 */
import {expect, request, test} from '@playwright/test';

import {adminApi} from '../helpers/admin';
import {GATEWAY_URL} from '../playwright.config';

/** Upstream-free policies: every path ends in a mock, so no echo backend is needed. */
const STORE = 'e2e-redis';

/** Scopes every key to one run, so a rerun never inherits a previous counter. */
const RUN = `run-${Date.now()}-${Math.floor(Math.random() * 1e6)}`;

/**
 * `store-incr`'s window in these tests.
 *
 * Long enough to land a second increment comfortably inside it and still
 * check comfortably after it closes -- see E2E-STORE-12, where those margins
 * are what make the test able to fail.
 */
const TTL_SECONDS = 3;

type Policy = {name: string; nodes: unknown[]; edges: unknown[]};

const mock = (id: string, status: number, body: string) => ({
  id,
  type: 'mocking',
  config: {response_status: status, response_example: body},
});

/**
 * The key is templated from a request header, which is also the point: it
 * proves the rendered key reaches redis per-request rather than being fixed
 * at compile time.
 */
const KEY = `${RUN}:{{request.headers.x-probe}}`;

const policies: Policy[] = [
  {
    name: 'e2e-store-read',
    nodes: [
      {id: 'listener', type: 'listener'},
      {id: 'read', type: 'store-get', config: {store: STORE, key: KEY, name: 'v'}},
      mock('hit', 200, 'hit={{message.v}}'),
      mock('absent', 404, 'miss'),
      {id: 'client', type: 'client'},
    ],
    edges: [
      {from: 'listener.out', to: 'read.in'},
      {from: 'read.success', to: 'hit.in'},
      {from: 'read.miss', to: 'absent.in'},
      {from: 'hit.success', to: 'client.in'},
      {from: 'absent.success', to: 'client.in'},
    ],
  },
  {
    name: 'e2e-store-write',
    nodes: [
      {id: 'listener', type: 'listener'},
      {id: 'write', type: 'store-set', config: {store: STORE, key: KEY, value: 'stored', ttl_seconds: 60}},
      mock('ok', 200, 'set'),
      {id: 'client', type: 'client'},
    ],
    edges: [
      {from: 'listener.out', to: 'write.in'},
      {from: 'write.success', to: 'ok.in'},
      {from: 'ok.success', to: 'client.in'},
    ],
  },
  {
    name: 'e2e-store-drop',
    nodes: [
      {id: 'listener', type: 'listener'},
      {id: 'drop', type: 'store-delete', config: {store: STORE, key: KEY}},
      mock('ok', 200, 'deleted'),
      {id: 'client', type: 'client'},
    ],
    edges: [
      {from: 'listener.out', to: 'drop.in'},
      {from: 'drop.success', to: 'ok.in'},
      {from: 'ok.success', to: 'client.in'},
    ],
  },
  {
    name: 'e2e-store-count',
    nodes: [
      {id: 'listener', type: 'listener'},
      {
        id: 'bump',
        type: 'store-incr',
        config: {store: STORE, key: KEY, name: 'n', ttl_seconds: TTL_SECONDS},
      },
      mock('ok', 200, 'count={{message.n}}'),
      {id: 'client', type: 'client'},
    ],
    edges: [
      {from: 'listener.out', to: 'bump.in'},
      {from: 'bump.success', to: 'ok.in'},
      {from: 'ok.success', to: 'client.in'},
    ],
  },
];

const routes = [
  {name: 'e2e-store-read', match: {path: '/e2e-store/read'}, policy: 'e2e-store-read'},
  {name: 'e2e-store-write', match: {path: '/e2e-store/write'}, policy: 'e2e-store-write'},
  {name: 'e2e-store-drop', match: {path: '/e2e-store/drop'}, policy: 'e2e-store-drop'},
  {name: 'e2e-store-count', match: {path: '/e2e-store/count'}, policy: 'e2e-store-count'},
];

test.describe('Policy state', () => {
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

  test('E2E-STORE-10: a read misses, a write makes it hit, a delete makes it miss again', async () => {
    const traffic = await request.newContext({baseURL: GATEWAY_URL});
    const probe = {'x-probe': 'k10'};

    // Nothing stored yet: the request leaves through `miss`, which is a wired
    // branch of the policy rather than an error.
    const first = await traffic.get('/e2e-store/read', {headers: probe});
    expect(first.status()).toBe(404);
    expect(await first.text()).toBe('miss');

    expect((await traffic.get('/e2e-store/write', {headers: probe})).status()).toBe(200);

    // The value survives between requests -- the whole point of the feature.
    const second = await traffic.get('/e2e-store/read', {headers: probe});
    expect(second.status()).toBe(200);
    expect(await second.text()).toBe('hit=stored');

    expect((await traffic.get('/e2e-store/drop', {headers: probe})).status()).toBe(200);

    // Deleting returns the route to its miss branch, which is what makes
    // `store-delete` usable for clearing state after a successful flow.
    const third = await traffic.get('/e2e-store/read', {headers: probe});
    expect(third.status()).toBe(404);
    expect(await third.text()).toBe('miss');

    await traffic.dispose();
  });

  test('E2E-STORE-11: keys are scoped per rendered value, not shared across callers', async () => {
    const traffic = await request.newContext({baseURL: GATEWAY_URL});

    await traffic.get('/e2e-store/write', {headers: {'x-probe': 'tenant-a'}});

    // Same route, same policy, different rendered key: a template that
    // collapsed to one constant would make this read hit, which is the
    // cross-tenant leak the empty-key guard exists to prevent.
    const other = await traffic.get('/e2e-store/read', {headers: {'x-probe': 'tenant-b'}});
    expect(other.status()).toBe(404);

    const own = await traffic.get('/e2e-store/read', {headers: {'x-probe': 'tenant-a'}});
    expect(own.status()).toBe(200);

    await traffic.dispose();
  });

  test('E2E-STORE-12: the counter increments across requests and its TTL resets the window', async () => {
    test.setTimeout(30_000);
    const traffic = await request.newContext({baseURL: GATEWAY_URL});
    const probe = {'x-probe': 'k12'};

    // t=0. The key is created here, so the window closes at t=TTL_SECONDS.
    const first = await traffic.get('/e2e-store/count', {headers: probe});
    expect(await first.text()).toBe('count=1');

    // t~2s: inside the window. This increment is the whole point of the
    // timing. A correct (creation-pinned) TTL leaves the expiry at t=3; a
    // refreshing one would push it out to t=5.
    //
    // Without an increment here the test cannot fail: with a quiet period
    // longer than the TTL, a refreshing implementation expires at exactly the
    // same moment as a correct one, and the assertion below passes either
    // way. Verified by mutation -- an unconditional EXPIRE passed the earlier
    // version of this test.
    await new Promise((r) => setTimeout(r, 2000));
    const second = await traffic.get('/e2e-store/count', {headers: probe});
    expect(await second.text()).toBe('count=2');

    // t~4s: past the creation-pinned expiry (t=3), before a refreshed one
    // would have expired (t=5). A counter still alive here has been kept
    // alive by its own traffic, which is the failure the pinned TTL exists to
    // prevent -- a client that keeps retrying would never let its own bound
    // reset.
    await new Promise((r) => setTimeout(r, 2000));

    const afterExpiry = await traffic.get('/e2e-store/count', {headers: probe});
    expect(
      await afterExpiry.text(),
      'a refreshing TTL would report count=3 here',
    ).toBe('count=1');

    await traffic.dispose();
  });

  test('E2E-STORE-13: store nodes do not force an upstream to buffer', async () => {
    const api = await adminApi();

    // `reads_response_body` is answered per configured instance, so a store
    // node whose key is an ordinary template must leave a streaming upstream
    // streaming. The validate endpoint names any node that blocks.
    const res = await api.post('/api/policies/validate', {
      data: {
        name: 'e2e-store-stream-check',
        nodes: [
          {id: 'listener', type: 'listener'},
          {id: 'up', type: 'upstream', config: {targets: [{host: '127.0.0.1', port: 3010}]}},
          {id: 'write', type: 'store-set', config: {store: STORE, key: KEY, value: 'x'}},
          {id: 'client', type: 'client'},
        ],
        edges: [
          {from: 'listener.out', to: 'up.in'},
          {from: 'up.success', to: 'write.in'},
          {from: 'write.success', to: 'client.in'},
        ],
      },
    });

    expect(res.ok(), await res.text()).toBeTruthy();
    const body = (await res.json()) as {valid: boolean; buffering?: unknown[]};
    expect(body.valid).toBeTruthy();
    expect(body.buffering ?? [], JSON.stringify(body.buffering)).toHaveLength(0);

    await api.dispose();
  });
});
