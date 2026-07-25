/**
 * Data-plane scenarios: what an ordinary HTTP client sees on :18081.
 * See E2E_TESTBOOK.md ("Data plane").
 */
import {test, expect} from '@playwright/test';

import {adminApi, dataPlane} from '../helpers/admin';

/** The echo backend answers with the request as it arrived at the upstream. */
type Echo = {method: string; path: string; headers: Record<string, string>; body?: string};

test.describe('Data plane', () => {
  test('E2E-DP-01: proxies to the upstream with the path prefix stripped', async () => {
    const traffic = await dataPlane();
    const res = await traffic.get('/api/hello');

    expect(res.status()).toBe(200);
    const echo = (await res.json()) as Echo;
    // proxy-rewrite stripped /api: the backend saw /hello, not /api/hello.
    expect(echo.path).toBe('/hello');
    await traffic.dispose();
  });

  test('E2E-DP-02: an unmatched path is not routed', async () => {
    const traffic = await dataPlane();
    expect((await traffic.get('/nope')).status()).toBe(404);
    await traffic.dispose();
  });

  test('E2E-DP-03: a method outside the route\'s methods is not routed', async () => {
    const traffic = await dataPlane();
    // secure-api matches GET only.
    const res = await traffic.post('/secure/thing', {data: {}});
    expect(res.status()).toBe(404);
    await traffic.dispose();
  });

  test('E2E-DP-04: method and body reach the upstream intact', async () => {
    const traffic = await dataPlane();
    const res = await traffic.post('/api/submit', {
      data: {hello: 'world'},
      headers: {'content-type': 'application/json'},
    });

    expect(res.status()).toBe(200);
    const echo = (await res.json()) as Echo;
    expect(echo.method).toBe('POST');
    expect(echo.path).toBe('/submit');
    expect(echo.body).toContain('world');
    await traffic.dispose();
  });

  /**
   * key-auth sets 401 on the context and then returns an error; the status only
   * reaches the client because secure-policy wires `api-key.error -> client.in`.
   * This is the scenario that pins that graph semantic.
   */
  test('E2E-DP-05: a missing API key is rejected with the plugin\'s own 401', async () => {
    const traffic = await dataPlane();
    const res = await traffic.get('/secure/data');

    expect(res.status()).toBe(401);
    expect(await res.text()).toContain('unauthorized');
    await traffic.dispose();
  });

  test('E2E-DP-06: a valid API key is proxied through', async () => {
    const traffic = await dataPlane();
    const res = await traffic.get('/secure/data', {headers: {'x-api-key': 'e2e-secret'}});

    expect(res.status()).toBe(200);
    const echo = (await res.json()) as Echo;
    expect(echo.path).toBe('/data');
    await traffic.dispose();
  });

  test('E2E-DP-07: exceeding the burst allowance is throttled with 429', async () => {
    const traffic = await dataPlane();

    // rate-limit: 2 rps, burst 2. Fire enough to exhaust the bucket; the first
    // few may pass, but a 429 must appear.
    const statuses: number[] = [];
    for (let i = 0; i < 8; i++) {
      const res = await traffic.get('/secure/burst', {headers: {'x-api-key': 'e2e-secret'}});
      statuses.push(res.status());
    }

    expect(statuses).toContain(429);
    await traffic.dispose();
  });

  test('E2E-DP-08: a dead upstream renders the error-handler, not a raw 500', async () => {
    const traffic = await dataPlane();
    const res = await traffic.get('/dead/anything');

    expect(res.status()).toBe(502);
    const body = await res.json();
    // The error-handler's template rendered, with the real error code substituted.
    expect(body).toHaveProperty('error');
    expect(body).toHaveProperty('message');
    expect(JSON.stringify(body)).not.toContain('{{');
    await traffic.dispose();
  });

  /**
   * KNOWN BUG -- marked `fail`, so the suite stays green while the bug stands and
   * turns red the day it is fixed (that is the signal to delete this annotation).
   *
   * `cors` writes 204 onto the context for a preflight and returns Ok. But writing
   * a response to the context does not stop the graph: the engine follows the
   * success edge to the next node, `upstream` proxies the OPTIONS anyway, and the
   * backend's answer overwrites the 204. The plugin's unit tests pass because they
   * only inspect the context; end-to-end, a preflight never gets its 204.
   *
   * There is no way to wire around it either: expressing "terminate here, but only
   * for OPTIONS" needs either a conditional/second output port on the node or an
   * engine-level terminal signal. Neither exists today.
   */
  test.fail('E2E-DP-09: CORS preflight is short-circuited with 204', async () => {
    const traffic = await dataPlane();
    const res = await traffic.fetch('/api/hello', {
      method: 'OPTIONS',
      headers: {
        origin: 'https://app.example.com',
        'access-control-request-method': 'GET',
      },
    });

    expect(res.status()).toBe(204);
    expect(res.headers()['access-control-allow-origin']).toBe('https://app.example.com');
    await traffic.dispose();
  });

  test('E2E-DP-10: requests are counted in the Prometheus metrics', async () => {
    const traffic = await dataPlane();
    const api = await adminApi();

    await traffic.get('/api/metric-me');
    const metrics = await (await api.get('/metrics')).text();

    // Per-route counters are exported; the route we just hit must appear.
    expect(metrics).toContain('echo-api');

    await traffic.dispose();
    await api.dispose();
  });
});
