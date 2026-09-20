/**
 * Script node scenarios. See E2E_TESTBOOK.md ("Scripts").
 *
 * The runtime and engine tests already pin the return protocol; what only
 * this level shows is a real route where the script's own 403 reaches the
 * client over the wire because the node left on `respond`, while a browser
 * UA still gets the upstream's body.
 */
import {expect, request, test} from '@playwright/test';

import {adminApi} from '../helpers/admin';
import {GATEWAY_URL} from '../playwright.config';

const BLOCKER = [
  'function execute(ctx)',
  '  local ua = (ctx.request.headers["user-agent"] or {})[1] or ""',
  '  if string.find(string.lower(ua), "scrapy") then',
  '    ctx.response.status_code = 403',
  '    ctx.response.body = \'{"error": "forbidden"}\'',
  '    ctx.response.headers["content-type"] = { "application/json" }',
  '    return ctx, "respond"',
  '  end',
  '  return ctx',
  'end',
].join('\n');

const policy = {
  name: 'e2e-script-respond',
  nodes: [
    {id: 'listener', type: 'listener'},
    {id: 'block', type: 'script', config: {runtime: 'lua', inline: BLOCKER}},
    {id: 'backend', type: 'mocking', config: {response_status: 200, response_example: 'proxied'}},
    {id: 'client', type: 'client'},
  ],
  edges: [
    {from: 'listener.out', to: 'block.in'},
    {from: 'block.respond', to: 'client.in'},
    {from: 'block.success', to: 'backend.in'},
    {from: 'backend.success', to: 'client.in'},
  ],
};
const route = {name: 'e2e-script-respond', match: {path: '/e2e-script'}, policy: policy.name};

test.describe('Scripts', () => {
  test.beforeAll(async () => {
    const api = await adminApi();
    const p = await api.put(`/api/policies/${policy.name}`, {data: policy});
    expect(p.ok(), await p.text()).toBeTruthy();
    await api.delete(`/api/routes/${route.name}`);
    const r = await api.post('/api/routes', {data: route});
    expect(r.ok(), await r.text()).toBeTruthy();
    await api.dispose();
  });

  test.afterAll(async () => {
    const api = await adminApi();
    await api.delete(`/api/routes/${route.name}`);
    await api.delete(`/api/policies/${policy.name}`);
    await api.dispose();
  });

  test('E2E-SCRIPT-01: a script answering with "respond" short-circuits the upstream', async () => {
    const dp = await request.newContext({baseURL: GATEWAY_URL});

    const bot = await dp.get('/e2e-script', {headers: {'user-agent': 'scrapy/2.0'}});
    expect(bot.status()).toBe(403);
    expect(await bot.json()).toEqual({error: 'forbidden'});

    const browser = await dp.get('/e2e-script', {headers: {'user-agent': 'Mozilla/5.0'}});
    expect(browser.status()).toBe(200);
    expect(await browser.text()).toBe('proxied');

    await dp.dispose();
  });
});
