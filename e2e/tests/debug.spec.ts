/**
 * Debug-mode scenarios: per-request policy tracing and the plugin sandbox.
 * See E2E_TESTBOOK.md ("Debug & sandbox").
 *
 * The fixture enables debug mode for the whole run, so these specs also rely on
 * every *other* spec proving the feature stays inert for untraced traffic.
 */
import {test, expect} from '@playwright/test';

import {ADMIN_URL} from '../playwright.config';
import {adminApi, dataPlane} from '../helpers/admin';

/** A genuinely anonymous request (Playwright contexts inherit httpCredentials). */
const anonymous = (path: string) => fetch(`${ADMIN_URL}${path}`, {redirect: 'manual'});

/** Header that opts a single request into tracing. */
const DEBUG_HEADER = {'x-featherbit-debug': '1'};

interface Step {
  node_id: string;
  node_type: string;
  edge: string;
  outcome: {kind: string; code?: string};
  changes: {path: string; kind: string; before?: string; after?: string}[];
  after: {
    request: {path: string; headers: Record<string, string[]>; body: {len: number; text?: string}};
    response: {status_code: number};
  };
}
interface TraceDetail {
  id: string;
  route?: string;
  policy: string;
  status: number;
  source: string;
  steps: Step[];
  initial: {request: {headers: Record<string, string[]>}};
}

test.describe('Debug mode', () => {
  test('E2E-DEBUG-01: the debug API requires authentication', async () => {
    expect((await anonymous('/api/debug/config')).status).toBe(401);
    expect((await anonymous('/api/debug/traces')).status).toBe(401);
  });

  test('E2E-DEBUG-02: config reports debug enabled and the trigger header', async () => {
    const api = await adminApi();
    const cfg = await (await api.get('/api/debug/config')).json();

    expect(cfg.enabled).toBe(true);
    expect(cfg.trigger_header).toBe('x-featherbit-debug');
    // Bodies are the costly part and must stay off unless asked for.
    expect(cfg.capture_bodies).toBe(false);
    await api.dispose();
  });

  /**
   * The whole point of opt-in tracing: ordinary traffic must not be recorded.
   */
  test('E2E-DEBUG-03: a request without the trigger header is not traced', async () => {
    const api = await adminApi();
    await api.delete('/api/debug/traces');

    const gw = await dataPlane();
    expect((await gw.get('/api/hello')).status()).toBe(200);

    const {traces} = await (await api.get('/api/debug/traces')).json();
    expect(traces).toHaveLength(0);

    await gw.dispose();
    await api.dispose();
  });

  test('E2E-DEBUG-04: an opted-in request is traced and reports its trace id', async () => {
    const api = await adminApi();
    await api.delete('/api/debug/traces');

    const gw = await dataPlane();
    const untraced = await gw.get('/api/hello');
    const traced = await gw.get('/api/hello', {headers: DEBUG_HEADER});

    // Tracing must not change routing or the response the caller gets. The
    // bodies are NOT byte-identical here, and that is expected: the trigger
    // header is deliberately left on the request (stripping it would make the
    // traced request differ from the one being debugged), so this echo backend
    // reflects it back. Assert on what tracing must not perturb instead.
    expect(traced.status()).toBe(untraced.status());
    const tracedBody = JSON.parse(await traced.text());
    const untracedBody = JSON.parse(await untraced.text());
    expect(tracedBody.path).toBe(untracedBody.path);
    expect(tracedBody.method).toBe(untracedBody.method);
    // ...and the trigger header does reach the upstream, which is worth pinning.
    expect(tracedBody.headers['x-featherbit-debug']).toBe('1');
    expect(untracedBody.headers['x-featherbit-debug']).toBeUndefined();

    const id = traced.headers()['x-featherbit-trace-id'];
    expect(id, 'traced response should carry the trace id').toBeTruthy();
    // ...and the untraced one must not leak that debug mode exists.
    expect(untraced.headers()['x-featherbit-trace-id']).toBeUndefined();

    const {traces} = await (await api.get('/api/debug/traces')).json();
    expect(traces).toHaveLength(1);
    expect(traces[0].id).toBe(id);
    expect(traces[0].route).toBe('echo-api');
    expect(traces[0].policy).toBe('echo-policy');
    expect(traces[0].source).toBe('request');
    expect(traces[0].status).toBe(200);

    await gw.dispose();
    await api.dispose();
  });

  /**
   * The feature's reason for existing: seeing which node changed what. The
   * echo policy strips `/api`, so that rewrite must be attributable to the
   * `strip-prefix` node specifically -- not just visible in the final result.
   */
  test('E2E-DEBUG-05: the trace attributes each change to the node that made it', async () => {
    const api = await adminApi();
    await api.delete('/api/debug/traces');

    const gw = await dataPlane();
    const res = await gw.get('/api/hello', {headers: DEBUG_HEADER});
    const id = res.headers()['x-featherbit-trace-id'];

    const trace: TraceDetail = await (await api.get(`/api/debug/traces/${id}`)).json();

    // Nodes appear in graph order, listener excluded (it is the entry point).
    const ids = trace.steps.map((s) => s.node_id);
    expect(ids).toEqual(['cors', 'strip-prefix', 'echo-backend', 'client']);
    expect(trace.steps[0].node_type).toBe('cors');

    const rewrite = trace.steps.find((s) => s.node_id === 'strip-prefix')!;
    const pathChange = rewrite.changes.find((c) => c.path === 'request.path');
    expect(pathChange, 'strip-prefix should report a request.path change').toBeTruthy();
    expect(pathChange!.before).toBe('/api/hello');
    expect(pathChange!.after).toBe('/hello');
    expect(rewrite.after.request.path).toBe('/hello');

    // The terminal client node ends the walk.
    expect(trace.steps[trace.steps.length - 1].edge).toBe('terminal');

    await gw.dispose();
    await api.dispose();
  });

  test('E2E-DEBUG-06: sensitive headers are redacted in the stored trace', async () => {
    const api = await adminApi();
    await api.delete('/api/debug/traces');

    const gw = await dataPlane();
    const res = await gw.get('/secure/hello', {
      headers: {...DEBUG_HEADER, 'x-api-key': 'e2e-secret', authorization: 'Bearer topsecret'},
    });
    const id = res.headers()['x-featherbit-trace-id'];
    const trace: TraceDetail = await (await api.get(`/api/debug/traces/${id}`)).json();

    const raw = JSON.stringify(trace);
    expect(raw, 'the api key must never reach the trace buffer').not.toContain('e2e-secret');
    expect(raw, 'the bearer token must never reach the trace buffer').not.toContain('topsecret');
    expect(trace.initial.request.headers['x-api-key']).toEqual(['<redacted>']);
    expect(trace.initial.request.headers['authorization']).toEqual(['<redacted>']);

    await gw.dispose();
    await api.dispose();
  });

  test('E2E-DEBUG-07: bodies are sized but not captured by default', async () => {
    const api = await adminApi();
    await api.delete('/api/debug/traces');

    const gw = await dataPlane();
    const res = await gw.post('/api/echo', {
      headers: {...DEBUG_HEADER, 'content-type': 'application/json'},
      data: {secret: 'do-not-capture'},
    });
    const id = res.headers()['x-featherbit-trace-id'];
    const trace: TraceDetail = await (await api.get(`/api/debug/traces/${id}`)).json();

    const first = trace.steps[0];
    expect(first.after.request.body.len).toBeGreaterThan(0);
    expect(first.after.request.body.text).toBeUndefined();
    expect(JSON.stringify(trace)).not.toContain('do-not-capture');

    await gw.dispose();
    await api.dispose();
  });

  /**
   * An upstream that refuses connections exercises the error path: the failing
   * node routes through its `error` edge to the error-handler.
   */
  test('E2E-DEBUG-08: an error path records the failing node and its edge', async () => {
    const api = await adminApi();
    await api.delete('/api/debug/traces');

    const gw = await dataPlane();
    const res = await gw.get('/dead/x', {headers: DEBUG_HEADER});
    expect(res.status()).toBe(502);

    const id = res.headers()['x-featherbit-trace-id'];
    const trace: TraceDetail = await (await api.get(`/api/debug/traces/${id}`)).json();

    const failed = trace.steps.find((s) => s.outcome.kind === 'error');
    expect(failed, 'the upstream failure should be recorded').toBeTruthy();
    expect(failed!.edge).toBe('error');
    expect(trace.status).toBe(502);

    await gw.dispose();
    await api.dispose();
  });

  test('E2E-DEBUG-09: the buffer is bounded and clearable', async () => {
    const api = await adminApi();
    await api.delete('/api/debug/traces');

    const gw = await dataPlane();
    const cfg = await (await api.get('/api/debug/config')).json();
    const cap: number = cfg.max_traces;

    for (let i = 0; i < cap + 3; i++) {
      await gw.get(`/api/hello?i=${i}`, {headers: DEBUG_HEADER});
    }
    const {traces} = await (await api.get('/api/debug/traces')).json();
    expect(traces.length).toBe(cap);

    const cleared = await api.delete('/api/debug/traces');
    expect(cleared.status()).toBe(200);
    const after = await (await api.get('/api/debug/traces')).json();
    expect(after.traces).toHaveLength(0);

    await gw.dispose();
    await api.dispose();
  });

  /**
   * A request that matches no route is still an incoming request. It is
   * captured with the `(no route matched)` policy label so it shows up in the
   * panel — the fix for "I sent a request but nothing appeared" on an unrouted
   * path.
   */
  test('E2E-DEBUG-09b: an unrouted request (404) is still captured', async () => {
    const api = await adminApi();
    await api.delete('/api/debug/traces');

    const gw = await dataPlane();
    const res = await gw.get('/no-such-route', {headers: DEBUG_HEADER});
    expect(res.status()).toBe(404);
    // It reports a trace id like any other traced request.
    expect(res.headers()['x-featherbit-trace-id']).toBeTruthy();

    const {traces} = await (await api.get('/api/debug/traces')).json();
    expect(traces).toHaveLength(1);
    expect(traces[0].status).toBe(404);
    expect(traces[0].policy).toBe('(no route matched)');
    expect(traces[0].route).toBeUndefined();
    // No policy ran, so there are no steps — but the request is visible.
    expect(traces[0].step_count).toBe(0);

    await gw.dispose();
    await api.dispose();
  });

  test('E2E-DEBUG-10: an unknown trace id is a 404', async () => {
    const api = await adminApi();
    expect((await api.get('/api/debug/traces/no-such-trace')).status()).toBe(404);
    await api.dispose();
  });

  /**
   * Browse recent traffic and narrow it to one policy/route — the "select a
   * recent request to debug" flow, without pre-flagging with the header.
   */
  test('E2E-DEBUG-11b: the trace list filters by route, policy and status', async () => {
    const api = await adminApi();
    await api.delete('/api/debug/traces');

    const gw = await dataPlane();
    await gw.get('/api/hello', {headers: DEBUG_HEADER}); // echo-api / echo-policy / 200
    await gw.get('/dead/x', {headers: DEBUG_HEADER}); // dead-api / dead-policy / 502

    const byRoute = await (await api.get('/api/debug/traces?route=echo-api')).json();
    expect(byRoute.traces).toHaveLength(1);
    expect(byRoute.traces[0].route).toBe('echo-api');

    const byPolicy = await (await api.get('/api/debug/traces?policy=dead-policy')).json();
    expect(byPolicy.traces).toHaveLength(1);
    expect(byPolicy.traces[0].policy).toBe('dead-policy');

    const byStatus = await (await api.get('/api/debug/traces?status=502')).json();
    expect(byStatus.traces).toHaveLength(1);
    expect(byStatus.traces[0].status).toBe(502);

    // A bare empty filter is not a filter.
    const all = await (await api.get('/api/debug/traces?route=')).json();
    expect(all.traces.length).toBe(2);

    await gw.dispose();
    await api.dispose();
  });
});

test.describe('Plugin sandbox', () => {
  test('E2E-DEBUG-11: replays a named policy against a synthetic context', async () => {
    const api = await adminApi();
    const res = await api.post('/api/debug/sandbox', {
      data: {policy: 'echo-policy', context: {path: '/api/hello'}},
    });
    expect(res.status()).toBe(200);
    const body = await res.json();

    expect(body.mode).toBe('policy');
    expect(body.warning).toContain('for real');

    const ids = (body.trace.steps as Step[]).map((s) => s.node_id);
    expect(ids).toEqual(['cors', 'strip-prefix', 'echo-backend', 'client']);
    // The upstream really was called: the echo backend answered.
    expect(body.trace.status).toBe(200);
    expect(body.trace.source).toBe('sandbox');

    await api.dispose();
  });

  test('E2E-DEBUG-12: runs an ad-hoc node list', async () => {
    const api = await adminApi();
    const res = await api.post('/api/debug/sandbox', {
      data: {
        nodes: [
          {
            id: 'rw',
            type: 'proxy-rewrite',
            config: {phase: 'request', strip_path_prefix: '/api'},
          },
        ],
        context: {path: '/api/hello'},
      },
    });
    expect(res.status()).toBe(200);
    const body = await res.json();

    expect(body.mode).toBe('nodes');
    const rw = (body.trace.steps as Step[]).find((s) => s.node_id === 'rw')!;
    expect(rw.changes.find((c) => c.path === 'request.path')?.after).toBe('/hello');

    await api.dispose();
  });

  /** Posting `{}` for the context must still produce a runnable request. */
  test('E2E-DEBUG-13: the synthetic context is fully defaulted', async () => {
    const api = await adminApi();
    const res = await api.post('/api/debug/sandbox', {
      data: {nodes: [{id: 'c', type: 'cors', config: {}}]},
    });
    expect(res.status()).toBe(200);
    const body = await res.json();
    const step = (body.trace.steps as Step[])[0];
    expect(step.node_id).toBe('c');
    await api.dispose();
  });

  test('E2E-DEBUG-14: an invalid plugin config is a 400 naming the problem', async () => {
    const api = await adminApi();
    const res = await api.post('/api/debug/sandbox', {
      data: {nodes: [{id: 'x', type: 'no-such-plugin', config: {}}]},
    });
    expect(res.status()).toBe(400);
    expect(JSON.stringify(await res.json())).toContain('no-such-plugin');
    await api.dispose();
  });

  test('E2E-DEBUG-15: an unknown policy is a 404', async () => {
    const api = await adminApi();
    const res = await api.post('/api/debug/sandbox', {data: {policy: 'nope'}});
    expect(res.status()).toBe(404);
    await api.dispose();
  });

  test('E2E-DEBUG-16: exactly one of nodes/policy is required', async () => {
    const api = await adminApi();
    expect((await api.post('/api/debug/sandbox', {data: {}})).status()).toBe(400);
    expect(
      (await api.post('/api/debug/sandbox', {data: {policy: 'echo-policy', nodes: []}})).status(),
    ).toBe(400);
    await api.dispose();
  });
});
