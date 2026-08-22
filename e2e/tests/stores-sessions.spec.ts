/**
 * Stores & sessions scenarios. See E2E_TESTBOOK.md ("Stores & sessions").
 *
 * These four scenarios are unconditional -- they run whether or not a real
 * redis is available, so they must never touch a live backend. `e2e-dead`
 * (fixture-owned, url redis://127.0.0.1:1) and the UI-created store below
 * both point at a closed port for a fast refusal/timeout. The gated
 * E2E-SESS-* scenarios that exercise a real redis live in a separate spec
 * added later and skip themselves when FEATHERBIT_TEST_REDIS_URL is unset.
 */
import {test, expect} from '@playwright/test';

import {adminApi} from '../helpers/admin';

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
