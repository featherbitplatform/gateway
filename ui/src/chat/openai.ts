/**
 * Minimal OpenAI Chat Completions streaming client. The only file that knows
 * the provider wire format. Works against any compatible server through
 * `baseUrl` (OpenAI, Azure, OpenRouter, Ollama, …).
 *
 * @module chat/openai
 */
import type { ChatMessage, ToolCall } from './store';

export interface ProviderSettings {
  baseUrl: string;
  model: string;
  apiKey: string;
}

export interface ToolDef {
  type: 'function';
  function: { name: string; description?: string; parameters: unknown };
}

export type WireMessage =
  | { role: 'system'; content: string }
  | { role: 'user'; content: string }
  | {
      role: 'assistant';
      content: string;
      tool_calls?: Array<{ id: string; type: 'function'; function: { name: string; arguments: string } }>;
    }
  | { role: 'tool'; tool_call_id: string; content: string };

export type StreamEvent = { type: 'text'; delta: string } | { type: 'tool_calls'; calls: ToolCall[] } | { type: 'done' };

/** A non-2xx reply from the provider; `body` is the raw response text. */
export class ProviderError extends Error {
  status: number;
  body: string;
  constructor(status: number, body: string) {
    super(`Provider returned ${status}`);
    this.name = 'ProviderError';
    this.status = status;
    this.body = body;
  }
}

/** Thread history → request messages. Errored, empty assistant turns are skipped. */
export function toWire(system: string, messages: ChatMessage[]): WireMessage[] {
  const out: WireMessage[] = [{ role: 'system', content: system }];
  for (const m of messages) {
    if (m.role === 'user') out.push({ role: 'user', content: m.content });
    else if (m.role === 'tool') out.push({ role: 'tool', tool_call_id: m.toolCallId, content: m.content });
    else {
      if (m.content === '' && !m.toolCalls?.length) continue;
      const w: Extract<WireMessage, { role: 'assistant' }> = { role: 'assistant', content: m.content };
      if (m.toolCalls?.length) {
        w.tool_calls = m.toolCalls.map((c) => ({ id: c.id, type: 'function', function: { name: c.name, arguments: c.arguments } }));
      }
      out.push(w);
    }
  }
  return out;
}

/** Splits a text buffer into complete SSE `data:` payloads plus the unfinished remainder. */
export function splitSseEvents(buffer: string): { events: string[]; rest: string } {
  const normalized = buffer.replace(/\r\n/g, '\n');
  const parts = normalized.split('\n\n');
  const rest = parts.pop() ?? '';
  const events = parts
    .map((block) =>
      block
        .split('\n')
        .filter((l) => l.startsWith('data:'))
        .map((l) => l.slice(5).trim())
        .join('\n'),
    )
    .filter((e) => e !== '');
  return { events, rest };
}

/** Reassembles `delta.tool_calls` fragments (indexed, arguments streamed piecewise). */
export class ToolCallAccumulator {
  private calls = new Map<number, { id: string; name: string; arguments: string }>();

  apply(deltas: unknown): void {
    if (!Array.isArray(deltas)) return;
    for (const d of deltas) {
      if (typeof d !== 'object' || d === null) continue;
      const o = d as { index?: number; id?: string; function?: { name?: string; arguments?: string } };
      const idx = typeof o.index === 'number' ? o.index : this.calls.size;
      const cur = this.calls.get(idx) ?? { id: '', name: '', arguments: '' };
      if (typeof o.id === 'string') cur.id = o.id;
      if (typeof o.function?.name === 'string') cur.name += o.function.name;
      if (typeof o.function?.arguments === 'string') cur.arguments += o.function.arguments;
      this.calls.set(idx, cur);
    }
  }

  finish(): ToolCall[] {
    return [...this.calls.entries()]
      .sort(([a], [b]) => a - b)
      .map(([, c]) => ({ id: c.id, name: c.name, arguments: c.arguments.trim() === '' ? '{}' : c.arguments }));
  }
}

/**
 * Streams one completion. Yields text deltas as they arrive, then (if the
 * model requested tools) one `tool_calls` event, then `done`.
 */
export async function* streamChat(
  settings: ProviderSettings,
  messages: WireMessage[],
  tools: ToolDef[],
  signal: AbortSignal,
  fetchImpl: typeof fetch = fetch,
): AsyncGenerator<StreamEvent> {
  const url = `${settings.baseUrl.replace(/\/+$/, '')}/chat/completions`;
  const body: Record<string, unknown> = { model: settings.model, stream: true, messages };
  if (tools.length > 0) body.tools = tools;
  const res = await fetchImpl(url, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json', Authorization: `Bearer ${settings.apiKey}` },
    body: JSON.stringify(body),
    signal,
  });
  if (!res.ok) throw new ProviderError(res.status, await res.text());
  if (!res.body) throw new ProviderError(res.status, 'empty response body');

  const reader = res.body.getReader();
  const decoder = new TextDecoder();
  const acc = new ToolCallAccumulator();
  let buffer = '';
  let sawToolCalls = false;

  const handle = function* (payload: string): Generator<StreamEvent> {
    if (payload === '[DONE]') return;
    let json: unknown;
    try {
      json = JSON.parse(payload);
    } catch {
      return;
    }
    const choice = (json as { choices?: Array<{ delta?: { content?: unknown; tool_calls?: unknown } }> }).choices?.[0];
    if (!choice?.delta) return;
    if (typeof choice.delta.content === 'string' && choice.delta.content !== '') {
      yield { type: 'text', delta: choice.delta.content };
    }
    if (choice.delta.tool_calls !== undefined) {
      sawToolCalls = true;
      acc.apply(choice.delta.tool_calls);
    }
  };

  for (;;) {
    const { value, done } = await reader.read();
    if (done) break;
    buffer += decoder.decode(value, { stream: true });
    const { events, rest } = splitSseEvents(buffer);
    buffer = rest;
    for (const e of events) yield* handle(e);
  }
  buffer += decoder.decode();
  if (buffer.trim() !== '') {
    const { events } = splitSseEvents(`${buffer}\n\n`);
    for (const e of events) yield* handle(e);
  }
  if (sawToolCalls) {
    const calls = acc.finish().filter((c) => c.name !== '');
    if (calls.length > 0) yield { type: 'tool_calls', calls };
  }
  yield { type: 'done' };
}

/** `GET {baseUrl}/models` → sorted model ids. Feeds the settings form's suggestion list. */
export async function listModels(settings: ProviderSettings, fetchImpl: typeof fetch = fetch): Promise<string[]> {
  const url = `${settings.baseUrl.replace(/\/+$/, '')}/models`;
  const res = await fetchImpl(url, { method: 'GET', headers: { Authorization: `Bearer ${settings.apiKey}` } });
  if (!res.ok) throw new ProviderError(res.status, await res.text());
  const body = (await res.json()) as { data?: Array<{ id?: unknown }> };
  return (body.data ?? [])
    .map((m) => m.id)
    .filter((id): id is string => typeof id === 'string')
    .sort();
}
