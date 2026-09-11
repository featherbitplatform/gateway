import { describe, expect, it } from 'vitest';
import { MAX_ROUNDS, isWriteTool, needsConfirmation, runTurn, type Provider, type ToolRunner, type TurnHooks } from './loop';
import type { StreamEvent, WireMessage } from './openai';
import { ProviderError, toWire } from './openai';
import { appendMessage, newThread, type Thread, type ToolCall } from './store';

type Script = (messages: WireMessage[], round: number) => StreamEvent[];

function provider(script: Script): Provider & { requests: WireMessage[][] } {
  const requests: WireMessage[][] = [];
  return {
    requests,
    async *stream(messages) {
      requests.push(messages);
      for (const e of script(messages, requests.length - 1)) yield e;
    },
  };
}

function runner(results: Record<string, { text: string; isError?: boolean }>): ToolRunner & { calls: Array<{ name: string; args: unknown }> } {
  const calls: Array<{ name: string; args: unknown }> = [];
  return {
    calls,
    tools: Object.keys(results).map((name) => ({ type: 'function', function: { name, parameters: {} } })),
    async call(name, args) {
      calls.push({ name, args });
      const r = results[name] ?? { text: 'unknown tool', isError: true };
      return { text: r.text, isError: !!r.isError };
    },
  };
}

const hooks = (confirm: (c: ToolCall) => Promise<boolean> = async () => true): TurnHooks & { snapshots: Thread[] } => {
  const snapshots: Thread[] = [];
  return { snapshots, onThread: (t) => snapshots.push(t), confirm };
};

const start = (text = 'hello'): Thread => appendMessage(newThread('t1', 1), { role: 'user', content: text }, 1);
const signal = () => new AbortController().signal;

describe('isWriteTool', () => {
  it('classifies by the shared WRITE_TOOLS list', () => {
    expect(isWriteTool('put_policy')).toBe(true);
    expect(isWriteTool('get_policy')).toBe(false);
  });
});

describe('needsConfirmation', () => {
  it('covers every write tool plus run_sandbox, which executes nodes for real', () => {
    expect(needsConfirmation('put_policy')).toBe(true);
    expect(needsConfirmation('run_sandbox')).toBe(true);
    expect(isWriteTool('run_sandbox')).toBe(false);
    expect(needsConfirmation('get_policy')).toBe(false);
  });
});

describe('runTurn', () => {
  it('streams a text-only reply into one assistant message', async () => {
    const p = provider(() => [{ type: 'text', delta: 'Hi ' }, { type: 'text', delta: 'there' }, { type: 'done' }]);
    const h = hooks();
    const out = await runTurn(start(), { provider: p, tools: null, hooks: h, signal: signal(), now: () => 5 });
    expect(out.messages.at(-1)).toEqual({ role: 'assistant', content: 'Hi there' });
    expect(h.snapshots.map((s) => (s.messages.at(-1) as { content: string }).content)).toEqual(['Hi ', 'Hi there']);
    expect(p.requests[0][0]).toMatchObject({ role: 'system' });
    expect(p.requests[0][0].content).toContain('inlined');
  });

  it('runs read tools immediately and feeds results back', async () => {
    const p = provider((_m, round) =>
      round === 0
        ? [{ type: 'tool_calls', calls: [{ id: 'c1', name: 'get_policy', arguments: '{"name":"api"}' }] }, { type: 'done' }]
        : [{ type: 'text', delta: 'The policy has 3 nodes.' }, { type: 'done' }],
    );
    const r = runner({ get_policy: { text: '{"nodes":3}' } });
    const h = hooks(async () => {
      throw new Error('confirm must not be called for reads');
    });
    const out = await runTurn(start(), { provider: p, tools: r, hooks: h, signal: signal() });
    expect(r.calls).toEqual([{ name: 'get_policy', args: { name: 'api' } }]);
    expect(out.messages.slice(1)).toEqual([
      { role: 'assistant', content: '', toolCalls: [{ id: 'c1', name: 'get_policy', arguments: '{"name":"api"}' }] },
      { role: 'tool', toolCallId: 'c1', name: 'get_policy', status: 'done', content: '{"nodes":3}' },
      { role: 'assistant', content: 'The policy has 3 nodes.' },
    ]);
    expect(p.requests[1].at(-1)).toEqual({ role: 'tool', tool_call_id: 'c1', content: '{"nodes":3}' });
    expect(p.requests[0][0].content).not.toContain('inlined');
  });

  it('asks before write tools: Run executes, Skip stores a declined result', async () => {
    const p = provider((_m, round) =>
      round === 0
        ? [
            {
              type: 'tool_calls',
              calls: [
                { id: 'w1', name: 'put_policy', arguments: '{"name":"a"}' },
                { id: 'w2', name: 'delete_route', arguments: '{"name":"r"}' },
              ],
            },
            { type: 'done' },
          ]
        : [{ type: 'text', delta: 'ok' }, { type: 'done' }],
    );
    const r = runner({ put_policy: { text: '{"applied":true}' }, delete_route: { text: 'never' } });
    const h = hooks(async (c) => c.name === 'put_policy');
    const out = await runTurn(start(), { provider: p, tools: r, hooks: h, signal: signal() });
    expect(r.calls.map((c) => c.name)).toEqual(['put_policy']);
    expect(out.messages.filter((m) => m.role === 'tool')).toEqual([
      { role: 'tool', toolCallId: 'w1', name: 'put_policy', status: 'done', content: '{"applied":true}' },
      { role: 'tool', toolCallId: 'w2', name: 'delete_route', status: 'declined', content: 'Declined by the user.' },
    ]);
  });

  it('asks before run_sandbox even though it is not a write tool', async () => {
    const p = provider((_m, round) =>
      round === 0
        ? [{ type: 'tool_calls', calls: [{ id: 's1', name: 'run_sandbox', arguments: '{"nodes":[]}' }] }, { type: 'done' }]
        : [{ type: 'text', delta: 'ok' }, { type: 'done' }],
    );
    const r = runner({ run_sandbox: { text: 'never' } });
    const asked: string[] = [];
    const h = hooks(async (c) => {
      asked.push(c.name);
      return false;
    });
    const out = await runTurn(start(), { provider: p, tools: r, hooks: h, signal: signal() });
    expect(asked).toEqual(['run_sandbox']);
    expect(r.calls).toEqual([]);
    expect(out.messages.filter((m) => m.role === 'tool')).toEqual([
      { role: 'tool', toolCallId: 's1', name: 'run_sandbox', status: 'declined', content: 'Declined by the user.' },
    ]);
  });

  it('answers every dangling tool call when the turn is aborted mid-round', async () => {
    const ctl = new AbortController();
    const p = provider(() => [
      {
        type: 'tool_calls',
        calls: [
          { id: 'c1', name: 'get_policy', arguments: '{}' },
          { id: 'c2', name: 'list_routes', arguments: '{}' },
        ],
      },
      { type: 'done' },
    ]);
    const r: ToolRunner = {
      tools: [],
      async call() {
        ctl.abort();
        throw new DOMException('aborted', 'AbortError');
      },
    };
    const out = await runTurn(start(), { provider: p, tools: r, hooks: hooks(), signal: ctl.signal });
    expect(out.messages.filter((m) => m.role === 'tool')).toEqual([
      { role: 'tool', toolCallId: 'c1', name: 'get_policy', status: 'declined', content: 'Aborted by the user.' },
      { role: 'tool', toolCallId: 'c2', name: 'list_routes', status: 'declined', content: 'Aborted by the user.' },
    ]);
    // The replayed wire history must answer every advertised tool_call_id, or
    // the provider rejects every later send of this thread with a 400.
    const wire = toWire('s', out.messages);
    const advertised = wire.flatMap((m) => (m.role === 'assistant' ? (m.tool_calls ?? []).map((c) => c.id) : []));
    const answered = wire.flatMap((m) => (m.role === 'tool' ? [m.tool_call_id] : []));
    expect(advertised).toEqual(['c1', 'c2']);
    expect(answered).toEqual(advertised);
  });

  it('marks isError results and malformed arguments as errors without stopping', async () => {
    const p = provider((_m, round) =>
      round === 0
        ? [{ type: 'tool_calls', calls: [{ id: 'c1', name: 'get_policy', arguments: '{bad' }, { id: 'c2', name: 'nope', arguments: '{}' }] }, { type: 'done' }]
        : [{ type: 'text', delta: 'sorry' }, { type: 'done' }],
    );
    const r = runner({ get_policy: { text: 'x' } });
    const out = await runTurn(start(), { provider: p, tools: r, hooks: hooks(), signal: signal() });
    const tools = out.messages.filter((m) => m.role === 'tool') as Array<{ status: string; content: string }>;
    expect(tools[0].status).toBe('error');
    expect(tools[0].content).toContain('Invalid JSON arguments');
    expect(tools[1].status).toBe('error');
    expect(r.calls).toEqual([{ name: 'nope', args: {} }]);
  });

  it('stops after MAX_ROUNDS tool rounds with a notice', async () => {
    const p = provider(() => [{ type: 'tool_calls', calls: [{ id: 'c', name: 'list_routes', arguments: '{}' }] }, { type: 'done' }]);
    const r = runner({ list_routes: { text: '[]' } });
    const out = await runTurn(start(), { provider: p, tools: r, hooks: hooks(), signal: signal() });
    expect(r.calls).toHaveLength(MAX_ROUNDS);
    expect(out.messages.at(-1)).toMatchObject({ role: 'assistant', content: expect.stringContaining(`${MAX_ROUNDS} tool rounds`) });
  });

  it('records provider errors on the thread and ends the turn', async () => {
    const p: Provider = {
      // eslint-disable-next-line require-yield -- stream throws before yielding, per the brief's runTurn error-path test
      async *stream() {
        throw new ProviderError(401, '{"error":"bad key"}');
      },
    };
    const out = await runTurn(start(), { provider: p, tools: null, hooks: hooks(), signal: signal() });
    expect(out.messages.at(-1)).toEqual({ role: 'assistant', content: '', error: 'Provider returned 401: {"error":"bad key"}' });
  });

  it('redacts the provider error text before storing it', async () => {
    const key = 'sk-abcdefghijklmnopqrstuvwxyz';
    const p: Provider = {
      // eslint-disable-next-line require-yield -- stream throws before yielding
      async *stream() {
        throw new ProviderError(401, `key ${key} leaked`);
      },
    };
    const out = await runTurn(start(), {
      provider: p,
      tools: null,
      hooks: hooks(),
      signal: signal(),
      redact: (t) => t.replace(key, '[REDACTED]'),
    });
    const last = out.messages.at(-1) as { error: string };
    expect(last.error).toContain('[REDACTED]');
    expect(last.error).not.toContain(key);
  });

  it('keeps partial text on abort', async () => {
    const ctl = new AbortController();
    const p: Provider = {
      async *stream() {
        yield { type: 'text', delta: 'partial' };
        ctl.abort();
        throw new DOMException('aborted', 'AbortError');
      },
    };
    const out = await runTurn(start(), { provider: p, tools: null, hooks: hooks(), signal: ctl.signal });
    expect(out.messages.at(-1)).toEqual({ role: 'assistant', content: 'partial' });
  });

  it('truncates oversized tool results', async () => {
    const p = provider((_m, round) =>
      round === 0 ? [{ type: 'tool_calls', calls: [{ id: 'c', name: 'get_trace', arguments: '{}' }] }, { type: 'done' }] : [{ type: 'done' }],
    );
    const r = runner({ get_trace: { text: 'z'.repeat(40_000) } });
    const out = await runTurn(start(), { provider: p, tools: r, hooks: hooks(), signal: signal() });
    const tool = out.messages.find((m) => m.role === 'tool') as { content: string };
    expect(tool.content.length).toBeLessThan(40_000);
    expect(tool.content).toContain('…[truncated');
    expect(p.requests[1].at(-1)).toMatchObject({ role: 'tool', content: tool.content });
  });

  it('redacts and bounds the malformed-arguments error text', async () => {
    const p = provider((_m, round) =>
      round === 0
        ? [{ type: 'tool_calls', calls: [{ id: 'c1', name: 'get_policy', arguments: '{"name": "Bearer abcdefgh12345678"' }] }, { type: 'done' }]
        : [{ type: 'done' }],
    );
    const r = runner({ get_policy: { text: 'x' } });
    const out = await runTurn(start(), {
      provider: p,
      tools: r,
      hooks: hooks(),
      signal: signal(),
      redact: (t) => t.replace('abcdefgh12345678', '[REDACTED]'),
    });
    const tool = out.messages.find((m) => m.role === 'tool') as { status: string; content: string };
    expect(tool.status).toBe('error');
    expect(tool.content).toBe('Invalid JSON arguments: {"name": "Bearer [REDACTED]"');
    expect(p.requests[1].at(-1)).toMatchObject({ role: 'tool', content: tool.content });
    expect(r.calls).toEqual([]);
  });

  it('applies the redactor to tool results before storing and replaying them', async () => {
    const p = provider((_m, round) =>
      round === 0 ? [{ type: 'tool_calls', calls: [{ id: 'c', name: 'get_trace', arguments: '{}' }] }, { type: 'done' }] : [{ type: 'done' }],
    );
    const r = runner({ get_trace: { text: 'authorization: Bearer abc.def' } });
    const out = await runTurn(start(), {
      provider: p,
      tools: r,
      hooks: hooks(),
      signal: signal(),
      redact: (t) => t.replace('abc.def', '[REDACTED]'),
    });
    const tool = out.messages.find((m) => m.role === 'tool') as { content: string };
    expect(tool.content).toBe('authorization: Bearer [REDACTED]');
    expect(p.requests[1].at(-1)).toMatchObject({ role: 'tool', content: 'authorization: Bearer [REDACTED]' });
  });
});
