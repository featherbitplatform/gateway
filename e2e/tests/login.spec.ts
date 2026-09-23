/**
 * Sign-in screen scenarios. See E2E_TESTBOOK.md ("Sign-in").
 *
 * The rest of the suite starts every page signed in (storageState in
 * playwright.config.ts); these clear that to start from a first visit.
 */
import {test, expect, type Page} from '@playwright/test';

import {ADMIN_PASS, ADMIN_URL, ADMIN_USER} from '../playwright.config';

test.use({storageState: {cookies: [], origins: []}});

async function signIn(page: Page, user: string, pass: string, remember = false) {
  await page.getByLabel('Username').fill(user);
  await page.getByLabel('Password').fill(pass);
  if (remember) await page.getByLabel('Remember me on this browser').check();
  await page.getByRole('button', {name: 'Sign in'}).click();
}

const stored = (page: Page) =>
  page.evaluate(() => ({
    session: sessionStorage.getItem('gw_credentials'),
    local: localStorage.getItem('gw_credentials'),
  }));

test.describe('Sign-in', () => {
  test('E2E-LOGIN-01: a first visit shows the sign-in screen, not the editor', async ({page}) => {
    await page.goto('/');
    await expect(page.getByRole('form', {name: 'Sign in'})).toBeVisible();
    await expect(page.getByText('echo-api', {exact: true})).toHaveCount(0);

    // The UI's requests get a bare 401 (no browser dialog); everyone else
    // still gets the Basic challenge.
    // Plain fetch: a Playwright request context would answer the challenge
    // with the suite's httpCredentials.
    const ui = await fetch(`${ADMIN_URL}/api/status`, {headers: {'X-Featherbit-Client': 'ui'}});
    expect(ui.status).toBe(401);
    expect(ui.headers.get('www-authenticate')).toBeNull();
    const curl = await fetch(`${ADMIN_URL}/api/status`);
    expect(curl.status).toBe(401);
    expect(curl.headers.get('www-authenticate')).toContain('Basic');
  });

  test('E2E-LOGIN-02: wrong credentials are refused and nothing is stored', async ({page}) => {
    await page.goto('/');
    await signIn(page, ADMIN_USER, 'not-the-password');
    await expect(page.getByRole('alert')).toHaveText('Wrong username or password.');
    await expect(page.getByRole('form', {name: 'Sign in'})).toBeVisible();
    expect(await stored(page)).toEqual({session: null, local: null});
  });

  test('E2E-LOGIN-03: signing in opens the editor, for this tab only by default', async ({page}) => {
    await page.goto('/');
    await signIn(page, ADMIN_USER, ADMIN_PASS);
    await expect(page.getByText('echo-api', {exact: true})).toBeVisible();
    await expect(page.getByText(`Signed in as ${ADMIN_USER}`)).toBeVisible();
    // The fixture runs on the shipped default, which the UI calls out.
    await expect(page.getByText(/still uses the default admin\/admin credentials/)).toBeVisible();
    expect(await stored(page)).toEqual({session: `${ADMIN_USER}:${ADMIN_PASS}`, local: null});

    // A reload keeps the tab signed in.
    await page.reload();
    await expect(page.getByText('echo-api', {exact: true})).toBeVisible();
  });

  test('E2E-LOGIN-04: "Remember me" persists; Sign out forgets everywhere', async ({page}) => {
    await page.goto('/');
    await signIn(page, ADMIN_USER, ADMIN_PASS, true);
    await expect(page.getByText('echo-api', {exact: true})).toBeVisible();
    expect(await stored(page)).toEqual({session: null, local: `${ADMIN_USER}:${ADMIN_PASS}`});

    await page.getByRole('button', {name: 'Sign out'}).click();
    await expect(page.getByRole('form', {name: 'Sign in'})).toBeVisible();
    expect(await stored(page)).toEqual({session: null, local: null});
    await page.reload();
    await expect(page.getByRole('form', {name: 'Sign in'})).toBeVisible();
  });

  test('E2E-LOGIN-05: a 401 mid-session overlays the sign-in and keeps the editor', async ({page}) => {
    await page.goto('/');
    await signIn(page, ADMIN_USER, ADMIN_PASS);
    await page.getByText('echo-api', {exact: true}).click();
    await page.waitForSelector('.react-flow__node');

    // Simulate the password changing server-side for the next call.
    await page.route('**/api/config/export', (route) => route.fulfill({status: 401, body: 'Unauthorized'}), {times: 1});
    await page.getByRole('button', {name: 'View YAML'}).click();

    await expect(page.getByText('Sign in again')).toBeVisible();
    await expect(page.locator('.react-flow__node').first()).toBeAttached(); // the canvas stayed mounted
    await expect(page.getByLabel('Username')).toHaveValue(ADMIN_USER);
    await page.getByLabel('Password').fill(ADMIN_PASS);
    await page.getByRole('button', {name: 'Sign in'}).click();

    await expect(page.getByText('Sign in again')).toHaveCount(0);
    await expect(page.locator('.react-flow__node').first()).toBeVisible();
  });
});
