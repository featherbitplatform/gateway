/**
 * Captures the documentation screenshots of the featherbit web UI.
 *
 *   node screenshots/capture.mjs            (from website/)
 *
 * Boots the release binary against the posed demo config in this directory,
 * drives the real UI with Playwright, and writes PNGs to static/img/ui/.
 * Committed so the shots can be regenerated whenever the UI changes -- docs
 * images rot exactly the way stale prose does, only more visibly.
 *
 * Each shot is taken twice, dark and light: the UI picks its theme from the OS
 * preference, and the docs pages serve the variant matching the reader's theme
 * (see src/components/UiShot.tsx).
 *
 * Requires: cargo build --release, and `npx playwright install chromium`.
 */
import {chromium} from 'playwright';
import {spawn} from 'node:child_process';
import {mkdirSync} from 'node:fs';
import {dirname, resolve} from 'node:path';
import {fileURLToPath} from 'node:url';

const here = dirname(fileURLToPath(import.meta.url));
const repo = resolve(here, '..', '..');
const outDir = resolve(here, '..', 'static', 'img', 'ui');

const ADMIN = 'http://127.0.0.1:19090';
const BIN = resolve(repo, 'target', 'release', process.platform === 'win32' ? 'featherbit.exe' : 'featherbit');

// A 16:10 viewport keeps the canvas legible once the image is scaled down to
// docs column width; deviceScaleFactor 2 keeps it crisp on HiDPI screens.
const VIEWPORT = {width: 1440, height: 900};

mkdirSync(outDir, {recursive: true});

console.log('starting gateway…');
const gw = spawn(
  BIN,
  [
    '--system-config', resolve(here, 'system.yaml'),
    '--gateway-config', resolve(here, 'gateway.yaml'),
  ],
  {cwd: repo, stdio: 'inherit'},
);

/** Polls the admin API until it answers, so we never race the boot. */
async function waitForAdmin(timeoutMs = 15000) {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    try {
      // Local screenshot rig: plain http to the dev gateway on localhost.
      // nosemgrep: typescript.react.security.react-insecure-request.react-insecure-request
      const res = await fetch(`${ADMIN}/api/routes`, {
        headers: {Authorization: 'Basic ' + Buffer.from('admin:admin').toString('base64')},
      });
      if (res.ok) return;
    } catch {
      /* not up yet */
    }
    await new Promise((r) => setTimeout(r, 250));
  }
  throw new Error('gateway admin API did not come up');
}

/** Captures the full set for one theme; `theme` is 'dark' or 'light'. */
async function captureTheme(browser, theme) {
  console.log(`capturing ${theme}…`);
  const page = await browser.newPage({
    viewport: VIEWPORT,
    deviceScaleFactor: 2,
    httpCredentials: {username: 'admin', password: 'admin'},
    // The UI reads the OS preference on first load (dark unless light is
    // explicitly preferred) and stores it under localStorage 'theme'.
    colorScheme: theme,
  });

  const shot = async (name, opts = {}) => {
    await page.screenshot({path: resolve(outDir, `${name}-${theme}.png`), ...opts});
    console.log(`  wrote static/img/ui/${name}-${theme}.png`);
  };

  await page.goto(ADMIN, {waitUntil: 'networkidle'});

  // Load the posed policy onto the canvas.
  await page.getByText('orders-api', {exact: true}).click();
  await page.waitForSelector('.react-flow__node');

  // ReactFlow's `fitView` prop only runs on mount -- at which point no route was
  // selected and the graph was empty. Click the fitView control now that the
  // nodes exist, or the graph sits as a small strip in a mostly empty canvas.
  await page.locator('.react-flow__controls-fitview').click();
  await page.waitForTimeout(1200); // let the zoom transition settle

  // 1. The whole editor: route list + graph. The "what is this product" shot.
  await shot('editor');

  // 2. The graph alone -- success chain across the top, the three error edges
  //    dropping to the shared handler below. Clipped to the nodes' own bounding
  //    box: the chain is wide and shallow, so fitting it to the 16:10 canvas
  //    leaves half the image empty sky. Padded generously at the bottom, where
  //    the error edges bow below the handler.
  const box = await page.evaluate(() => {
    const rects = [...document.querySelectorAll('.react-flow__node')].map((n) => n.getBoundingClientRect());
    const pad = 32;
    const x = Math.min(...rects.map((r) => r.left)) - pad;
    const y = Math.min(...rects.map((r) => r.top)) - pad;
    return {
      x,
      y,
      width: Math.max(...rects.map((r) => r.right)) + pad - x,
      height: Math.max(...rects.map((r) => r.bottom)) + pad * 1.5 - y,
    };
  });
  await shot('policy-graph', {clip: box});

  // 3. Plugin drawer open: makes "80+ plugins" concrete.
  await page.getByRole('button', {name: 'Add Node'}).click();
  await page.waitForTimeout(700);
  await shot('plugin-drawer');
  await page.getByRole('button', {name: 'Close'}).click();
  await page.waitForTimeout(400);

  // 4. Node inspector: the auth node's config form -- the "YAML and the UI are
  //    two views of one thing" shot. Re-fit first; opening the drawer drops the
  //    canvas back to its unzoomed viewport.
  await page.locator('.react-flow__controls-fitview').click();
  await page.waitForTimeout(800);
  await page.locator('.react-flow__node', {hasText: 'api-key'}).first().click();
  await page.waitForTimeout(700);
  await shot('node-inspector');

  await page.close();
}

let browser;
try {
  await waitForAdmin();
  console.log('gateway up; launching browser…');
  browser = await chromium.launch();
  for (const theme of ['dark', 'light']) await captureTheme(browser, theme);
  console.log('done.');
} finally {
  if (browser) await browser.close();
  gw.kill();
}
