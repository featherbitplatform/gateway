/**
 * The editor save round-trip: what you build on the canvas is what gets saved.
 * See E2E_TESTBOOK.md ("Editor round-trip").
 *
 * The existing editor tests prove the UI *renders*. These prove the other
 * direction -- edit the graph, click Save Policy, and read the policy back
 * through the admin API -- which is where editor bugs actually live: an edge
 * serialized to the wrong port, a switch stored as a string, a deleted node that
 * lingers. All of them mutate the throwaway `rt-policy`, never a shared route.
 */
import {test, expect, type Page, type APIRequestContext} from '@playwright/test';

import {adminApi, dataPlane} from '../helpers/admin';

type PolicyNode = {id: string; type: string; config?: Record<string, unknown>};
type PolicyEdge = {from: string; to: string};
type Policy = {nodes: PolicyNode[]; edges: PolicyEdge[]};

async function getPolicy(api: APIRequestContext, name: string): Promise<Policy> {
  return (await api.get(`/api/policies/${name}`)).json() as Promise<Policy>;
}

/** Opens rt-api on the canvas and waits for the graph. */
async function openRt(page: Page) {
  await page.goto('/');
  await page.getByText('rt-api', {exact: true}).click();
  await page.waitForSelector('.react-flow__node');
}

/**
 * Saves the policy. The inspector/drawer overlays the toolbar, so close it first
 * -- but only if one is open. A short timeout matters: without it, clicking a
 * Close that isn't there blocks until the whole test times out.
 */
async function save(page: Page) {
  const close = page.getByRole('button', {name: 'Close'});
  if (await close.isVisible().catch(() => false)) await close.click();
  await page.getByRole('button', {name: 'Save Policy'}).click();
}

test.describe('Editor round-trip', () => {
  // Every test mutates a policy, so reset the ones they touch to the seed before
  // each -- the tests are then independent of order and of each other's leftovers
  // (admin writes are in-memory and the gateway is reused across local runs).
  const seeds: Record<string, Policy> = {};

  test.beforeAll(async () => {
    const api = await adminApi();
    for (const name of ['rt-policy', 'uiauth-policy']) {
      seeds[name] = await getPolicy(api, name);
    }
    await api.dispose();
  });

  test.beforeEach(async () => {
    const api = await adminApi();
    for (const [name, policy] of Object.entries(seeds)) {
      await api.put(`/api/policies/${name}`, {data: policy});
    }
    await api.dispose();
  });

  test('E2E-UI-09: a number field is saved as a JSON number, not a string', async ({page}) => {
    const api = await adminApi();
    await openRt(page);

    await page.locator('.react-flow__node', {hasText: 'error-handler'}).first().click();
    const statusField = page.locator('input[type="number"]').first();
    await expect(statusField).toBeVisible();
    await statusField.fill('507');
    await save(page);

    const policy = await getPolicy(api, 'rt-policy');
    const handler = policy.nodes.find((n) => n.id === 'error-handler')!;
    // The bug this guards: a form value stored as "507" would break the policy.
    expect(handler.config!.status_code).toBe(507);
    expect(typeof handler.config!.status_code).toBe('number');

    await api.dispose();
  });

  test('E2E-UI-10: a switch is saved as a JSON boolean', async ({page}) => {
    const api = await adminApi();
    await openRt(page);

    // The seeded cors node has an "allow credentials" switch; toggle it on.
    await page.locator('.react-flow__node', {hasText: 'auth'}).first().click();
    // The switch is a button[role=switch] whose visible label is the switchLabel.
    const toggle = page.locator('button[role="switch"]').filter({hasText: 'Allow credentials'});
    await expect(toggle).toBeVisible();
    await expect(toggle).toHaveAttribute('aria-checked', 'false');
    await toggle.click();
    await expect(toggle).toHaveAttribute('aria-checked', 'true');
    await save(page);

    const policy = await getPolicy(api, 'rt-policy');
    const cors = policy.nodes.find((n) => n.id === 'auth')!;
    // The bug this guards: a switch stored as the string "true" would break config.
    expect(cors.config!.allow_credentials).toBe(true);
    expect(typeof cors.config!.allow_credentials).toBe('boolean');

    await api.dispose();
  });

  /**
   * Delete a node in the inspector, Save, and confirm the node AND its edges are
   * gone from the policy. error-handler is the safe choice: it is a target of two
   * error edges, so removing it (and its edges) leaves every other node still
   * connected, so the save is valid.
   *
   * (Node *creation* by dragging a wire between ports is not automated here:
   * ReactFlow v12 drives connections off pointer-event hit-testing that Playwright
   * cannot reliably reproduce headless. Deletion exercises the same UI→policy
   * serialization path in the reliable direction.)
   */
  test('E2E-UI-11: deleting a node removes it and its edges from the policy', async ({page}) => {
    const api = await adminApi();
    await openRt(page);
    const before = await getPolicy(api, 'rt-policy');
    expect(before.nodes.some((n) => n.id === 'error-handler')).toBe(true);
    expect(before.edges.some((e) => e.to.startsWith('error-handler'))).toBe(true);

    await page.locator('.react-flow__node', {hasText: 'error-handler'}).first().click();
    await page.getByRole('button', {name: 'Delete Node'}).click();
    await save(page);

    const after = await getPolicy(api, 'rt-policy');
    expect(after.nodes.some((n) => n.id === 'error-handler')).toBe(false);
    // Every edge that touched the node is gone too -- no dangling references.
    expect(after.edges.some((e) => e.from.startsWith('error-handler') || e.to.startsWith('error-handler'))).toBe(false);

    await api.dispose();
  });

  test('E2E-UI-12: saving an unchanged graph preserves nodes and edges (round-trip fidelity)', async ({page}) => {
    const api = await adminApi();
    // Read the current state fresh, so this is order-independent.
    const seed = await getPolicy(api, 'rt-policy');

    await openRt(page);
    await save(page); // no edits -- a pure serialize -> deserialize cycle

    const after = await getPolicy(api, 'rt-policy');

    const ids = (p: Policy) => p.nodes.map((n) => n.id).sort();
    expect(ids(after)).toEqual(ids(seed));

    // Compare edges semantically. The listener's port is `out` in hand-written
    // YAML but the UI serializes it as `success`; the engine treats the two as
    // the same success edge (engine.rs: "`success` and `out` become success
    // edges"), so normalize before comparing -- the round-trip is faithful, just
    // not byte-identical on that one port name.
    const edgeKey = (e: PolicyEdge) => `${e.from.replace(/\.out$/, '.success')}->${e.to}`;
    expect(after.edges.map(edgeKey).sort()).toEqual(seed.edges.map(edgeKey).sort());

    await api.dispose();
  });

  /**
   * The definitive proof of the basic-auth users-shape fix, driven through the
   * ACTUAL editor form. Add a user to a basic-auth node via the node inspector's
   * repeating-object field, Save, and confirm the credential both lands in the
   * policy (as the array shape the UI emits) AND authenticates real traffic.
   *
   * Before the fix, the form emitted `users: [{username, password}]`, the plugin
   * parsed only a map, and the credential was silently dropped -- so step (c)
   * below would return 401.
   */
  test('E2E-UI-13: a user added via the editor form authenticates real traffic', async ({page}) => {
    const api = await adminApi();
    const traffic = await dataPlane();

    // Open the wired basic-auth route and select the node.
    await page.goto('/');
    await page.getByText('uiauth-api', {exact: true}).click();
    await page.waitForSelector('.react-flow__node');
    await page.locator('.react-flow__node', {hasText: 'auth'}).first().click();

    // (a) Add a user through the repeating-object "users" field.
    await page.getByRole('button', {name: 'Add User'}).click();
    await page.locator('label:text-is("Username") + input').fill('alice');
    await page.locator('label:text-is("Password") + input').fill('secret');
    await save(page);

    // (b) The credential is in the saved policy, in the UI's array shape.
    const policy = await getPolicy(api, 'uiauth-policy');
    const auth = policy.nodes.find((n) => n.id === 'auth')!;
    expect(auth.config!.users).toEqual([{username: 'alice', password: 'secret'}]);

    // (c) And it actually authenticates -- the proof the array shape is parsed.
    const basic = (u: string, p: string) => ({
      authorization: 'Basic ' + Buffer.from(`${u}:${p}`).toString('base64'),
    });
    expect((await traffic.get('/uiauth/thing', {headers: basic('alice', 'secret')})).status()).toBe(200);
    expect((await traffic.get('/uiauth/thing', {headers: basic('alice', 'wrong')})).status()).toBe(401);

    await api.dispose();
    await traffic.dispose();
  });
});
