import {expect, test, type Page, type Route} from '@playwright/test';
import {ADMIN_URL} from '../playwright.config';
import {adminApi} from '../helpers/admin';

const WRITE = 'e2e-write-token-0123456789';
const FAKE_BASE = `${ADMIN_URL}/fake-openai/v1`;
const SETTINGS_KEY = 'featherbit.chat.settings';
const THREADS_KEY = 'featherbit.chat.threads';

/** One SSE chunk stream in Chat Completions shape. */
function sse(deltas: Array<Record<string, unknown>>, finish: 'stop' | 'tool_calls'): string {
  const lines = deltas.map((d) => `data: ${JSON.stringify({choices: [{delta: d, finish_reason: null}]})}\n\n`);
  lines.push(`data: ${JSON.stringify({choices: [{delta: {}, finish_reason: finish}]})}\n\n`, 'data: [DONE]\n\n');
  return lines.join('');
}

function text(t: string) {
  return sse([{content: t}], 'stop');
}

function toolCall(id: string, name: string, args: Record<string, unknown>) {
  return sse([{tool_calls: [{index: 0, id, type: 'function', function: {name, arguments: JSON.stringify(args)}}]}], 'tool_calls');
}

/**
 * A scripted model: answers by looking at the last message. Reads: the first
 * user turn asks for a read tool, the tool result gets a text answer. Writes:
 * a user message containing "please write a policy" asks for put_policy; a declined result
 * gets an acknowledgement.
 */
async function installFakeProvider(page: Page, seen: {bodies: Array<Record<string, unknown>>}) {
  await page.route(`${FAKE_BASE}/**`, async (route: Route) => {
    const req = route.request();
    if (req.method() === 'OPTIONS') return route.fulfill({status: 204});
    const body = req.postDataJSON() as {messages: Array<{role: string; content: string}>; tools?: unknown[]};
    seen.bodies.push(body);
    const last = body.messages.at(-1)!;
    let reply: string;
    if (last.role === 'tool') {
      reply = last.content === 'Declined by the user.' ? text('Understood, skipped the write.') : text('The node exited on that port because the request matched.');
    } else if (/please write a policy/i.test(last.content)) {
      reply = toolCall('call_w', 'put_policy', {name: 'e2e-chat-tmp', definition: {nodes: [], edges: []}});
    } else {
      reply = toolCall('call_r', 'list_policies', {});
    }
    await route.fulfill({status: 200, headers: {'content-type': 'text/event-stream'}, body: reply});
  });
}

async function seedSettings(page: Page) {
  await page.addInitScript(
    ([key, settings]) => {
      window.localStorage.setItem(key, JSON.stringify(settings));
    },
    [SETTINGS_KEY, {baseUrl: FAKE_BASE, model: 'fake-model', apiKey: 'sk-e2e', mcpToken: WRITE}] as const,
  );
}

test.describe('Chat', () => {
  test('E2E-CHAT-01: "Ask agent" from a trace seeds a thread, runs a real read tool, shows the answer, survives reload', async ({page}) => {
    const seen = {bodies: [] as Array<Record<string, unknown>>};
    await seedSettings(page);
    await installFakeProvider(page, seen);
    const api = await adminApi();
    const policies = await (await api.get('/api/policies')).json();
    const policy = policies[0].name as string;
    const run = await api.post('/api/debug/sandbox', {data: {policy, context: {method: 'GET', path: '/'}}});
    expect(run.ok()).toBeTruthy();

    await page.goto('/');
    await page.getByRole('button', {name: 'Debug'}).click();
    const debug = page.getByRole('dialog', {name: 'Debug'});
    const rows = debug.locator('button.w-full.text-left');
    await rows.first().click();
    await debug.getByRole('button', {name: 'Ask AI about this step'}).click();

    const chat = page.getByRole('dialog', {name: 'Chat'});
    await expect(chat).toBeVisible();
    await expect(chat.getByTestId('chat-connection-line')).toContainText('write scope');
    await expect(chat.getByTestId('chat-threads')).toContainText('why_this_port');
    // The seeded prompt is the first user bubble and inlines the policy name.
    await expect(chat.locator('[data-role="user"]').first()).toContainText(policy);
    // A real MCP read tool ran (auto, no confirmation) and the fake model answered.
    const card = chat.getByTestId('tool-call-list_policies');
    await expect(card).toContainText('done');
    await expect(chat.locator('[data-role="assistant"]')).toContainText('exited on that port');
    // The provider saw the gateway's tool schemas.
    expect((seen.bodies[0].tools as unknown[]).length).toBeGreaterThan(10);
    expect(seen.bodies[0].messages).toEqual(expect.arrayContaining([expect.objectContaining({role: 'system'})]));

    // Persisted: reload, reopen, same thread and reply.
    await page.reload();
    await page.getByRole('button', {name: 'Chat'}).click();
    const again = page.getByRole('dialog', {name: 'Chat'});
    await again.getByTestId('chat-threads').getByText(/why_this_port/).click();
    await expect(again.locator('[data-role="assistant"]')).toContainText('exited on that port');
    await api.dispose();
  });

  test('E2E-CHAT-02: a write tool waits for Run/Skip; Skip feeds a declined result back', async ({page}) => {
    const seen = {bodies: [] as Array<Record<string, unknown>>};
    await seedSettings(page);
    await installFakeProvider(page, seen);
    await page.goto('/');
    await page.getByRole('button', {name: 'Chat'}).click();
    const chat = page.getByRole('dialog', {name: 'Chat'});
    await chat.getByRole('button', {name: 'New chat'}).click();
    await chat.getByLabel('Message').fill('please write a policy for me');
    await chat.getByRole('button', {name: 'Send'}).click();

    const card = chat.getByTestId('tool-call-put_policy');
    await expect(card).toContainText('awaiting confirmation');
    await expect(card.getByRole('button', {name: 'Run'})).toBeVisible();
    await card.getByRole('button', {name: 'Skip'}).click();
    await expect(card).toContainText('declined');
    await expect(chat.locator('[data-role="assistant"]').last()).toContainText('Understood, skipped');
    // The declined result reached the model verbatim.
    const last = seen.bodies.at(-1)!.messages as Array<{role: string; content: string}>;
    expect(last.at(-1)).toMatchObject({role: 'tool', content: 'Declined by the user.'});
    // Nothing was written.
    const api = await adminApi();
    expect((await api.get('/api/policies/e2e-chat-tmp')).status()).toBe(404);
    await api.dispose();
  });

  test('E2E-CHAT-03: secrets are redacted before storage and sending; "Clear all chats" flushes threads and keeps settings', async ({page}) => {
    const seen = {bodies: [] as Array<Record<string, unknown>>};
    await seedSettings(page);
    await installFakeProvider(page, seen);
    await page.goto('/');
    await page.getByRole('button', {name: 'Chat'}).click();
    const chat = page.getByRole('dialog', {name: 'Chat'});
    await chat.getByRole('button', {name: 'New chat'}).click();
    await chat.getByLabel('Message').fill(`hello, my header is Authorization: Bearer supersecrettoken123 and my mcp token is ${WRITE}`);
    await chat.getByRole('button', {name: 'Send'}).click();
    await expect(chat.getByTestId('tool-call-list_policies')).toContainText('done');
    // Redacted in the bubble, in what the provider received, and in local storage.
    await expect(chat.locator('[data-role="user"]').first()).toContainText('[REDACTED]');
    await expect(chat.locator('[data-role="user"]').first()).not.toContainText('supersecrettoken123');
    const sent = JSON.stringify(seen.bodies);
    expect(sent).not.toContain('supersecrettoken123');
    expect(sent).not.toContain(WRITE);
    const stored = await page.evaluate((k) => localStorage.getItem(k) ?? '', THREADS_KEY);
    expect(stored).not.toContain('supersecrettoken123');
    expect(stored).not.toContain(WRITE);
    expect(await page.evaluate((k) => JSON.parse(localStorage.getItem(k) ?? 'null')?.threads?.length, THREADS_KEY)).toBe(1);

    await chat.getByRole('button', {name: 'Clear all chats'}).click();
    await expect(chat.getByTestId('chat-threads')).toContainText('No chats yet.');
    expect(await page.evaluate((k) => JSON.parse(localStorage.getItem(k) ?? 'null')?.threads?.length, THREADS_KEY)).toBe(0);
    expect(await page.evaluate((k) => JSON.parse(localStorage.getItem(k) ?? 'null')?.apiKey, SETTINGS_KEY)).toBe('sk-e2e');
  });
});
