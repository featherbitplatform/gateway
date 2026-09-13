import {expect, test} from '@playwright/test';
import {ADMIN_URL} from '../playwright.config';
import {adminApi} from '../helpers/admin';

const READ = 'e2e-read-token-0123456789';
const WRITE = 'e2e-write-token-0123456789';
const MCP = `${ADMIN_URL}/mcp`;

/** Minimal MCP client over Streamable HTTP: one JSON-RPC call per request (stateless from our side). */
async function rpc(token: string | null, method: string, params: unknown, id = 1, session?: string) {
  const headers: Record<string, string> = {
    'content-type': 'application/json',
    accept: 'application/json, text/event-stream',
  };
  if (token) headers.authorization = `Bearer ${token}`;
  if (session) headers['mcp-session-id'] = session;
  const res = await fetch(MCP, {method: 'POST', headers, body: JSON.stringify({jsonrpc: '2.0', id, method, params})});
  const text = await res.text();
  // The server may answer as plain JSON or as a single SSE event.
  const data = text.trim().startsWith('{') ? text : text.split('\n').filter((l) => l.startsWith('data:')).map((l) => l.slice(5).trim()).join('');
  return {status: res.status, session: res.headers.get('mcp-session-id') ?? session, body: data ? JSON.parse(data) : null};
}

const INIT = {protocolVersion: '2025-03-26', capabilities: {}, clientInfo: {name: 'e2e', version: '0'}};

async function initialized(token: string) {
  const init = await rpc(token, 'initialize', INIT);
  expect(init.status).toBe(200);
  await fetch(MCP, {
    method: 'POST',
    headers: {
      'content-type': 'application/json',
      accept: 'application/json, text/event-stream',
      authorization: `Bearer ${token}`,
      ...(init.session ? {'mcp-session-id': init.session} : {}),
    },
    body: JSON.stringify({jsonrpc: '2.0', method: 'notifications/initialized'}),
  });
  return init.session;
}

test.describe('MCP server', () => {
  test('E2E-MCP-01: bearer scopes gate the tool list; anonymous and Basic Auth are refused', async () => {
    const anon = await rpc(null, 'initialize', INIT);
    expect(anon.status).toBe(401);

    const basic = await fetch(MCP, {
      method: 'POST',
      headers: {
        'content-type': 'application/json',
        accept: 'application/json, text/event-stream',
        authorization: 'Basic ' + Buffer.from('admin:admin').toString('base64'),
      },
      body: JSON.stringify({jsonrpc: '2.0', id: 1, method: 'initialize', params: INIT}),
    });
    expect(basic.status).toBe(401);

    const rs = await initialized(READ);
    const readTools = await rpc(READ, 'tools/list', {}, 2, rs);
    const readNames = readTools.body.result.tools.map((t: {name: string}) => t.name);
    expect(readNames).toContain('get_policy');
    expect(readNames.some((n: string) => n.startsWith('put_'))).toBe(false);

    const ws = await initialized(WRITE);
    const writeTools = await rpc(WRITE, 'tools/list', {}, 2, ws);
    const writeNames = writeTools.body.result.tools.map((t: {name: string}) => t.name);
    expect(writeNames).toContain('put_policy');

    const forbidden = await rpc(READ, 'tools/call', {name: 'put_policy', arguments: {name: 'x', definition: {}}}, 3, rs);
    expect(forbidden.body.result.isError).toBe(true);
    expect(JSON.parse(forbidden.body.result.content[0].text).code).toBe('forbidden');

    // An MCP token does not open the Admin API.
    const api = await fetch(`${ADMIN_URL}/api/policies`, {headers: {authorization: `Bearer ${WRITE}`}});
    expect(api.status).toBe(401);
  });

  test('E2E-MCP-02: Agent panel shows the endpoint, snippets and a scope list matching tools/list', async ({page}) => {
    await page.goto('/');
    await page.getByRole('button', {name: 'Agent'}).click();
    const panel = page.getByRole('dialog', {name: 'Agent'});
    await expect(panel.getByTestId('mcp-endpoint')).toHaveText(`${ADMIN_URL}/mcp`);
    await expect(panel.getByText('claude mcp add --transport http featherbit')).toBeVisible();
    await expect(panel.getByText('<TOKEN>').first()).toBeVisible();
    await expect(panel.getByText('why_this_port')).toBeVisible();

    const ws = await initialized(WRITE);
    const all = await rpc(WRITE, 'tools/list', {}, 2, ws);
    const serverNames: string[] = all.body.result.tools.map((t: {name: string}) => t.name);

    // Ruling: skip the brittle name-extraction regex over the combined text.
    // The two scope-explainer paragraphs ("read: ..." / "write (also
    // everything above): ...") are grabbed by their own text content and
    // checked with toContain per tool name -- this is less flaky than one
    // getByText(name) locator per tool, several of which are substrings of
    // other visible strings (prompt names, other tool names) and would risk
    // strict-mode "resolved to N elements" failures.
    // getByText(/^write/) alone resolves to the inner <b>write</b> (its own
    // text is just "write", matching the regex, and Playwright's text engine
    // prefers the innermost matching element) rather than the whole <p> --
    // so filter on a phrase unique to the paragraph's own text instead.
    const readText = (await panel.locator('p').filter({hasText: 'list_node_types'}).textContent()) ?? '';
    const writeText = (await panel.locator('p').filter({hasText: 'also everything above'}).textContent()) ?? '';
    const combined = `${readText} ${writeText}`;
    for (const n of serverNames) {
      expect(combined, `scope explainer lists ${n}`).toContain(n);
    }
  });

  test('E2E-MCP-03: "Copy prompt" on a trace copies the troubleshooting prompt with the policy inlined and the MCP hint', async ({
    page,
    context,
  }) => {
    await context.grantPermissions(['clipboard-read', 'clipboard-write']);
    const api = await adminApi();
    // Make a trace via the sandbox against the fixture's first policy.
    const policies = await (await api.get('/api/policies')).json();
    const policy = policies[0].name as string;
    const run = await api.post('/api/debug/sandbox', {data: {policy, context: {method: 'GET', path: '/'}}});
    expect(run.ok()).toBeTruthy();

    await page.goto('/');
    await page.getByRole('button', {name: 'Debug'}).click();
    const debug = page.getByRole('dialog', {name: 'Debug'});
    // Rows are <button>s showing method/path/status text, not trace ids
    // (DebugPanel.tsx:309-339) -- the newest trace (ours, just created) is
    // always the first row since the store lists newest-first and the suite
    // runs single-worker/serial, so nothing else can race a trace in.
    const rows = debug.locator('button.w-full.text-left');
    await expect(rows.first()).toBeVisible();
    await rows.first().click();

    // The primary action opens the chat (covered by E2E-CHAT-01); the
    // secondary "Copy prompt" keeps the clipboard path for external agents.
    await expect(debug.getByRole('button', {name: 'Troubleshoot with AI'})).toBeVisible();
    await debug.getByRole('button', {name: 'Copy prompt'}).click();
    await expect(page.getByText('Copied to clipboard')).toBeVisible();
    const text = await page.evaluate(() => navigator.clipboard.readText());
    expect(text).toContain('# Troubleshoot `GET /`');
    expect(text).toContain('validate it with validate_policy');
    expect(text).toContain('If the `featherbit` MCP server is connected');
    expect(text).toContain(policy);
    await api.dispose();
  });
});
