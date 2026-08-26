/**
 * ACME scenarios. See E2E_TESTBOOK.md ("ACME certificates").
 *
 * E2E-ACME-01 is unconditional: the main fixture gateway has no `acme:` block,
 * so it proves the "not configured" surface. E2E-ACME-02 is gated on
 * FEATHERBIT_TEST_PEBBLE_URL: it spawns a SECOND gateway (HTTPS on the port
 * Pebble validates against) from e2e/fixtures/acme/ and drives its UI.
 */
import {spawn, type ChildProcess} from 'node:child_process';
import {mkdirSync, rmSync} from 'node:fs';
import {dirname, resolve} from 'node:path';
import {fileURLToPath} from 'node:url';
import {test, expect, request, type APIRequestContext} from '@playwright/test';

import {ADMIN_PASS, ADMIN_USER, GATEWAY_BIN} from '../playwright.config';
import {adminApi} from '../helpers/admin';

const ACME_ADMIN_URL = 'http://127.0.0.1:19092';
// The suite runs as an ES module (package.json "type": "module"), so
// __dirname is unavailable -- derive it the same way playwright.config.ts does.
const here = dirname(fileURLToPath(import.meta.url));
const repo = resolve(here, '..', '..');

test.describe('ACME certificates', () => {
  test('E2E-ACME-01: without an acme: block the API and the panel say "not configured"', async ({page}) => {
    const api = await adminApi();
    const res = await api.get('/api/acme/certs');
    expect(res.status()).toBe(200);
    expect(await res.json()).toEqual({enabled: false, certs: []});
    const renew = await api.post('/api/acme/certs/x/renew');
    expect(renew.status()).toBe(501);
    await api.dispose();

    await page.goto('/');
    await page.getByRole('button', {name: 'Certificates'}).click();
    await expect(page.getByTestId('acme-not-configured')).toBeVisible();
  });

  test('E2E-ACME-02: a Pebble-backed gateway issues, shows and force-renews its certificate', async ({page}) => {
    test.skip(!process.env.FEATHERBIT_TEST_PEBBLE_URL, 'FEATHERBIT_TEST_PEBBLE_URL not set');
    test.setTimeout(180_000);

    const acmeDir = resolve(repo, 'e2e', '.tmp', 'acme');
    rmSync(acmeDir, {recursive: true, force: true});
    mkdirSync(acmeDir, {recursive: true});

    const child: ChildProcess = spawn(
      GATEWAY_BIN,
      ['--system-config', 'e2e/fixtures/acme/system.yaml', '--gateway-config', 'e2e/fixtures/acme/gateway.yaml'],
      {
        cwd: repo,
        env: {...process.env, ECHO_HOST: '127.0.0.1', ECHO_PORT: '3010', LOG_LEVEL: 'warn', FEATHERBIT_TEST_ACME_DIR: acmeDir},
        stdio: ['ignore', 'ignore', 'pipe'],
      },
    );
    let stderr = '';
    child.stderr?.on('data', (d) => (stderr += d.toString()));

    let api: APIRequestContext | undefined;
    try {
      api = await request.newContext({
        baseURL: ACME_ADMIN_URL,
        httpCredentials: {username: ADMIN_USER, password: ADMIN_PASS},
      });
      await waitFor(async () => (await api!.get('/healthz')).ok(), 15_000, `gateway did not start: ${stderr}`);

      // Placeholder first: /readyz is 503 naming the cert; then issuance lands.
      const first = (await (await api.get('/api/acme/certs')).json()) as {certs: {id: string; state: string}[]};
      expect(first.certs).toHaveLength(1);
      const id = first.certs[0].id;
      const issued = await waitFor(
        async () => {
          const body = (await (await api!.get('/api/acme/certs')).json()) as {certs: {state: string; serial: string; issuer: string}[]};
          return body.certs[0].state === 'issued' ? body.certs[0] : null;
        },
        90_000,
        `never issued: ${stderr}`,
      );
      expect(issued.issuer.toLowerCase()).toContain('pebble');
      expect((await api.get('/readyz')).status()).toBe(200);

      // UI: the row shows Issued; Renew now → not due → Force renew → new serial.
      await page.goto(`${ACME_ADMIN_URL}/`);
      await page.getByRole('button', {name: 'Certificates'}).click();
      const row = page.getByTestId('acme-cert-row').filter({has: page.getByText(id.split(',')[0])});
      await expect(row.getByTestId('acme-cert-state')).toHaveText(/issued/i);
      await row.getByRole('button', {name: 'Renew now'}).click();
      await row.getByRole('button', {name: 'Confirm'}).click();
      await row.getByRole('button', {name: 'Force renew'}).click();
      await waitFor(
        async () => {
          const body = (await (await api!.get('/api/acme/certs')).json()) as {certs: {state: string; serial: string}[]};
          return body.certs[0].state === 'issued' && body.certs[0].serial !== issued.serial;
        },
        90_000,
        `never renewed: ${stderr}`,
      );
    } finally {
      await api?.dispose();
      child.kill();
      await new Promise((r) => child.once('exit', r));
    }
  });
});

async function waitFor<T>(probe: () => Promise<T | null | false>, timeoutMs: number, message: string): Promise<T> {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    try {
      const v = await probe();
      if (v) return v as T;
    } catch {
      // not up yet
    }
    await new Promise((r) => setTimeout(r, 300));
  }
  throw new Error(message);
}
