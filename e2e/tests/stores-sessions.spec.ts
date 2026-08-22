/**
 * Stores & sessions scenarios. See E2E_TESTBOOK.md ("Stores & sessions").
 *
 * The four E2E-STORE-* scenarios are unconditional -- they run whether or not
 * a real redis is available, so they must never touch a live backend.
 * `e2e-dead` (fixture-owned, url redis://127.0.0.1:1) and the UI-created
 * store below both point at a closed port for a fast refusal/timeout.
 *
 * The gated E2E-SESS-* scenarios below drive the `oidc-redis` route/policy
 * (openid-connect, session_storage: redis) against a real redis and skip
 * themselves when FEATHERBIT_TEST_REDIS_URL is unset.
 */
import {test, expect, request} from '@playwright/test';

import {GATEWAY_URL} from '../playwright.config';
import {adminApi, waitForDataPlane} from '../helpers/admin';

test.describe('Stores & sessions', () => {
  test('E2E-STORE-01: stores CRUD, raw placeholder text, duplicate-create conflict', async () => {
    const api = await adminApi();
    await api.delete('/api/stores/e2e-tmp');

    // GET /api/stores lists the two fixture stores with their RAW config --
    // ${FEATHERBIT_TEST_REDIS_URL:-...} must appear verbatim, never resolved.
    const stores = (await (await api.get('/api/stores')).json()) as {
      name: string;
      type: string;
      url: string;
    }[];
    const e2eRedis = stores.find((s) => s.name === 'e2e-redis');
    const e2eDead = stores.find((s) => s.name === 'e2e-dead');
    expect(e2eRedis, JSON.stringify(stores)).toBeTruthy();
    expect(e2eDead, JSON.stringify(stores)).toBeTruthy();
    expect(e2eRedis!.url).toBe('${FEATHERBIT_TEST_REDIS_URL:-redis://127.0.0.1:6379}');
    expect(e2eDead!.url).toBe('redis://127.0.0.1:1');

    // A duplicate create is rejected, not silently upserted.
    const dup = await api.post('/api/stores', {
      data: {name: 'e2e-redis', type: 'redis', url: 'redis://127.0.0.1:6379'},
    });
    expect(dup.status()).toBe(409);

    // A scratch store round-trips: PUT (upsert-create), GET, DELETE, then 404.
    const put = await api.put('/api/stores/e2e-tmp', {
      data: {name: 'e2e-tmp', type: 'redis', url: 'redis://127.0.0.1:6379'},
    });
    expect(put.ok(), await put.text()).toBeTruthy();
    const got = await api.get('/api/stores/e2e-tmp');
    expect(got.status()).toBe(200);
    const del = await api.delete('/api/stores/e2e-tmp');
    expect(del.ok(), await del.text()).toBeTruthy();
    const gone = await api.get('/api/stores/e2e-tmp');
    expect(gone.status()).toBe(404);

    await api.dispose();
  });

  test('E2E-STORE-02: deleting a store referenced by a plugin config is a 409 naming the referrer', async () => {
    const api = await adminApi();
    await api.delete('/api/plugin-configs/e2e-store-ref');

    const putRef = await api.put('/api/plugin-configs/e2e-store-ref', {
      data: {
        name: 'e2e-store-ref',
        type: 'limit-count',
        config: {count: 1, time_window: 60, policy: 'redis', store: 'e2e-redis'},
      },
    });
    expect(putRef.ok(), await putRef.text()).toBeTruthy();

    const del = await api.delete('/api/stores/e2e-redis');
    expect(del.status()).toBe(409);
    const body = (await del.json()) as {error: string; referrers: string[]};
    expect(body.error).toBe('in_use');
    expect(body.referrers).toContain("plugin_config 'e2e-store-ref'");

    // Cleanup: drop the referencing plugin config only. e2e-redis is
    // fixture-owned and must survive for every other test in the suite.
    await api.delete('/api/plugin-configs/e2e-store-ref');
    const stillThere = await api.get('/api/stores/e2e-redis');
    expect(stillThere.status()).toBe(200);

    await api.dispose();
  });

  test('E2E-STORE-03: create/ping/delete a store from the sidebar', async ({page}) => {
    const api = await adminApi();
    await api.delete('/api/stores/e2e-ui-store');

    await page.goto('/');
    await page.getByRole('button', {name: 'New store'}).click();
    await page.getByPlaceholder('sessions-redis').fill('e2e-ui-store');
    await page.getByPlaceholder('redis://127.0.0.1:6379').fill('redis://127.0.0.1:1');
    await page.getByRole('button', {name: 'Create store'}).click();

    // Creating a store auto-selects it, so its name renders twice: the
    // sidebar row AND the now-open panel's heading. `.first()` targets the
    // sidebar row (DOM order), which is also what `.hover()` needs below to
    // reveal the row's delete button.
    const row = page.getByText('e2e-ui-store', {exact: true}).first();
    await expect(row).toBeVisible();
    await row.click();

    await page.getByRole('button', {name: 'Ping store'}).click();
    await expect(page.getByText(/Timed out|refused|connect/i)).toBeVisible();

    await row.hover(); // the delete button is only revealed on hover
    await page.getByRole('button', {name: 'Delete store e2e-ui-store'}).click();
    await page.getByRole('button', {name: 'Delete', exact: true}).click();

    await expect(page.getByRole('button', {name: 'Delete store e2e-ui-store'})).toHaveCount(0);
    const gone = await api.get('/api/stores/e2e-ui-store');
    expect(gone.status()).toBe(404);

    await api.dispose();
  });

  test('E2E-STORE-04: the sessions endpoint validates its params without ever touching a backend', async () => {
    const api = await adminApi();

    const noStore = await api.get('/api/sessions');
    expect(noStore.status()).toBe(400);

    const unknownStore = await api.get('/api/sessions?store=nope');
    expect(unknownStore.status()).toBe(404);

    await api.dispose();
  });
});

/** The echo backend reports the request as it reached the upstream. */
type Echo = {method: string; path: string; headers: Record<string, string>};

/** The mock IdP always issues this subject by default (e2e/mock-idp/server.mjs). */
const SUBJECT = 'alice';

test.describe('Redis-backed sessions', () => {
  test.skip(!process.env.FEATHERBIT_TEST_REDIS_URL, 'FEATHERBIT_TEST_REDIS_URL not set');

  // Idempotent cleanup, both before (a prior failed run must not poison this
  // one) and after (this run must not leak into E2E-SESS-02's listing) each
  // test. A double-revoke of an already-gone subject is a no-op 200.
  const cleanup = async () => {
    const api = await adminApi();
    await api.delete(`/api/sessions?store=e2e-redis&subject=${SUBJECT}`);
    await api.dispose();
  };
  test.beforeEach(cleanup);
  test.afterEach(cleanup);

  test('E2E-SESS-01: interactive login establishes a bare-id redis session, replayed without the idp', async () => {
    // A plain API context: no browser needed, its cookie jar carries the flow
    // cookie and then the session cookie through every hop of the redirect
    // chain exactly like the interactive openid-connect.spec.ts scenarios do.
    const traffic = await request.newContext({baseURL: GATEWAY_URL});

    const first = await traffic.get('/oidc-redis/echo');
    expect(first.status()).toBe(200);
    const echo = (await first.json()) as Echo;
    expect(echo.path).toBe('/echo');

    const state = await traffic.storageState();
    const cookie = state.cookies.find((c) => c.name === 'oidc_redis_session');
    expect(cookie, 'the oidc_redis_session cookie must be set').toBeTruthy();
    // A bare 128-bit id, not a sealed blob -- cookie-mode's value would be far
    // longer and look nothing like plain hex.
    expect(cookie!.value).toMatch(/^[0-9a-f]{32}$/);

    // Replay: the same context's jar carries the id, and the gateway must
    // accept it from the store without a second round trip to the IdP.
    const second = await traffic.get('/oidc-redis/second-visit');
    expect(second.status()).toBe(200);
    const echo2 = (await second.json()) as Echo;
    expect(echo2.path).toBe('/second-visit');

    await traffic.dispose();
  });

  test('E2E-SESS-02: the admin listing surfaces subject/plugin/policy/route attribution, no payload', async () => {
    const traffic = await request.newContext({baseURL: GATEWAY_URL});
    await traffic.get('/oidc-redis/echo'); // establishes the session
    await traffic.dispose();

    const api = await adminApi();
    const res = await api.get('/api/sessions?store=e2e-redis');
    expect(res.status()).toBe(200);
    const body = (await res.json()) as {
      sessions: Record<string, unknown>[];
      next_cursor: string | null;
    };

    const session = body.sessions.find((s) => s.subject === SUBJECT);
    expect(session, JSON.stringify(body)).toBeTruthy();
    expect(session!.plugin).toBe('openid-connect');
    expect(session!.policy).toBe('oidc-redis-policy');
    expect(session!.route).toBe('oidc-redis');
    expect(session!.id).toMatch(/^[0-9a-f]{32}$/);
    expect(typeof session!.created_at).toBe('number');
    expect(typeof session!.expires_at).toBe('number');

    // Meta only -- the sealed payload (id_token/access_token/claims) never
    // leaves the store through this endpoint.
    expect(Object.keys(session!).sort()).toEqual(
      ['created_at', 'expires_at', 'id', 'plugin', 'policy', 'route', 'subject'].sort(),
    );

    await api.dispose();
  });

  // Browser. Only the panel's UI can drive a click-through revoke.
  test('E2E-SESS-03: revoking a session via the Sessions panel forces the data plane back into login', async ({
    page,
    context,
  }) => {
    // Log in through a real browser so the session cookie lands in this
    // context's jar, same choreography as E2E-OIDC-08.
    await page.goto(`${GATEWAY_URL}/oidc-redis/echo`);
    const body = await page.locator('body').innerText();
    const echo = JSON.parse(body) as Echo;
    expect(echo.path).toBe('/echo');

    const sessionCookie = (await context.cookies()).find((c) => c.name === 'oidc_redis_session');
    expect(sessionCookie, 'the oidc_redis_session cookie must be set').toBeTruthy();
    const sessionId = sessionCookie!.value;

    // Open the Sessions panel from the admin UI (footer button) and revoke
    // that exact row.
    await page.goto('/');
    await page.getByRole('button', {name: 'Sessions'}).click();
    await page.getByLabel('Session store').selectOption('e2e-redis');
    const revokeButton = page.getByRole('button', {name: `Revoke session ${sessionId}`});
    await expect(revokeButton).toBeVisible();
    await revokeButton.click();
    await expect(revokeButton).toHaveCount(0);

    // The data plane must now refuse the old cookie and re-enter login.
    const raw = await request.newContext({
      baseURL: GATEWAY_URL,
      maxRedirects: 0,
      extraHTTPHeaders: {cookie: `oidc_redis_session=${sessionId}`},
    });
    const result = await waitForDataPlane(raw, '/oidc-redis/echo', (status) => status === 302);
    expect(result.status).toBe(302);

    await raw.dispose();
  });
});
