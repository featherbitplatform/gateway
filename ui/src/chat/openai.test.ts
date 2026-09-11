import { describe, expect, it } from 'vitest';
import {
  ToolCallAccumulator,
  listModels,
  splitSseEvents,
  streamChat,
  toWire,
  type StreamEvent,
  type WireMessage,
} from './openai';

const settings = { baseUrl: 'https://api.example/v1/', model: 'm', apiKey: 'sk-test' };

function streamFrom(chunks: string[]): ReadableStream<Uint8Array> {
  const enc = new TextEncoder();
  return new ReadableStream({
    start(c) {
      for (const ch of chunks) c.enqueue(enc.encode(ch));
      c.close();
    },
  });
}

function fakeFetch(chunks: string[], status = 200, capture?: { init?: RequestInit; url?: string }): typeof fetch {
  return (async (url: RequestInfo | URL, init?: RequestInit) => {
    if (capture) {
      capture.init = init;
      capture.url = String(url);
    }
    if (status !== 200) return new Response(chunks.join(''), { status });
    return new Response(streamFrom(chunks), { status, headers: { 'content-type': 'text/event-stream' } });
  }) as typeof fetch;
}

async function collect(gen: AsyncGenerator<StreamEvent>): Promise<StreamEvent[]> {
  const out: StreamEvent[] = [];
  for await (const e of gen) out.push(e);
  return out;
}

const data = (o: unknown) => `data: ${JSON.stringify(o)}\n\n`;

describe('splitSseEvents', () => {
  it('splits complete events and keeps the incomplete tail', () => {
    const { events, rest } = splitSseEvents('data: a\n\ndata: b\n\ndata: c');
    expect(events).toEqual(['a', 'b']);
    expect(rest).toBe('data: c');
  });
  it('handles CRLF and multi-line data', () => {
    const { events } = splitSseEvents('data: x\r\ndata: y\r\n\r\n');
    expect(events).toEqual(['x\ny']);
  });
});

describe('ToolCallAccumulator', () => {
  it('reassembles tool calls split across deltas by index', () => {
    const acc = new ToolCallAccumulator();
    acc.apply([{ index: 0, id: 'call_1', type: 'function', function: { name: 'get_policy', arguments: '{"na' } }]);
    acc.apply([{ index: 1, id: 'call_2', function: { name: 'list_routes', arguments: '' } }]);
    acc.apply([{ index: 0, function: { arguments: 'me":"api"}' } }]);
    acc.apply([{ index: 1, function: { arguments: '{}' } }]);
    expect(acc.finish()).toEqual([
      { id: 'call_1', name: 'get_policy', arguments: '{"name":"api"}' },
      { id: 'call_2', name: 'list_routes', arguments: '{}' },
    ]);
  });
  it('defaults empty arguments to {} and ignores junk', () => {
    const acc = new ToolCallAccumulator();
    acc.apply([{ index: 0, id: 'c', function: { name: 'list_routes' } }]);
    acc.apply('nope');
    expect(acc.finish()).toEqual([{ id: 'c', name: 'list_routes', arguments: '{}' }]);
  });
});

describe('toWire', () => {
  it('maps thread messages to the Chat Completions shape', () => {
    const wire = toWire('SYS', [
      { role: 'user', content: 'hi' },
      { role: 'assistant', content: '', toolCalls: [{ id: 'c1', name: 'list_routes', arguments: '{}' }] },
      { role: 'tool', toolCallId: 'c1', name: 'list_routes', status: 'done', content: '[]' },
      { role: 'assistant', content: '', error: 'boom' },
      { role: 'assistant', content: 'done' },
    ]);
    expect(wire as WireMessage[]).toEqual([
      { role: 'system', content: 'SYS' },
      { role: 'user', content: 'hi' },
      {
        role: 'assistant',
        content: '',
        tool_calls: [{ id: 'c1', type: 'function', function: { name: 'list_routes', arguments: '{}' } }],
      },
      { role: 'tool', tool_call_id: 'c1', content: '[]' },
      { role: 'assistant', content: 'done' },
    ]);
  });
});

describe('streamChat', () => {
  it('posts to {baseUrl}/chat/completions with bearer auth and yields text then done', async () => {
    const cap: { init?: RequestInit; url?: string } = {};
    const chunks = [
      data({ choices: [{ delta: { content: 'Hel' } }] }),
      data({ choices: [{ delta: { content: 'lo' }, finish_reason: null }] }).slice(0, 20),
    ];
    chunks.push(data({ choices: [{ delta: { content: 'lo' }, finish_reason: null }] }).slice(20));
    chunks.push(data({ choices: [{ delta: {}, finish_reason: 'stop' }] }), 'data: [DONE]\n\n');
    const events = await collect(
      streamChat(settings, [{ role: 'user', content: 'x' }], [], new AbortController().signal, fakeFetch(chunks, 200, cap)),
    );
    expect(events).toEqual([{ type: 'text', delta: 'Hel' }, { type: 'text', delta: 'lo' }, { type: 'done' }]);
    expect(cap.url).toBe('https://api.example/v1/chat/completions');
    const headers = cap.init!.headers as Record<string, string>;
    expect(headers.Authorization).toBe('Bearer sk-test');
    const body = JSON.parse(cap.init!.body as string);
    expect(body).toMatchObject({ model: 'm', stream: true, messages: [{ role: 'user', content: 'x' }] });
    expect(body.tools).toBeUndefined();
  });

  it('includes tools when given and yields assembled tool calls before done', async () => {
    const cap: { init?: RequestInit } = {};
    const tools = [{ type: 'function' as const, function: { name: 'list_routes', parameters: { type: 'object' } } }];
    const chunks = [
      data({ choices: [{ delta: { tool_calls: [{ index: 0, id: 'c1', function: { name: 'list_routes', arguments: '' } }] } }] }),
      data({ choices: [{ delta: { tool_calls: [{ index: 0, function: { arguments: '{}' } }] } }] }),
      data({ choices: [{ delta: {}, finish_reason: 'tool_calls' }] }),
      'data: [DONE]\n\n',
    ];
    const events = await collect(
      streamChat(settings, [], tools, new AbortController().signal, fakeFetch(chunks, 200, cap)),
    );
    expect(events).toEqual([
      { type: 'tool_calls', calls: [{ id: 'c1', name: 'list_routes', arguments: '{}' }] },
      { type: 'done' },
    ]);
    expect(JSON.parse(cap.init!.body as string).tools).toEqual(tools);
  });

  it('throws ProviderError with status and body on non-2xx', async () => {
    const gen = streamChat(settings, [], [], new AbortController().signal, fakeFetch(['{"error":"bad key"}'], 401));
    await expect(collect(gen)).rejects.toMatchObject({ status: 401, body: '{"error":"bad key"}' });
  });

  it('propagates abort', async () => {
    const ctl = new AbortController();
    const hanging: typeof fetch = ((_: unknown, init?: RequestInit) =>
      new Promise<Response>((_, reject) => {
        init?.signal?.addEventListener('abort', () => reject(new DOMException('aborted', 'AbortError')));
      })) as typeof fetch;
    const p = collect(streamChat(settings, [], [], ctl.signal, hanging));
    ctl.abort();
    await expect(p).rejects.toMatchObject({ name: 'AbortError' });
  });
});

describe('listModels', () => {
  it('GETs {baseUrl}/models with bearer auth and returns sorted ids', async () => {
    const cap: { init?: RequestInit; url?: string } = {};
    const f = (async (url: RequestInfo | URL, init?: RequestInit) => {
      cap.url = String(url);
      cap.init = init;
      return new Response(JSON.stringify({ object: 'list', data: [{ id: 'gpt-b' }, { id: 'gpt-a' }, { nope: 1 }] }), { status: 200 });
    }) as typeof fetch;
    expect(await listModels(settings, f)).toEqual(['gpt-a', 'gpt-b']);
    expect(cap.url).toBe('https://api.example/v1/models');
    expect(cap.init?.method ?? 'GET').toBe('GET');
    expect((cap.init!.headers as Record<string, string>).Authorization).toBe('Bearer sk-test');
  });
  it('throws ProviderError on non-2xx', async () => {
    const f = (async () => new Response('nope', { status: 403 })) as typeof fetch;
    await expect(listModels(settings, f)).rejects.toMatchObject({ status: 403, body: 'nope' });
  });
});
