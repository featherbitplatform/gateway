import {defineConfig, devices} from '@playwright/test';
import {copyFileSync, existsSync, mkdirSync} from 'node:fs';
import {dirname, resolve} from 'node:path';
import {fileURLToPath} from 'node:url';

const here = dirname(fileURLToPath(import.meta.url));
const repo = resolve(here, '..');

export const ADMIN_URL = 'http://127.0.0.1:19091';
export const GATEWAY_URL = 'http://127.0.0.1:18081';
export const IDP_URL = 'http://127.0.0.1:3011';
export const AUTH_URL = 'http://127.0.0.1:3012';
export const ADMIN_USER = 'admin';
export const ADMIN_PASS = 'admin';

/** The release binary the suite exercises. Checked in global-setup. */
export const GATEWAY_BIN = resolve(
  repo,
  'target',
  'release',
  process.platform === 'win32' ? 'featherbit.exe' : 'featherbit',
);

// Ubuntu (and the CI runner) ship `python3` with no bare `python`; Windows is the
// other way round. PYTHON overrides both.
const PYTHON = process.env.PYTHON ?? (process.platform === 'win32' ? 'python' : 'python3');

/**
 * Stages the fixture config into e2e/.tmp/ and checks the binary exists.
 *
 * This has to run at config-load time, NOT in globalSetup: Playwright starts the
 * webServers *before* globalSetup, so a gateway launched against e2e/.tmp/ would
 * find nothing there on a clean checkout and die before setup ever ran.
 *
 * But it must run ONLY in the main process. Playwright re-imports this config in
 * every worker, and re-copying gateway.yaml while the gateway is live makes the
 * file watcher reload from disk -- silently deleting every route the tests created
 * through the admin API, because FileConfigStore keeps admin writes in memory and
 * never writes them back to the file. TEST_WORKER_INDEX is set only in workers.
 */
function stageFixtures() {
  if (process.env.TEST_WORKER_INDEX !== undefined) return;

  if (!existsSync(GATEWAY_BIN)) {
    throw new Error(
      `\nThe gateway binary is missing:\n  ${GATEWAY_BIN}\n\n` +
        `The e2e suite runs the release build. Build it first:\n` +
        `  cargo build --release\n` +
        `or, from e2e/:  npm run build:gateway\n`,
    );
  }

  const tmp = resolve(here, '.tmp');
  mkdirSync(tmp, {recursive: true});
  for (const f of ['system.yaml', 'gateway.yaml']) {
    copyFileSync(resolve(here, 'fixtures', f), resolve(tmp, f));
  }
}

stageFixtures();

export default defineConfig({
  testDir: './tests',
  // One gateway process holds the route table all specs share, and several specs
  // mutate it. Serial execution keeps failures meaningful; the suite is small
  // enough that parallelism buys little.
  fullyParallel: false,
  workers: 1,
  forbidOnly: !!process.env.CI,
  retries: process.env.CI ? 1 : 0,
  reporter: process.env.CI ? [['github'], ['html', {open: 'never'}]] : [['list'], ['html', {open: 'never'}]],
  timeout: 30_000,

  use: {
    baseURL: ADMIN_URL,
    httpCredentials: {username: ADMIN_USER, password: ADMIN_PASS},
    trace: 'retain-on-failure',
    screenshot: 'only-on-failure',
  },

  projects: [{name: 'chromium', use: {...devices['Desktop Chrome']}}],

  webServer: [
    {
      // The same echo backend docker-compose uses: returns {method, path, headers, body}.
      command: `${PYTHON} dev/echo-backend/server.py`,
      cwd: repo,
      env: {PORT: '3010'},
      url: 'http://127.0.0.1:3010/',
      reuseExistingServer: !process.env.CI,
      stdout: 'ignore',
      stderr: 'pipe',
    },
    {
      // Hermetic OpenID Connect provider: discovery, JWKS, /authorize, /token.
      // The gateway verifies its RS256 signatures for real against the JWKS.
      command: 'node mock-idp/server.mjs',
      cwd: here,
      env: {PORT: '3011'},
      url: `${IDP_URL}/.well-known/openid-configuration`,
      reuseExistingServer: !process.env.CI,
      stdout: 'ignore',
      stderr: 'pipe',
    },
    {
      // Mock for the HTTP-delegation auth plugins (forward-auth, opa, and the
      // token/ticket validators). Each endpoint speaks one plugin's real contract.
      command: 'node mock-auth/server.mjs',
      cwd: here,
      env: {PORT: '3012'},
      url: `${AUTH_URL}/cas/serviceValidate?ticket=probe`,
      reuseExistingServer: !process.env.CI,
      stdout: 'ignore',
      stderr: 'pipe',
    },
    {
      // Mock LDAP for ldap-auth. Raw LDAP over TCP, so readiness is a port check
      // (no HTTP url) -- Playwright waits for the port to accept connections.
      command: 'node mock-ldap/server.mjs',
      cwd: here,
      env: {LDAP_PORT: '3389'},
      port: 3389,
      reuseExistingServer: !process.env.CI,
      stdout: 'ignore',
      stderr: 'pipe',
    },
    {
      command: `"${GATEWAY_BIN}" --system-config e2e/.tmp/system.yaml --gateway-config e2e/.tmp/gateway.yaml`,
      cwd: repo,
      env: {ECHO_HOST: '127.0.0.1', ECHO_PORT: '3010', LOG_LEVEL: 'warn'},
      // /healthz needs no auth, so it is the honest readiness signal here.
      url: `${ADMIN_URL}/healthz`,
      reuseExistingServer: !process.env.CI,
      stdout: 'ignore',
      stderr: 'pipe',
    },
  ],
});
