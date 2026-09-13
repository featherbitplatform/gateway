import { describe, expect, it } from 'vitest';
import {
  DEFAULT_SETTINGS,
  MAX_THREADS,
  MAX_TOOL_RESULT_CHARS,
  SETTINGS_KEY,
  THREADS_KEY,
  appendMessage,
  loadSettings,
  loadThreads,
  newThread,
  replaceLastAssistant,
  saveSettings,
  saveThreads,
  titleFor,
  trimThreads,
  truncateToolResult,
  upsertThread,
  type Thread,
} from './store';

/** Minimal in-memory Storage double (only what the store touches). */
function memoryStorage(seed: Record<string, string> = {}, opts: { failAbove?: number } = {}): Storage {
  const data = new Map(Object.entries(seed));
  return {
    getItem: (k) => data.get(k) ?? null,
    setItem: (k, v) => {
      if (opts.failAbove !== undefined && v.length > opts.failAbove) {
        throw new DOMException('quota', 'QuotaExceededError');
      }
      data.set(k, v);
    },
    removeItem: (k) => void data.delete(k),
    clear: () => data.clear(),
    key: (i) => [...data.keys()][i] ?? null,
    get length() {
      return data.size;
    },
  };
}

function thread(n: number, updatedAt = n): Thread {
  return { ...newThread(`t${n}`, n), updatedAt, title: `thread ${n}` };
}

describe('settings', () => {
  it('returns defaults when storage is empty, missing, or corrupt', () => {
    expect(loadSettings(memoryStorage())).toEqual(DEFAULT_SETTINGS);
    expect(loadSettings(null)).toEqual(DEFAULT_SETTINGS);
    expect(loadSettings(memoryStorage({ [SETTINGS_KEY]: '{not json' }))).toEqual(DEFAULT_SETTINGS);
  });

  it('round-trips and fills missing fields from defaults', () => {
    const s = memoryStorage();
    saveSettings({ ...DEFAULT_SETTINGS, apiKey: 'sk-x', mcpToken: 'tok' }, s);
    expect(loadSettings(s)).toEqual({ ...DEFAULT_SETTINGS, apiKey: 'sk-x', mcpToken: 'tok' });
    const partial = memoryStorage({ [SETTINGS_KEY]: JSON.stringify({ model: 'm' }) });
    expect(loadSettings(partial)).toEqual({ ...DEFAULT_SETTINGS, model: 'm' });
  });

  it('round-trips the auto-approve toggle and defaults it off', () => {
    const s = memoryStorage();
    saveSettings({ ...DEFAULT_SETTINGS, autoApprove: true }, s);
    expect(loadSettings(s).autoApprove).toBe(true);
    expect(loadSettings(memoryStorage({ [SETTINGS_KEY]: JSON.stringify({ model: 'm' }) })).autoApprove).toBe(false);
    expect(loadSettings(memoryStorage({ [SETTINGS_KEY]: JSON.stringify({ autoApprove: 'yes' }) })).autoApprove).toBe(false);
  });

  it('round-trips the redact toggle and defaults it on', () => {
    const s = memoryStorage();
    saveSettings({ ...DEFAULT_SETTINGS, redact: false }, s);
    expect(loadSettings(s).redact).toBe(false);
    expect(loadSettings(memoryStorage({ [SETTINGS_KEY]: JSON.stringify({ model: 'm' }) })).redact).toBe(true);
  });
});

describe('threads', () => {
  it('loads [] for empty/corrupt/wrong-version envelopes', () => {
    expect(loadThreads(memoryStorage())).toEqual([]);
    expect(loadThreads(memoryStorage({ [THREADS_KEY]: '[]' }))).toEqual([]);
    expect(loadThreads(memoryStorage({ [THREADS_KEY]: JSON.stringify({ version: 2, threads: [] }) }))).toEqual([]);
    expect(loadThreads(null)).toEqual([]);
  });

  it('round-trips through the v1 envelope and skips malformed items', () => {
    const s = memoryStorage();
    const t = appendMessage(thread(1), { role: 'user', content: 'hi' }, 5);
    saveThreads([t], s);
    expect(JSON.parse(s.getItem(THREADS_KEY)!)).toMatchObject({ version: 1 });
    expect(loadThreads(s)).toEqual([t]);
    const bad = memoryStorage({ [THREADS_KEY]: JSON.stringify({ version: 1, threads: [t, { id: 1 }, null] }) });
    expect(loadThreads(bad)).toEqual([t]);
  });

  it('trims to MAX_THREADS newest-first by updatedAt', () => {
    const many = Array.from({ length: MAX_THREADS + 5 }, (_, i) => thread(i));
    const kept = trimThreads(many);
    expect(kept).toHaveLength(MAX_THREADS);
    expect(kept[0].id).toBe(`t${MAX_THREADS + 4}`);
    expect(kept.at(-1)!.id).toBe('t5');
  });

  it('drops the oldest thread and retries on quota errors', () => {
    const s = memoryStorage({}, { failAbove: 400 });
    const big = (n: number) => appendMessage(thread(n), { role: 'user', content: 'x'.repeat(150) }, n);
    const out = saveThreads([big(3), big(2), big(1)], s);
    expect(out.ok).toBe(true);
    expect(out.dropped).toBeGreaterThan(0);
    expect(out.threads.map((t) => t.id)).toEqual(out.threads.map((t) => t.id).sort().reverse());
    expect(out.threads[0].id).toBe('t3');
    expect(loadThreads(s)).toEqual(out.threads);
  });

  it('reports failure when even an empty list cannot be written', () => {
    const s = memoryStorage({}, { failAbove: 0 });
    const out = saveThreads([thread(1)], s);
    expect(out.ok).toBe(false);
    expect(out.threads).toEqual([]);
  });

  it('upsert replaces by id and moves the thread first', () => {
    const list = [thread(1), thread(2)];
    const t2 = { ...thread(2), title: 'renamed' };
    expect(upsertThread(list, t2).map((t) => t.title)).toEqual(['renamed', 'thread 1']);
    expect(upsertThread(list, thread(3)).map((t) => t.id)).toEqual(['t3', 't1', 't2']);
  });
});

describe('messages', () => {
  it('appendMessage bumps updatedAt and keeps history immutable', () => {
    const t = thread(1);
    const t2 = appendMessage(t, { role: 'user', content: 'a' }, 9);
    expect(t.messages).toHaveLength(0);
    expect(t2.messages).toEqual([{ role: 'user', content: 'a' }]);
    expect(t2.updatedAt).toBe(9);
  });

  it('replaceLastAssistant swaps the trailing assistant message or appends one', () => {
    const t = appendMessage(thread(1), { role: 'user', content: 'a' }, 1);
    const t2 = replaceLastAssistant(t, { role: 'assistant', content: 'par' }, 2);
    const t3 = replaceLastAssistant(t2, { role: 'assistant', content: 'partial text' }, 3);
    expect(t3.messages).toEqual([
      { role: 'user', content: 'a' },
      { role: 'assistant', content: 'partial text' },
    ]);
  });

  it('truncates long tool results with a marker', () => {
    expect(truncateToolResult('short')).toBe('short');
    const long = 'y'.repeat(MAX_TOOL_RESULT_CHARS + 10);
    const out = truncateToolResult(long);
    expect(out.startsWith('y'.repeat(MAX_TOOL_RESULT_CHARS))).toBe(true);
    expect(out.endsWith('…[truncated 10 chars]')).toBe(true);
  });

  it('derives titles from the seed, else the first user message', () => {
    expect(titleFor({ prompt: 'why_this_port', args: { trace_id: 'abcdef1234567890', node_id: 'n' } }, undefined)).toBe(
      'why_this_port · abcdef12…',
    );
    expect(titleFor({ prompt: 'review_policy', args: { policy_name: 'api' } }, undefined)).toBe('review_policy · api');
    expect(titleFor({ prompt: 'design_policy', args: {} }, undefined)).toBe('design_policy');
    expect(titleFor(undefined, 'x'.repeat(80))).toBe('x'.repeat(60) + '…');
    expect(titleFor(undefined, '  hello  ')).toBe('hello');
    expect(titleFor(undefined, undefined)).toBe('New chat');
  });
});
