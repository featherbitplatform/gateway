import { describe, expect, it } from 'vitest';
import { McpError, createMcpClient, parseRpcBody, resultText, toToolDefs } from './mcpClient';

interface Call {
  method: string;
  headers: Record<string, string>;
  body: Record<string, unknown> | null;
}

/** Scripted MCP server double: answers by JSON-RPC method, records every call. */
function fakeServer(opts: { sse?: boolean; expire?: boolean } = {}) {
  const calls: Call[] = [];
  let sessionCounter = 0;
  let expiredOnce = false;
  const respond = (id: unknown, result: unknown, session: string) => {
    const payload = JSON.stringify({ jsonrpc: '2.0', id, result });
    const body = opts.sse ? `event: message\ndata: ${payload}\n\n` : payload;
    return new Response(body, {
      status: 200,
      headers: { 'content-type': opts.sse ? 'text/event-stream' : 'application/json', 'mcp-session-id': session },
    });
  };
  const fetchImpl = (async (_url: RequestInfo | URL, init?: RequestInit) => {
    const headers = Object.fromEntries(Object.entries((init?.headers as Record<string, string>) ?? {}).map(([k, v]) => [k.toLowerCase(), v]));
    const body = init?.body ? (JSON.parse(init.body as string) as Record<string, unknown>) : null;
    calls.push({ method: String(body?.method), headers, body });
    const session = headers['mcp-session-id'];
    if (body?.method === 'initialize') {
      sessionCounter += 1;
      return respond(body.id, { protocolVersion: '2025-03-26', capabilities: {}, serverInfo: { name: 'featherbit' } }, `s${sessionCounter}`);
    }
    if (body?.method === 'notifications/initialized') return new Response(null, { status: 202 });
    if (opts.expire && !expiredOnce && session === 's1') {
      expiredOnce = true;
      return new Response('session not found', { status: 404 });
    }
    if (body?.method === 'tools/list') {
      return respond(body.id, { tools: [{ name: 'list_routes', description: 'Lists', inputSchema: { type: 'object' } }] }, session!);
    }
    if (body?.method === 'tools/call') {
      const p = body.params as { name: string };
      if (p.name === 'boom') return respond(body.id, { content: [{ type: 'text', text: '{"code":"not_found"}' }], isError: true }, session!);
      return respond(body.id, { content: [{ type: 'text', text: '["a"]' }] }, session!);
    }
    return new Response('nope', { status: 500 });
  }) as typeof fetch;
  return { calls, fetchImpl };
}

describe('parseRpcBody', () => {
  it('parses plain JSON and single-event SSE', () => {
    expect(parseRpcBody('{"a":1}')).toEqual({ a: 1 });
    expect(parseRpcBody('event: message\ndata: {"a":2}\n\n')).toEqual({ a: 2 });
    expect(parseRpcBody('')).toBeNull();
  });
});

describe('createMcpClient', () => {
  it('initializes once, sends the initialized notification, then lists tools with the session header', async () => {
    const srv = fakeServer();
    const c = createMcpClient({ url: 'http://gw/mcp', token: 'tok', fetchImpl: srv.fetchImpl });
    const tools = await c.listTools();
    expect(tools.map((t) => t.name)).toEqual(['list_routes']);
    expect(srv.calls.map((x) => x.method)).toEqual(['initialize', 'notifications/initialized', 'tools/list']);
    expect(srv.calls[0].headers.authorization).toBe('Bearer tok');
    expect(srv.calls[0].headers.accept).toBe('application/json, text/event-stream');
    expect(srv.calls[2].headers['mcp-session-id']).toBe('s1');
    await c.callTool('list_routes', {});
    expect(srv.calls.filter((x) => x.method === 'initialize')).toHaveLength(1);
  });

  it('understands SSE-framed replies', async () => {
    const srv = fakeServer({ sse: true });
    const c = createMcpClient({ url: 'http://gw/mcp', token: 'tok', fetchImpl: srv.fetchImpl });
    const r = await c.callTool('list_routes', {});
    expect(resultText(r)).toBe('["a"]');
    expect(r.isError).toBeUndefined();
  });

  it('re-initializes once on a 404 and retries the call', async () => {
    const srv = fakeServer({ expire: true });
    const c = createMcpClient({ url: 'http://gw/mcp', token: 'tok', fetchImpl: srv.fetchImpl });
    await c.listTools();
    const r = await c.callTool('list_routes', {});
    expect(resultText(r)).toBe('["a"]');
    expect(srv.calls.filter((x) => x.method === 'initialize')).toHaveLength(2);
    const last = srv.calls.at(-1)!;
    expect(last.headers['mcp-session-id']).toBe('s2');
  });

  it('passes isError through and wraps HTTP failures in McpError', async () => {
    const srv = fakeServer();
    const c = createMcpClient({ url: 'http://gw/mcp', token: 'tok', fetchImpl: srv.fetchImpl });
    const r = await c.callTool('boom', {});
    expect(r.isError).toBe(true);
    const failing = (async () => new Response('{"error":"unauthorized"}', { status: 401 })) as typeof fetch;
    const bad = createMcpClient({ url: 'http://gw/mcp', token: 'x', fetchImpl: failing });
    await expect(bad.listTools()).rejects.toBeInstanceOf(McpError);
    await expect(bad.listTools()).rejects.toMatchObject({ status: 401 });
  });
});

describe('toToolDefs / resultText', () => {
  it('maps MCP tools to OpenAI function tools, defaulting the schema', () => {
    expect(toToolDefs([{ name: 'a', description: 'd', inputSchema: { type: 'object', properties: {} } }, { name: 'b' }])).toEqual([
      { type: 'function', function: { name: 'a', description: 'd', parameters: { type: 'object', properties: {} } } },
      { type: 'function', function: { name: 'b', parameters: { type: 'object', properties: {} } } },
    ]);
  });
  it('joins text parts', () => {
    expect(resultText({ content: [{ type: 'text', text: 'a' }, { type: 'image' }, { type: 'text', text: 'b' }] })).toBe('a\nb');
    expect(resultText({ content: [] })).toBe('');
  });
});
