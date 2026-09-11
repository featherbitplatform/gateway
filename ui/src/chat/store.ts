/**
 * Browser-side persistence for the agent chat: settings (provider + MCP
 * token) and threads, both in localStorage, both guarded so a private window
 * or blocked storage degrades to in-memory state.
 *
 * Nothing here talks to the network; see openai.ts / mcpClient.ts.
 *
 * @module chat/store
 */

export const SETTINGS_KEY = 'featherbit.chat.settings';
export const THREADS_KEY = 'featherbit.chat.threads';
export const MAX_THREADS = 50;
export const MAX_TOOL_RESULT_CHARS = 32_000;
export const DEFAULT_BASE_URL = 'https://api.openai.com/v1';
/** Just a default string for the settings form; any Chat Completions model works. */
export const DEFAULT_MODEL = 'gpt-5-mini';

export interface ChatSettings {
  baseUrl: string;
  model: string;
  apiKey: string;
  mcpToken: string;
  /** Client-side secret redaction before storing/sending (see chat/redact.ts). */
  redact: boolean;
}

export const DEFAULT_SETTINGS: ChatSettings = {
  baseUrl: DEFAULT_BASE_URL,
  model: DEFAULT_MODEL,
  apiKey: '',
  mcpToken: '',
  redact: true,
};

/** One tool call the model requested; `arguments` is the raw JSON string. */
export interface ToolCall {
  id: string;
  name: string;
  arguments: string;
}

export type ToolStatus = 'done' | 'declined' | 'error';

export type ChatMessage =
  | { role: 'user'; content: string }
  | { role: 'assistant'; content: string; toolCalls?: ToolCall[]; error?: string }
  | { role: 'tool'; toolCallId: string; name: string; status: ToolStatus; content: string };

export interface ThreadSeed {
  prompt: string;
  args: Record<string, string>;
}

export interface Thread {
  id: string;
  title: string;
  createdAt: number;
  updatedAt: number;
  seed?: ThreadSeed;
  messages: ChatMessage[];
}

export interface ThreadStore {
  version: 1;
  threads: Thread[];
}

export interface SaveThreadsOutcome {
  /** What is actually persisted (input minus anything dropped for quota). */
  threads: Thread[];
  dropped: number;
  ok: boolean;
}

function isRecord(v: unknown): v is Record<string, unknown> {
  return typeof v === 'object' && v !== null;
}

export function loadSettings(storage: Storage | null | undefined): ChatSettings {
  try {
    const raw = storage?.getItem(SETTINGS_KEY);
    if (!raw) return { ...DEFAULT_SETTINGS };
    const parsed: unknown = JSON.parse(raw);
    if (!isRecord(parsed)) return { ...DEFAULT_SETTINGS };
    const pick = (k: keyof Omit<ChatSettings, 'redact'>) => (typeof parsed[k] === 'string' ? (parsed[k] as string) : DEFAULT_SETTINGS[k]);
    return {
      baseUrl: pick('baseUrl'),
      model: pick('model'),
      apiKey: pick('apiKey'),
      mcpToken: pick('mcpToken'),
      redact: typeof parsed.redact === 'boolean' ? parsed.redact : true,
    };
  } catch {
    return { ...DEFAULT_SETTINGS };
  }
}

export function saveSettings(s: ChatSettings, storage: Storage | null | undefined): void {
  try {
    storage?.setItem(SETTINGS_KEY, JSON.stringify(s));
  } catch {
    // Blocked storage: settings live for this page load only.
  }
}

function isToolCall(v: unknown): v is ToolCall {
  return isRecord(v) && typeof v.id === 'string' && typeof v.name === 'string' && typeof v.arguments === 'string';
}

function isMessage(v: unknown): v is ChatMessage {
  if (!isRecord(v) || typeof v.content !== 'string') return false;
  switch (v.role) {
    case 'user':
      return true;
    case 'assistant':
      return (
        (v.toolCalls === undefined || (Array.isArray(v.toolCalls) && v.toolCalls.every(isToolCall))) &&
        (v.error === undefined || typeof v.error === 'string')
      );
    case 'tool':
      return (
        typeof v.toolCallId === 'string' &&
        typeof v.name === 'string' &&
        (v.status === 'done' || v.status === 'declined' || v.status === 'error')
      );
    default:
      return false;
  }
}

function isThread(v: unknown): v is Thread {
  if (!isRecord(v)) return false;
  const seedOk =
    v.seed === undefined ||
    (isRecord(v.seed) && typeof v.seed.prompt === 'string' && isRecord(v.seed.args));
  return (
    typeof v.id === 'string' &&
    typeof v.title === 'string' &&
    typeof v.createdAt === 'number' &&
    typeof v.updatedAt === 'number' &&
    seedOk &&
    Array.isArray(v.messages) &&
    v.messages.every(isMessage)
  );
}

export function loadThreads(storage: Storage | null | undefined): Thread[] {
  try {
    const raw = storage?.getItem(THREADS_KEY);
    if (!raw) return [];
    const parsed: unknown = JSON.parse(raw);
    if (!isRecord(parsed) || parsed.version !== 1 || !Array.isArray(parsed.threads)) return [];
    return parsed.threads.filter(isThread);
  } catch {
    return [];
  }
}

/** Newest first by `updatedAt`, capped at {@link MAX_THREADS}. */
export function trimThreads(threads: Thread[]): Thread[] {
  return [...threads].sort((a, b) => b.updatedAt - a.updatedAt).slice(0, MAX_THREADS);
}

/**
 * Persists the threads. On a quota error the oldest thread is dropped and the
 * write retried until it fits; `ok: false` means even `[]` could not be
 * written (blocked storage) — callers keep their in-memory list then.
 */
export function saveThreads(threads: Thread[], storage: Storage | null | undefined): SaveThreadsOutcome {
  let list = trimThreads(threads);
  let dropped = threads.length - list.length;
  for (;;) {
    try {
      const env: ThreadStore = { version: 1, threads: list };
      storage?.setItem(THREADS_KEY, JSON.stringify(env));
      return { threads: list, dropped, ok: true };
    } catch {
      if (list.length === 0) return { threads: [], dropped, ok: false };
      list = list.slice(0, -1);
      dropped += 1;
    }
  }
}

export function truncateToolResult(text: string): string {
  if (text.length <= MAX_TOOL_RESULT_CHARS) return text;
  const over = text.length - MAX_TOOL_RESULT_CHARS;
  return `${text.slice(0, MAX_TOOL_RESULT_CHARS)}\n…[truncated ${over} chars]`;
}

export function titleFor(seed: ThreadSeed | undefined, firstUserMessage: string | undefined): string {
  if (seed) {
    const first = Object.values(seed.args).find((v) => v.trim() !== '');
    if (!first) return seed.prompt;
    // Long values (trace ids) shorten; policy/route names usually fit as-is.
    const short = first.length > 12 ? `${first.slice(0, 8)}…` : first;
    return `${seed.prompt} · ${short}`;
  }
  const text = (firstUserMessage ?? '').trim().replace(/\s+/g, ' ');
  if (!text) return 'New chat';
  return text.length > 60 ? `${text.slice(0, 60)}…` : text;
}

export function newThread(id: string, now: number, seed?: ThreadSeed): Thread {
  const t: Thread = { id, title: titleFor(seed, undefined), createdAt: now, updatedAt: now, messages: [] };
  if (seed) t.seed = seed;
  return t;
}

export function appendMessage(thread: Thread, msg: ChatMessage, now: number): Thread {
  const messages = [...thread.messages, msg];
  const title =
    !thread.seed && thread.messages.length === 0 && msg.role === 'user' ? titleFor(undefined, msg.content) : thread.title;
  return { ...thread, title, messages, updatedAt: now };
}

/** Replaces a trailing assistant message (streaming in progress) or appends one. */
export function replaceLastAssistant(
  thread: Thread,
  msg: Extract<ChatMessage, { role: 'assistant' }>,
  now: number,
): Thread {
  const last = thread.messages.at(-1);
  if (last?.role === 'assistant') {
    return { ...thread, messages: [...thread.messages.slice(0, -1), msg], updatedAt: now };
  }
  return appendMessage(thread, msg, now);
}

export function upsertThread(list: Thread[], thread: Thread): Thread[] {
  return [thread, ...list.filter((t) => t.id !== thread.id)];
}
