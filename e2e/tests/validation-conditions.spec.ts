/**
 * `request-validation`'s `conditions` config key (boolean predicates over
 * headers/vars/JSONPath body queries, see `src/vars/mod.rs`'s `Expr`), and
 * the `ConditionBuilder` UI that edits it (`ui/src/components/
 * ConditionBuilder.tsx`, `ui/src/conditions.ts`). See E2E_TESTBOOK.md
 * ("Validation conditions").
 *
 * Idioms reused from var-suggestions.spec.ts / templates.spec.ts: build a
 * throwaway policy + route via the admin API (`PUT` policy, `POST` route),
 * drive real HTTP traffic, then delete both. `validate.denied -> client.in`
 * is the same graph semantic as `api-key.denied -> client.in`
 * (data-plane.spec.ts): the plugin sets `rejected_code` on the context and
 * exits its own `denied` port, so that edge is what carries the status to
 * the caller.
 */
import {test, expect, type Page} from '@playwright/test';

import {adminApi, dataPlane, deleteRouteIfPresent} from '../helpers/admin';

const POLICY = 'vc-policy';
const ROUTE = 'vc-route';

type Echo = {method: string; path: string; headers: Record<string, string>; body?: string};

/** Opens a route's policy on the canvas and waits for the graph to render (see editor.spec.ts). */
async function openRoute(page: Page, route: string) {
  await page.goto('/');
  await page.getByText(route, {exact: true}).click();
  await page.waitForSelector('.react-flow__node');
}

/**
 * Saves the policy. The inspector overlay sits on top of the toolbar, so
 * close it first if one is open (see editor-roundtrip.spec.ts).
 */
async function save(page: Page) {
  const close = page.getByRole('button', {name: 'Close'});
  if (await close.isVisible().catch(() => false)) await close.click();
  await page.getByRole('button', {name: 'Save Policy'}).click();
}

/**
 * The seed policy payload: listener -> validate (request-validation,
 * conditions) -> echo backend -> client. conditions: authorization present
 * AND a Bearer scheme AND (an email OR a non-null id) -- mirrors
 * request_validation.rs's own test_request_validation_conditions_accept
 * fixture. A fresh object per call, since E2E-VC-06 mutates its own copy of
 * the `conditions` array in the browser -- reusing one shared object here
 * would risk aliasing bugs if a future edit mutated it in place.
 */
function seedPolicy() {
  return {
    name: POLICY,
    nodes: [
      {id: 'listener', type: 'listener', config: {}},
      {
        id: 'validate',
        type: 'request-validation',
        config: {
          rejected_code: 401,
          conditions: [
            ['http_authorization', 'present'],
            ['http_authorization', 'contains', 'Bearer'],
            ['OR', ['$.user.email', 'present'], ['NOT', ['$.user.id', 'is_null']]],
          ],
        },
      },
      {id: 'echo-backend', type: 'upstream', config: {targets: [{host: '127.0.0.1', port: 3010}]}},
      {id: 'client', type: 'client'},
    ],
    edges: [
      {from: 'listener.out', to: 'validate.in'},
      {from: 'validate.success', to: 'echo-backend.in'},
      {from: 'validate.denied', to: 'client.in'},
      {from: 'echo-backend.success', to: 'client.in'},
    ],
  };
}

test.describe('Validation conditions', () => {
  test.beforeAll(async () => {
    const api = await adminApi();
    await deleteRouteIfPresent(api, ROUTE);
    await api.delete(`/api/policies/${POLICY}`);

    const policyRes = await api.put(`/api/policies/${POLICY}`, {data: seedPolicy()});
    expect(policyRes.ok(), `policy save failed: ${policyRes.status()} ${await policyRes.text()}`).toBeTruthy();

    const routeRes = await api.post('/api/routes', {
      data: {name: ROUTE, match: {path: '/conditions/*', methods: ['POST']}, policy: POLICY},
    });
    expect(routeRes.ok(), `route save failed: ${routeRes.status()} ${await routeRes.text()}`).toBeTruthy();

    await api.dispose();
  });

  // E2E-VC-06 mutates vc-policy's `contains` rule (Bearer -> Token) through the
  // UI. Reset to the seed before every test -- as editor-roundtrip.spec.ts
  // does for its own shared policy -- so the suite is order-independent and,
  // critically, so a retried E2E-VC-06 (CI's `retries: 1`) doesn't re-enter
  // with the builder already showing 'Token' and fail its own `toHaveValue('Bearer')`
  // precondition with a misleading error.
  test.beforeEach(async () => {
    const api = await adminApi();
    await api.put(`/api/policies/${POLICY}`, {data: seedPolicy()});
    await api.dispose();
  });

  test.afterAll(async () => {
    const api = await adminApi();
    await api.delete(`/api/routes/${ROUTE}`);
    await api.delete(`/api/policies/${POLICY}`);
    await api.dispose();
  });

  test('E2E-VC-01: bearer auth with a present email (one OR arm) reaches the upstream', async () => {
    const traffic = await dataPlane();
    const res = await traffic.post('/conditions/thing', {
      headers: {authorization: 'Bearer tok'},
      data: {user: {email: 'a@b.c'}},
    });

    expect(res.status()).toBe(200);
    const echo = (await res.json()) as Echo;
    expect(echo.path).toBe('/conditions/thing');
    await traffic.dispose();
  });

  test('E2E-VC-02: no authorization header is rejected with the plugin\'s own 401 and message', async () => {
    const traffic = await dataPlane();
    const res = await traffic.post('/conditions/thing', {data: {}});

    expect(res.status()).toBe(401);
    expect(await res.json()).toEqual({
      error: 'validation_failed',
      message: 'request conditions not satisfied',
    });
    await traffic.dispose();
  });

  test('E2E-VC-03: a non-Bearer scheme fails the contains rule', async () => {
    const traffic = await dataPlane();
    const res = await traffic.post('/conditions/thing', {
      headers: {authorization: 'Basic xyz'},
      data: {user: {email: 'a@b.c'}},
    });

    expect(res.status()).toBe(401);
    await traffic.dispose();
  });

  test('E2E-VC-04: bearer auth but neither OR arm holds (absent email, null id) is rejected', async () => {
    const traffic = await dataPlane();
    const res = await traffic.post('/conditions/thing', {
      headers: {authorization: 'Bearer tok'},
      data: {user: {id: null}},
    });

    expect(res.status()).toBe(401);
    await traffic.dispose();
  });

  test('E2E-VC-05: bearer auth with a present, non-null id (NOT is_null) reaches the upstream', async () => {
    const traffic = await dataPlane();
    const res = await traffic.post('/conditions/thing', {
      headers: {authorization: 'Bearer tok'},
      data: {user: {id: 7}},
    });

    expect(res.status()).toBe(200);
    const echo = (await res.json()) as Echo;
    expect(echo.path).toBe('/conditions/thing');
    await traffic.dispose();
  });

  /**
   * The ConditionBuilder half. Per conditions.ts's fromExpr/parseNode, the
   * saved expression parses into a root AND group with exactly 3 top-level
   * children: the two flat rules (present, contains) and one nested OR group
   * (whose own present/NOT-is_null children render one level deeper). Only
   * the 'contains' rule has a scalar operand -- the two 'present' rules and
   * the NOT-wrapped 'is_null' rule are all unary -- so exactly one "Condition
   * value" input exists anywhere in the tree, which is what this test edits.
   *
   * data-testid="condition-node" / data-depth on RuleRow and GroupCard (added
   * in this task, ConditionBuilder.tsx) is what makes "top-level" assertable:
   * depth is each node's own path length, so depth=1 is exactly the root's
   * direct children -- unambiguous even though the nested OR/NOT groups add
   * more rule/group nodes deeper in the same tree.
   */
  test('E2E-VC-06: editing the contains rule\'s value in the builder changes live traffic', async ({page}) => {
    await openRoute(page, ROUTE);
    await page.locator('.react-flow__node', {hasText: 'validate'}).first().click();

    const topLevel = page.locator('[data-testid="condition-node"][data-depth="1"]');
    await expect(topLevel).toHaveCount(3);

    const valueInput = page.getByLabel('Condition value');
    await expect(valueInput).toHaveCount(1);
    await expect(valueInput).toHaveValue('Bearer');
    await valueInput.fill('Token');

    await save(page);
    await expect(page.getByText('Policy saved')).toBeVisible();

    const traffic = await dataPlane();
    const passingBody = {user: {email: 'a@b.c'}};

    // Hot-swap is quick but not synchronous with the save response (see
    // helpers/admin.ts's waitForDataPlane doc); that helper only speaks GET,
    // so poll the POST directly instead.
    await expect
      .poll(
        async () => {
          const res = await traffic.post('/conditions/thing', {
            headers: {authorization: 'Bearer tok'},
            data: passingBody,
          });
          return res.status();
        },
        {timeout: 10_000},
      )
      .toBe(401);

    const nowPasses = await traffic.post('/conditions/thing', {
      headers: {authorization: 'Token tok'},
      data: passingBody,
    });
    expect(nowPasses.status()).toBe(200);
    const echo = (await nowPasses.json()) as Echo;
    expect(echo.path).toBe('/conditions/thing');

    await traffic.dispose();
  });
});
