# Agent Chat Panel Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A chat panel in the embedded web UI where the browser talks to an OpenAI-compatible Chat Completions endpoint directly, uses the gateway's own MCP tools mid-conversation (reads auto-run, writes ask first), and keeps threads and settings in the browser's local storage; every "Copy as agent prompt" spot gains an "Ask agent" action that seeds a thread.

**Architecture:** Pure TypeScript units under `ui/src/chat/` (store, OpenAI streaming client, MCP client, turn loop, system prompt) with no React, each unit-tested with injected `fetch`/`Storage` doubles. A `useChat` hook glues them to React state; `ChatPanel` renders inside the existing `Dialog` shell like the Debug and Sessions panels. One Rust change: the MCP `Origin` check accepts same-origin requests so the UI can call `/mcp` with the default empty allow-list.

**Tech Stack:** React 19 + TypeScript (Vite, vitest), lucide-react icons, existing `Dialog`/`DialogButton`/`DialogField` components; Rust/axum for the auth change; Playwright for e2e.

**Spec:** `docs/superpowers/specs/2026-09-11-agent-chat-design.md`

## Global Constraints

- No new npm runtime dependencies (`ui/package.json` `dependencies` unchanged).
- Local storage keys: `featherbit.chat.settings`, `featherbit.chat.threads`; envelope `{ version: 1, threads }`.
- Limits: 50 threads kept (oldest by `updatedAt` dropped); tool results truncated at 32 000 characters with a `…[truncated N chars]` marker; on quota error drop the oldest thread and retry, then warn once.
- Default base URL `https://api.openai.com/v1`; default model is one constant `DEFAULT_MODEL`.
- Write tools = `WRITE_TOOLS` from `ui/src/agentPrompts.ts`; they wait for Run/Skip. Skip stores `Declined by the user.`.
- Round cap: 16 tool rounds per user turn.
- Credentials are never written to notifications, toasts, or any gateway endpoint.
- Conventional Commits, no Co-Authored-By trailer other than the one the session instructions require. Branch: `feature/agent-chat` (already exists, based on `feature/mcp-server`).
- All UI work must pass `cd ui && npm run lint && npm test && npm run build`; Rust work must pass `cargo test mcp`.
- The `Copy` behaviours and e2e scenarios E2E-MCP-01..03 must keep passing unchanged.

---

## File structure

| Path | Responsibility |
|---|---|
| `src/mcp/auth.rs` (modify) | Same-origin acceptance rule in `authenticate`; middleware passes the request authority. |
| `src/config/system.rs` (modify) | Doc comment on `allowed_origins` describing the same-origin rule. |
| `ui/src/chat/store.ts` (create) | Types (`ChatSettings`, `Thread`, `ChatMessage`, `ToolCall`), defaults, guarded load/save for settings and threads, trim/truncate/title helpers. |
| `ui/src/chat/openai.ts` (create) | `streamChat` over fetch + SSE; `ToolCallAccumulator`; `toWire`; `ProviderError`. |
| `ui/src/chat/mcpClient.ts` (create) | `createMcpClient` (initialize/list/call, JSON-or-SSE parsing, 404 re-init); `toToolDefs`; `resultText`. |
| `ui/src/chat/systemPrompt.ts` (create) | `systemPrompt({ toolsAvailable })`. |
| `ui/src/chat/loop.ts` (create) | `runTurn` — the agent turn with read auto-run, write confirm, round cap, abort. |
| `ui/src/chat/useChat.ts` (create) | React hook owning settings, threads, connection, pending confirmation, abort. |
| `ui/src/components/ChatPanel.tsx` (create) | Dialog with thread list + conversation + settings view. |
| `ui/src/components/chat/ThreadList.tsx`, `MessageList.tsx`, `ToolCallCard.tsx`, `ChatSettingsForm.tsx` (create) | Focused presentational pieces. |
| `ui/src/App.tsx`, `ui/src/components/Sidebar.tsx`, `ui/src/commands.ts`, `ui/src/components/AgentPanel.tsx`, `ui/src/components/TraceViewer.tsx`, `ui/src/components/DebugPanel.tsx`, `ui/src/components/GraphCanvas.tsx` (modify) | Open the chat; "Ask agent" entry points. |
| `e2e/tests/chat.spec.ts` (create), `e2e/E2E_TESTBOOK.md` (modify) | Browser scenarios with a mocked provider. |
| `website/docs/guides/mcp.md`, `website/docs/reference/roadmap.md`, `CLAUDE.md` (modify) | Documentation. |

---

### Task 1: Same-origin acceptance in the MCP Origin check

**Files:**
- Modify: `src/mcp/auth.rs` (function `authenticate` at ~line 69, `bearer_middleware` at ~line 106, tests module)
- Modify: `src/config/system.rs:742-746` (doc comment on `allowed_origins`)

**Interfaces:**
- Produces: `pub fn authenticate(auth: &McpAuthState, headers: &HeaderMap) -> Result<McpPrincipal, AuthFailure>` (unchanged signature, now same-origin aware via the `Host` header) and `pub fn authenticate_for(auth, headers, authority: Option<&str>)` used by the middleware (authority from `Host`, falling back to the URI authority for HTTP/2).

- [ ] **Step 1: Write the failing tests**

Add to the `tests` module in `src/mcp/auth.rs`, after `origin_is_checked_before_the_token`:

```rust
    #[test]
    fn same_origin_is_accepted_without_an_allow_list() {
        let mut none = cfg();
        none.allowed_origins.clear();
        let auth = McpAuthState::from_config(&none);
        let ok = headers(&[
            ("host", "127.0.0.1:19091"),
            ("origin", "http://127.0.0.1:19091"),
            ("authorization", &format!("Bearer {READ}")),
        ]);
        assert!(authenticate(&auth, &ok).is_ok(), "Origin authority == Host authority");

        // Case-insensitive host comparison; scheme is ignored.
        let https = headers(&[
            ("host", "Gateway.Example:9091"),
            ("origin", "https://gateway.example:9091"),
            ("authorization", &format!("Bearer {READ}")),
        ]);
        assert!(authenticate(&auth, &https).is_ok());

        // A different authority is still refused.
        let cross = headers(&[
            ("host", "127.0.0.1:19091"),
            ("origin", "http://evil.example"),
            ("authorization", &format!("Bearer {READ}")),
        ]);
        assert_eq!(authenticate(&auth, &cross), Err(AuthFailure::OriginNotAllowed));

        // Same host but a different port is a different origin.
        let port = headers(&[
            ("host", "127.0.0.1:19091"),
            ("origin", "http://127.0.0.1:5173"),
            ("authorization", &format!("Bearer {READ}")),
        ]);
        assert_eq!(authenticate(&auth, &port), Err(AuthFailure::OriginNotAllowed));

        // No Host header and no allow-list: an Origin is still refused.
        let no_host = headers(&[
            ("origin", "http://127.0.0.1:19091"),
            ("authorization", &format!("Bearer {READ}")),
        ]);
        assert_eq!(authenticate(&auth, &no_host), Err(AuthFailure::OriginNotAllowed));
    }

    #[test]
    fn authenticate_for_uses_the_uri_authority_when_host_is_absent() {
        let mut none = cfg();
        none.allowed_origins.clear();
        let auth = McpAuthState::from_config(&none);
        let h = headers(&[
            ("origin", "http://127.0.0.1:19091"),
            ("authorization", &format!("Bearer {READ}")),
        ]);
        assert!(authenticate_for(&auth, &h, Some("127.0.0.1:19091")).is_ok());
        assert_eq!(
            authenticate_for(&auth, &h, Some("other.example")),
            Err(AuthFailure::OriginNotAllowed)
        );
    }
```

Also extend the existing `origin_is_checked_before_the_token` test: its final assertion (`// With no allowed origins, ANY Origin header is refused.`) remains valid because `ok` there carries no `Host` header — leave it.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test mcp::auth -- --nocapture`
Expected: compile error `cannot find function authenticate_for` (and the same-origin test would fail if it compiled).

- [ ] **Step 3: Implement**

In `src/mcp/auth.rs`, replace the `authenticate` function with:

```rust
/// Extracts `host[:port]` from an `Origin` value such as `https://a.b:9091`.
fn origin_authority(origin: &str) -> Option<&str> {
    let rest = origin.split_once("://")?.1;
    let authority = rest.split('/').next()?;
    (!authority.is_empty()).then_some(authority)
}

/// Resolves the principal for a request from its headers, taking the request
/// authority (`Host`, or the HTTP/2 `:authority`) from `authority`.
///
/// An `Origin` header is accepted when it is allow-listed **or** when its
/// authority equals the request's own authority (the embedded web UI calling
/// `/mcp` on whatever hostname it was served from). Cross-site pages fail
/// both tests. Under DNS rebinding both values name the attacker's domain and
/// the request reaches the endpoint — but without the bearer token, which a
/// foreign origin cannot read from this origin's storage, it is still `401`.
pub fn authenticate_for(
    auth: &McpAuthState,
    headers: &HeaderMap,
    authority: Option<&str>,
) -> Result<McpPrincipal, AuthFailure> {
    if let Some(origin) = headers.get("origin") {
        let origin = origin.to_str().map_err(|_| AuthFailure::OriginNotAllowed)?;
        let listed = auth.allowed_origins.iter().any(|a| a == origin);
        let same_origin = match (origin_authority(origin), authority) {
            (Some(o), Some(h)) => o.eq_ignore_ascii_case(h),
            _ => false,
        };
        if !listed && !same_origin {
            return Err(AuthFailure::OriginNotAllowed);
        }
    }

    let presented = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|h| {
            // RFC 6750: the scheme is case-insensitive.
            let (scheme, rest) = h.split_at_checked(7)?;
            scheme.eq_ignore_ascii_case("Bearer ").then(|| rest.trim())
        })
        .filter(|t| !t.is_empty())
        .ok_or(AuthFailure::Unauthorized)?;

    // Compare against every configured token without early exit so timing
    // does not reveal which entry (if any) matched.
    let mut matched: Option<McpPrincipal> = None;
    for (token, principal) in &auth.tokens {
        let same_len = token.len() == presented.len();
        let eq = same_len && bool::from(token.as_slice().ct_eq(presented.as_bytes()));
        if eq && matched.is_none() {
            matched = Some(principal.clone());
        }
    }
    matched.ok_or(AuthFailure::Unauthorized)
}

/// [`authenticate_for`] with the authority taken from the `Host` header.
pub fn authenticate(auth: &McpAuthState, headers: &HeaderMap) -> Result<McpPrincipal, AuthFailure> {
    let host = headers.get("host").and_then(|v| v.to_str().ok());
    authenticate_for(auth, headers, host)
}
```

In `bearer_middleware`, replace `match authenticate(&auth, req.headers())` with:

```rust
    let authority = req
        .headers()
        .get("host")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned)
        .or_else(|| req.uri().authority().map(|a| a.as_str().to_owned()));
    match authenticate_for(&auth, req.headers(), authority.as_deref()) {
```

Update the doc comment on `AuthFailure::OriginNotAllowed` to: `/// An `Origin` header was present, not allow-listed, and not same-origin.`

In `src/config/system.rs` replace the `allowed_origins` doc comment with:

```rust
    /// Browser origins allowed to call the endpoint, in addition to the
    /// request's own origin (an `Origin` whose `host[:port]` equals the
    /// request's `Host` is always accepted, so the embedded web UI's chat
    /// works with the empty default). Any other `Origin` is refused
    /// (DNS-rebinding defence); non-browser agents send none. List the Vite
    /// dev server here (`http://localhost:5173`) when developing the UI.
```

- [ ] **Step 4: Run the tests**

Run: `cargo test mcp -- --nocapture`
Expected: all pass, including `same_origin_is_accepted_without_an_allow_list`, `authenticate_for_uses_the_uri_authority_when_host_is_absent`, and the pre-existing origin/scope tests.

- [ ] **Step 5: Commit**

```bash
git add src/mcp/auth.rs src/config/system.rs
git commit -m "feat(mcp): accept same-origin requests on the Origin check

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>"
```

---

### Task 2: Chat store — types, settings, threads, limits

**Files:**
- Create: `ui/src/chat/store.ts`
- Test: `ui/src/chat/store.test.ts`

**Interfaces:**
- Produces (all exported from `ui/src/chat/store.ts`):
  - constants `SETTINGS_KEY`, `THREADS_KEY`, `MAX_THREADS = 50`, `MAX_TOOL_RESULT_CHARS = 32000`, `DEFAULT_BASE_URL`, `DEFAULT_MODEL`, `DEFAULT_SETTINGS`
  - types `ChatSettings`, `ToolCall`, `ToolStatus`, `ChatMessage`, `ThreadSeed`, `Thread`, `ThreadStore`, `SaveThreadsOutcome`
  - `loadSettings(storage: Storage | null | undefined): ChatSettings`
  - `saveSettings(s: ChatSettings, storage): void`
  - `loadThreads(storage): Thread[]`
  - `saveThreads(threads: Thread[], storage): SaveThreadsOutcome` — `{ threads, dropped, ok }`
  - `trimThreads(threads: Thread[]): Thread[]`
  - `truncateToolResult(text: string): string`
  - `titleFor(seed: ThreadSeed | undefined, firstUserMessage: string | undefined): string`
  - `newThread(id: string, now: number, seed?: ThreadSeed): Thread`
  - `appendMessage(thread: Thread, msg: ChatMessage, now: number): Thread`
  - `replaceLastAssistant(thread: Thread, msg: Extract<ChatMessage, { role: 'assistant' }>, now: number): Thread`
  - `upsertThread(list: Thread[], thread: Thread): Thread[]`

- [ ] **Step 1: Write the failing tests**

Create `ui/src/chat/store.test.ts`:

```ts
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
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cd ui && npx vitest run src/chat/store.test.ts`
Expected: FAIL — cannot resolve `./store`.

- [ ] **Step 3: Implement `ui/src/chat/store.ts`**

```ts
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
}

export const DEFAULT_SETTINGS: ChatSettings = {
  baseUrl: DEFAULT_BASE_URL,
  model: DEFAULT_MODEL,
  apiKey: '',
  mcpToken: '',
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
    const pick = (k: keyof ChatSettings) => (typeof parsed[k] === 'string' ? (parsed[k] as string) : DEFAULT_SETTINGS[k]);
    return { baseUrl: pick('baseUrl'), model: pick('model'), apiKey: pick('apiKey'), mcpToken: pick('mcpToken') };
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
```

- [ ] **Step 4: Run the tests**

Run: `cd ui && npx vitest run src/chat/store.test.ts`
Expected: PASS (all describe blocks).

- [ ] **Step 5: Commit**

```bash
git add ui/src/chat/store.ts ui/src/chat/store.test.ts
git commit -m "feat(ui): chat store — settings, threads, limits in localStorage

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>"
```

---

### Task 3: OpenAI streaming client

**Files:**
- Create: `ui/src/chat/openai.ts`
- Test: `ui/src/chat/openai.test.ts`
- Create: `ui/src/chat/modelMatch.ts`
- Test: `ui/src/chat/modelMatch.test.ts`

**Interfaces:**
- Consumes: `ChatMessage`, `ToolCall` from `./store`.
- Produces:
  - `interface ProviderSettings { baseUrl: string; model: string; apiKey: string }`
  - `type WireMessage` (OpenAI request message shape)
  - `interface ToolDef { type: 'function'; function: { name: string; description?: string; parameters: unknown } }`
  - `type StreamEvent = { type: 'text'; delta: string } | { type: 'tool_calls'; calls: ToolCall[] } | { type: 'done' }`
  - `class ProviderError extends Error { status: number; body: string }`
  - `function toWire(system: string, messages: ChatMessage[]): WireMessage[]`
  - `function splitSseEvents(buffer: string): { events: string[]; rest: string }`
  - `class ToolCallAccumulator { apply(deltas: unknown): void; finish(): ToolCall[] }`
  - `async function* streamChat(settings, messages: WireMessage[], tools: ToolDef[], signal: AbortSignal, fetchImpl?: typeof fetch): AsyncGenerator<StreamEvent>`
  - `async function listModels(settings: ProviderSettings, fetchImpl?: typeof fetch): Promise<string[]>` — `GET {baseUrl}/models`, returns the sorted `data[].id` list; throws `ProviderError` on non-2xx.
  - In a separate file `ui/src/chat/modelMatch.ts` (tested in `ui/src/chat/modelMatch.test.ts`): `scoreModel(query: string, id: string): number` (0 = no match; exact 100, prefix 80, substring 60, in-order subsequence 30; case-insensitive; empty query = 1) and `rankModels(query: string, ids: string[], limit = 8): string[]` (score desc, then id asc, non-matches dropped). Feeds the settings form's combobox in Task 7.

- [ ] **Step 1: Write the failing tests**

Create `ui/src/chat/openai.test.ts`:

```ts
import { describe, expect, it } from 'vitest';
import {
  ProviderError,
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
    expect(wire).toEqual<WireMessage[]>([
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
    await expect(collect(gen)).rejects.toMatchObject<Partial<ProviderError>>({ status: 401, body: '{"error":"bad key"}' });
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
    await expect(listModels(settings, f)).rejects.toMatchObject<Partial<ProviderError>>({ status: 403, body: 'nope' });
  });
});
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cd ui && npx vitest run src/chat/openai.test.ts`
Expected: FAIL — cannot resolve `./openai`.

- [ ] **Step 3: Implement `ui/src/chat/openai.ts`**

```ts
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
```

- [ ] **Step 4: Run the tests**

Run: `cd ui && npx vitest run src/chat/openai.test.ts`
Expected: PASS.

- [ ] **Step 4b: Write the failing model-matcher tests**

Create `ui/src/chat/modelMatch.test.ts`:

```ts
import { describe, expect, it } from 'vitest';
import { rankModels, scoreModel } from './modelMatch';

const IDS = ['gpt-5', 'gpt-5-mini', 'gpt-4.1', 'gpt-4.1-mini', 'o3', 'o4-mini', 'text-embedding-3-small'];

describe('scoreModel', () => {
  it('ranks exact > prefix > substring > subsequence > none, case-insensitively', () => {
    expect(scoreModel('gpt-5', 'gpt-5')).toBe(100);
    expect(scoreModel('GPT-5', 'gpt-5-mini')).toBe(80);
    expect(scoreModel('mini', 'gpt-5-mini')).toBe(60);
    expect(scoreModel('g5m', 'gpt-5-mini')).toBe(30);
    expect(scoreModel('claude', 'gpt-5-mini')).toBe(0);
  });
  it('treats an empty or blank query as a weak match for everything', () => {
    expect(scoreModel('', 'o3')).toBe(1);
    expect(scoreModel('   ', 'o3')).toBe(1);
  });
});

describe('rankModels', () => {
  it('orders by score then id, drops non-matches, and caps the list', () => {
    expect(rankModels('gpt-4', IDS)).toEqual(['gpt-4.1', 'gpt-4.1-mini']);
    expect(rankModels('mini', IDS)).toEqual(['gpt-4.1-mini', 'gpt-5-mini', 'o4-mini']);
    expect(rankModels('gpt-5', IDS)).toEqual(['gpt-5', 'gpt-5-mini']);
    expect(rankModels('zzz', IDS)).toEqual([]);
    expect(rankModels('', IDS, 3)).toEqual(['gpt-4.1', 'gpt-4.1-mini', 'gpt-5']);
  });
  it('lets subsequence matches through when nothing closer exists', () => {
    expect(rankModels('tem3', IDS)).toEqual(['text-embedding-3-small']);
  });
});
```

Run: `cd ui && npx vitest run src/chat/modelMatch.test.ts` — Expected: FAIL, cannot resolve `./modelMatch`.

- [ ] **Step 4c: Implement `ui/src/chat/modelMatch.ts`**

```ts
/**
 * Ranks provider model ids against what the user has typed, for the chat
 * settings combobox. Pure string scoring — no fetching.
 *
 * @module chat/modelMatch
 */

/** 100 exact · 80 prefix · 60 substring · 30 in-order subsequence · 0 none; blank query = 1. */
export function scoreModel(query: string, id: string): number {
  const q = query.trim().toLowerCase();
  if (q === '') return 1;
  const s = id.toLowerCase();
  if (s === q) return 100;
  if (s.startsWith(q)) return 80;
  if (s.includes(q)) return 60;
  let i = 0;
  for (const ch of s) {
    if (ch === q[i]) i += 1;
    if (i === q.length) return 30;
  }
  return 0;
}

/** Best matches first (score desc, id asc), non-matches dropped, at most `limit`. */
export function rankModels(query: string, ids: string[], limit = 8): string[] {
  return ids
    .map((id) => ({ id, score: scoreModel(query, id) }))
    .filter((m) => m.score > 0)
    .sort((a, b) => b.score - a.score || a.id.localeCompare(b.id))
    .slice(0, limit)
    .map((m) => m.id);
}
```

Run: `cd ui && npx vitest run src/chat/modelMatch.test.ts` — Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add ui/src/chat/openai.ts ui/src/chat/openai.test.ts ui/src/chat/modelMatch.ts ui/src/chat/modelMatch.test.ts
git commit -m "feat(ui): OpenAI-compatible streaming chat client

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>"
```

---

### Task 4: Browser MCP client

**Files:**
- Create: `ui/src/chat/mcpClient.ts`
- Test: `ui/src/chat/mcpClient.test.ts`

**Interfaces:**
- Consumes: `ToolDef` from `./openai`.
- Produces:
  - `interface McpTool { name: string; description?: string; inputSchema?: unknown }`
  - `interface McpToolResult { content: Array<{ type: string; text?: string }>; isError?: boolean }`
  - `class McpError extends Error { status?: number; code?: number }`
  - `interface McpClient { listTools(): Promise<McpTool[]>; callTool(name: string, args: unknown, signal?: AbortSignal): Promise<McpToolResult>; reset(): void }`
  - `function createMcpClient(opts: { url: string; token: string; fetchImpl?: typeof fetch }): McpClient`
  - `function parseRpcBody(text: string): unknown`
  - `function toToolDefs(tools: McpTool[]): ToolDef[]`
  - `function resultText(r: McpToolResult): string`

- [ ] **Step 1: Write the failing tests**

Create `ui/src/chat/mcpClient.test.ts`:

```ts
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
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cd ui && npx vitest run src/chat/mcpClient.test.ts`
Expected: FAIL — cannot resolve `./mcpClient`.

- [ ] **Step 3: Implement `ui/src/chat/mcpClient.ts`**

```ts
/**
 * Fetch-based MCP client for this gateway's own `/mcp` endpoint (Streamable
 * HTTP). Same wire shape as the e2e helper in e2e/tests/mcp.spec.ts: one
 * JSON-RPC request per POST, a session id from `initialize` echoed back on
 * every later call, replies as plain JSON or a single SSE event.
 *
 * @module chat/mcpClient
 */
import type { ToolDef } from './openai';

export interface McpTool {
  name: string;
  description?: string;
  inputSchema?: unknown;
}

export interface McpToolResult {
  content: Array<{ type: string; text?: string }>;
  isError?: boolean;
}

/** Transport/protocol failure (not a tool-level `isError`, which is returned). */
export class McpError extends Error {
  status?: number;
  code?: number;
  constructor(message: string, opts: { status?: number; code?: number } = {}) {
    super(message);
    this.name = 'McpError';
    this.status = opts.status;
    this.code = opts.code;
  }
}

export interface McpClient {
  /** Initializes lazily, then `tools/list`. */
  listTools(): Promise<McpTool[]>;
  callTool(name: string, args: unknown, signal?: AbortSignal): Promise<McpToolResult>;
  /** Forgets the session; the next call re-initializes. */
  reset(): void;
}

const PROTOCOL_VERSION = '2025-03-26';

/** Parses a JSON or single-event SSE body; empty → null. */
export function parseRpcBody(text: string): unknown {
  const t = text.trim();
  if (t === '') return null;
  if (t.startsWith('{') || t.startsWith('[')) return JSON.parse(t);
  const data = t
    .split(/\r?\n/)
    .filter((l) => l.startsWith('data:'))
    .map((l) => l.slice(5).trim())
    .join('');
  return data === '' ? null : JSON.parse(data);
}

export function toToolDefs(tools: McpTool[]): ToolDef[] {
  return tools.map((t) => {
    const fn: ToolDef['function'] = { name: t.name, parameters: t.inputSchema ?? { type: 'object', properties: {} } };
    if (t.description) fn.description = t.description;
    return { type: 'function', function: fn };
  });
}

export function resultText(r: McpToolResult): string {
  return r.content
    .filter((c) => c.type === 'text' && typeof c.text === 'string')
    .map((c) => c.text as string)
    .join('\n');
}

export function createMcpClient(opts: { url: string; token: string; fetchImpl?: typeof fetch }): McpClient {
  const fetchImpl = opts.fetchImpl ?? fetch;
  let session: string | null = null;
  let nextId = 1;
  let initializing: Promise<void> | null = null;

  const headers = (): Record<string, string> => {
    const h: Record<string, string> = {
      'Content-Type': 'application/json',
      Accept: 'application/json, text/event-stream',
      Authorization: `Bearer ${opts.token}`,
    };
    if (session) h['Mcp-Session-Id'] = session;
    return h;
  };

  const post = async (payload: unknown, signal?: AbortSignal): Promise<Response> =>
    fetchImpl(opts.url, { method: 'POST', headers: headers(), body: JSON.stringify(payload), signal });

  const initialize = async (): Promise<void> => {
    session = null;
    const id = nextId++;
    const res = await post({
      jsonrpc: '2.0',
      id,
      method: 'initialize',
      params: { protocolVersion: PROTOCOL_VERSION, capabilities: {}, clientInfo: { name: 'featherbit-ui', version: '1' } },
    });
    if (!res.ok) throw new McpError(`initialize failed: ${res.status} ${await res.text()}`, { status: res.status });
    const body = parseRpcBody(await res.text()) as { error?: { code?: number; message?: string } } | null;
    if (body?.error) throw new McpError(body.error.message ?? 'initialize error', { code: body.error.code });
    session = res.headers.get('mcp-session-id');
    await post({ jsonrpc: '2.0', method: 'notifications/initialized' });
  };

  const ensureSession = async (): Promise<void> => {
    if (session) return;
    initializing ??= initialize().finally(() => (initializing = null));
    await initializing;
  };

  const rpc = async <T>(method: string, params: unknown, signal?: AbortSignal, retried = false): Promise<T> => {
    await ensureSession();
    const id = nextId++;
    const res = await post({ jsonrpc: '2.0', id, method, params }, signal);
    if (res.status === 404 && !retried) {
      session = null;
      return rpc<T>(method, params, signal, true);
    }
    const text = await res.text();
    if (!res.ok) throw new McpError(`${method} failed: ${res.status} ${text}`, { status: res.status });
    const body = parseRpcBody(text) as { result?: T; error?: { code?: number; message?: string } } | null;
    if (!body) throw new McpError(`${method}: empty reply`);
    if (body.error) throw new McpError(body.error.message ?? `${method} error`, { code: body.error.code });
    return body.result as T;
  };

  return {
    async listTools() {
      const r = await rpc<{ tools?: McpTool[] }>('tools/list', {});
      return r.tools ?? [];
    },
    async callTool(name, args, signal) {
      return rpc<McpToolResult>('tools/call', { name, arguments: args ?? {} }, signal);
    },
    reset() {
      session = null;
    },
  };
}
```

- [ ] **Step 4: Run the tests**

Run: `cd ui && npx vitest run src/chat/mcpClient.test.ts`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add ui/src/chat/mcpClient.ts ui/src/chat/mcpClient.test.ts
git commit -m "feat(ui): browser MCP client for the gateway's /mcp endpoint

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>"
```

---

### Task 5: System prompt and the turn loop

**Files:**
- Create: `ui/src/chat/systemPrompt.ts`
- Create: `ui/src/chat/loop.ts`
- Test: `ui/src/chat/loop.test.ts`
- Create: `ui/src/chat/redact.ts`
- Test: `ui/src/chat/redact.test.ts`
- Modify: `ui/src/chat/store.ts` (`ChatSettings.redact`), `ui/src/chat/store.test.ts`

**Interfaces:**
- Consumes: `Thread`, `ChatMessage`, `ToolCall`, `appendMessage`, `replaceLastAssistant`, `truncateToolResult` from `./store`; `StreamEvent`, `ToolDef`, `WireMessage`, `toWire`, `ProviderError` from `./openai`; `WRITE_TOOLS` from `../agentPrompts`.
- Produces:
  - `systemPrompt(opts: { toolsAvailable: boolean }): string`
  - `MAX_ROUNDS = 16`
  - `isWriteTool(name: string): boolean`
  - `interface Provider { stream(messages: WireMessage[], tools: ToolDef[], signal: AbortSignal): AsyncIterable<StreamEvent> }`
  - `interface ToolRunner { tools: ToolDef[]; call(name: string, args: unknown, signal: AbortSignal): Promise<{ text: string; isError: boolean }> }`
  - `interface TurnHooks { onThread(t: Thread): void; confirm(call: ToolCall): Promise<boolean> }`
  - `runTurn(thread: Thread, deps: { provider: Provider; tools: ToolRunner | null; hooks: TurnHooks; signal: AbortSignal; now?: () => number; redact?: (text: string) => string }): Promise<Thread>` — `redact` (default identity) is applied to every tool result and tool error text before it is stored or replayed.
  - From `ui/src/chat/redact.ts`: `REDACTED = '[REDACTED]'`, `interface RedactResult { text: string; count: number }`, `redactSecrets(text: string, literals?: readonly string[]): RedactResult`.
  - `ChatSettings` (in `store.ts`) gains `redact: boolean` (default `true`); `loadSettings` reads it as a boolean, defaulting to `true`.

- [ ] **Step 1: Write the failing tests**

Create `ui/src/chat/loop.test.ts`:

```ts
import { describe, expect, it } from 'vitest';
import { MAX_ROUNDS, isWriteTool, runTurn, type Provider, type ToolRunner, type TurnHooks } from './loop';
import type { StreamEvent, WireMessage } from './openai';
import { ProviderError } from './openai';
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
      async *stream() {
        throw new ProviderError(401, '{"error":"bad key"}');
      },
    };
    const out = await runTurn(start(), { provider: p, tools: null, hooks: hooks(), signal: signal() });
    expect(out.messages.at(-1)).toEqual({ role: 'assistant', content: '', error: 'Provider returned 401: {"error":"bad key"}' });
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
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cd ui && npx vitest run src/chat/loop.test.ts`
Expected: FAIL — cannot resolve `./loop`.

- [ ] **Step 3: Implement `ui/src/chat/systemPrompt.ts`**

```ts
/**
 * The fixed system prompt for the in-UI chat.
 *
 * @module chat/systemPrompt
 */

const BASE = `You are the assistant built into the Featherbit API gateway's admin UI. You help the operator understand and change this gateway: routes (match rules + a policy), node-graph policies (YAML nodes wired by declared ports: success, outcome ports such as denied/limited/redirect, and error), supernodes (reusable subgraphs), shared plugin configs, stores (redis/valkey), consumers, debug traces (per-request node-by-node execution records) and the sandbox (runs a policy against a synthetic request).

Ground rules:
- Be concrete and brief. Quote node ids, ports and config keys exactly as they appear.
- When you propose configuration, show the YAML and explain what each node does before suggesting it be applied.`;

const WITH_TOOLS = `
- You have tools that read this gateway's live state (list_/get_/validate_/list_traces/get_trace/get_trace_step/run_sandbox) and, if the operator's token allows it, change it (put_/delete_/reload_config). Prefer calling a tool over guessing. Validate a policy with validate_policy before proposing to write it. Write tools ask the operator for confirmation; if a call comes back "Declined by the user.", do not retry it — offer an alternative instead.`;

const WITHOUT_TOOLS = `
- You have no tools in this session: the only data available is what is inlined in the conversation. Say so when a question needs data you do not have, and tell the operator what to paste.`;

export function systemPrompt(opts: { toolsAvailable: boolean }): string {
  return BASE + (opts.toolsAvailable ? WITH_TOOLS : WITHOUT_TOOLS);
}
```

- [ ] **Step 4: Implement `ui/src/chat/loop.ts`**

```ts
/**
 * One user turn of the agent chat: stream a reply, run the requested tools
 * (reads immediately, writes behind a confirmation), feed the results back,
 * repeat until the model answers with text, the user aborts, or the round
 * cap is hit. Pure with respect to React and the network — both come in as
 * dependencies.
 *
 * @module chat/loop
 */
import { WRITE_TOOLS } from '../agentPrompts';
import { ProviderError, toWire, type StreamEvent, type ToolDef, type WireMessage } from './openai';
import { appendMessage, replaceLastAssistant, truncateToolResult, type Thread, type ToolCall } from './store';
import { systemPrompt } from './systemPrompt';

export const MAX_ROUNDS = 16;

export function isWriteTool(name: string): boolean {
  return WRITE_TOOLS.includes(name);
}

export interface Provider {
  stream(messages: WireMessage[], tools: ToolDef[], signal: AbortSignal): AsyncIterable<StreamEvent>;
}

export interface ToolRunner {
  tools: ToolDef[];
  call(name: string, args: unknown, signal: AbortSignal): Promise<{ text: string; isError: boolean }>;
}

export interface TurnHooks {
  /** Called after every thread mutation, streaming deltas included. */
  onThread(thread: Thread): void;
  /** Write-tool gate: resolve true to run, false to decline. */
  confirm(call: ToolCall): Promise<boolean>;
}

export interface TurnDeps {
  provider: Provider;
  tools: ToolRunner | null;
  hooks: TurnHooks;
  signal: AbortSignal;
  now?: () => number;
  /** Applied to tool result/error text before it is stored or replayed (default: identity). */
  redact?: (text: string) => string;
}

function isAbort(e: unknown): boolean {
  return (e instanceof DOMException && e.name === 'AbortError') || (e instanceof Error && e.name === 'AbortError');
}

function describeError(e: unknown): string {
  if (e instanceof ProviderError) return `Provider returned ${e.status}: ${e.body}`;
  if (e instanceof Error) return e.message;
  return String(e);
}

export async function runTurn(thread: Thread, deps: TurnDeps): Promise<Thread> {
  const now = deps.now ?? (() => Date.now());
  const redact = deps.redact ?? ((s: string) => s);
  const toolDefs = deps.tools?.tools ?? [];
  const system = systemPrompt({ toolsAvailable: toolDefs.length > 0 });
  let t = thread;
  const emit = (next: Thread) => {
    t = next;
    deps.hooks.onThread(t);
  };

  for (let round = 0; ; round++) {
    let content = '';
    let calls: ToolCall[] = [];
    try {
      for await (const ev of deps.provider.stream(toWire(system, t.messages), toolDefs, deps.signal)) {
        if (ev.type === 'text') {
          content += ev.delta;
          emit(replaceLastAssistant(t, { role: 'assistant', content }, now()));
        } else if (ev.type === 'tool_calls') {
          calls = ev.calls;
        }
      }
    } catch (e) {
      if (isAbort(e)) {
        if (content !== '') emit(replaceLastAssistant(t, { role: 'assistant', content }, now()));
        return t;
      }
      emit(appendMessage(t, { role: 'assistant', content: '', error: describeError(e) }, now()));
      return t;
    }

    if (calls.length === 0) {
      // Streaming already emitted the final text; only emit when the thread
      // does not yet end with exactly this assistant message.
      const last = t.messages.at(-1);
      if (!(last?.role === 'assistant' && last.content === content && !last.toolCalls && !last.error)) {
        emit(replaceLastAssistant(t, { role: 'assistant', content }, now()));
      }
      return t;
    }

    emit(replaceLastAssistant(t, { role: 'assistant', content, toolCalls: calls }, now()));

    for (const call of calls) {
      if (deps.signal.aborted) return t;
      let args: unknown;
      try {
        args = call.arguments.trim() === '' ? {} : JSON.parse(call.arguments);
      } catch {
        emit(
          appendMessage(
            t,
            {
              role: 'tool',
              toolCallId: call.id,
              name: call.name,
              status: 'error',
              // Model-generated text: redact + bound it like any other stored tool text.
              content: truncateToolResult(redact(`Invalid JSON arguments: ${call.arguments}`)),
            },
            now(),
          ),
        );
        continue;
      }
      if (isWriteTool(call.name)) {
        const ok = await deps.hooks.confirm(call);
        if (!ok) {
          emit(appendMessage(t, { role: 'tool', toolCallId: call.id, name: call.name, status: 'declined', content: 'Declined by the user.' }, now()));
          continue;
        }
      }
      if (!deps.tools) {
        emit(appendMessage(t, { role: 'tool', toolCallId: call.id, name: call.name, status: 'error', content: 'No tools are connected.' }, now()));
        continue;
      }
      try {
        const r = await deps.tools.call(call.name, args, deps.signal);
        emit(
          appendMessage(
            t,
            {
              role: 'tool',
              toolCallId: call.id,
              name: call.name,
              status: r.isError ? 'error' : 'done',
              content: truncateToolResult(redact(r.text)),
            },
            now(),
          ),
        );
      } catch (e) {
        if (isAbort(e)) return t;
        emit(
          appendMessage(
            t,
            { role: 'tool', toolCallId: call.id, name: call.name, status: 'error', content: truncateToolResult(redact(describeError(e))) },
            now(),
          ),
        );
      }
    }

    if (round + 1 >= MAX_ROUNDS) {
      emit(
        appendMessage(
          t,
          { role: 'assistant', content: `Stopped after ${MAX_ROUNDS} tool rounds. Send a message to continue.` },
          now(),
        ),
      );
      return t;
    }
  }
}
```

- [ ] **Step 4b: Write the failing redaction tests**

Create `ui/src/chat/redact.test.ts`:

```ts
import { describe, expect, it } from 'vitest';
import { REDACTED, redactSecrets } from './redact';

describe('redactSecrets', () => {
  it('leaves ordinary text, placeholders and already-masked values alone', () => {
    const t = 'token_count: 3\nscope: read\ntoken: ${FEATHERBIT_MCP_READ_TOKEN}\nauthorization: <redacted>\n"port": "denied"\npassthrough: true';
    expect(redactSecrets(t)).toEqual({ text: t, count: 0 });
  });

  it('redacts secret-keyed values in JSON, YAML and header form', () => {
    const r = redactSecrets(
      '{"password":"hunter2","client_secret": "abc","api_key":"k1","refresh_token":"r"}\nsecret_key: s3cr3t\nX-Api-Key: zzz\nCookie: sid=abc; theme=dark',
    );
    expect(r.text).toBe(
      `{"password":"${REDACTED}","client_secret": "${REDACTED}","api_key":"${REDACTED}","refresh_token":"${REDACTED}"}\nsecret_key: ${REDACTED}\nX-Api-Key: ${REDACTED}\nCookie: ${REDACTED}`,
    );
    expect(r.count).toBe(7);
  });

  it('redacts auth schemes, JWTs, PEM blocks and well-known key prefixes', () => {
    const jwt = 'eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.SflKxwRJSMeKKF2QT4fwpMeJf36POk6yJV_adQssw5c';
    const r = redactSecrets(
      `use Bearer abcdefgh12345678 or ${jwt}\nkey sk-abcdefghijklmnopqrstuvwxyz and AKIAABCDEFGHIJKLMNOP\n-----BEGIN RSA PRIVATE KEY-----\nMIIE\n-----END RSA PRIVATE KEY-----`,
    );
    expect(r.text).toBe(`use Bearer ${REDACTED} or ${REDACTED}\nkey ${REDACTED} and ${REDACTED}\n${REDACTED}`);
    expect(r.count).toBe(5);
  });

  it('redacts literal secrets of 8+ chars anywhere and ignores shorter ones', () => {
    const r = redactSecrets('my key is sk-live-XYZ and short is abc', ['sk-live-XYZ', 'abc']);
    expect(r.text).toBe(`my key is ${REDACTED} and short is abc`);
    expect(r.count).toBe(1);
  });

  it('is idempotent', () => {
    const once = redactSecrets('password: x\nBearer abcdefgh12345678').text;
    expect(redactSecrets(once)).toEqual({ text: once, count: 0 });
  });
});
```

Run: `cd ui && npx vitest run src/chat/redact.test.ts` — Expected: FAIL, cannot resolve `./redact`.

- [ ] **Step 4c: Implement `ui/src/chat/redact.ts`**

```ts
/**
 * Client-side secret redaction for the agent chat — a second line of
 * defence behind the gateway's own capture-time trace redaction and
 * credential masking. Runs over everything the chat stores or sends:
 * seeded prompts, typed messages, tool results.
 *
 * @module chat/redact
 */

export const REDACTED = '[REDACTED]';

export interface RedactResult {
  text: string;
  /** Number of replacements made. */
  count: number;
}

/** Keys whose values are secrets wherever they appear (JSON, YAML, headers). */
const SECRET_KEYS = [
  'password', 'passwd', 'pass', 'secret', 'client_secret', 'client-secret',
  'api_key', 'apikey', 'api-key', 'x-api-key', 'x-auth-token',
  'access_token', 'refresh_token', 'id_token', 'auth_token', 'session_token', 'token', 'bearer',
  'private_key', 'private-key', 'secret_key', 'secret-key', 'access_key', 'access-key',
  'authorization', 'proxy-authorization', 'cookie', 'set-cookie',
];

const KEY_ALT = SECRET_KEYS.map((k) => k.replace(/-/g, '\\-')).join('|');

/** `key: value`, `"key": "value"`, `key=value` — the value runs to a delimiter. */
const KEYED_VALUE = new RegExp(`(?<![\\w-])((?:"|')?(?:${KEY_ALT})(?:"|')?\\s*[:=]\\s*)("?)([^"\\r\\n,}\\]]*)`, 'gi');
const AUTH_SCHEME = /\b(Bearer|Basic|Digest|Token)\s+([A-Za-z0-9\-._~+/=]{8,})/g;
const JWT = /\beyJ[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}\b/g;
const PEM = /-----BEGIN [A-Z ]*PRIVATE KEY-----[\s\S]*?-----END [A-Z ]*PRIVATE KEY-----/g;
const KEY_PREFIXES = /\b(sk-[A-Za-z0-9_-]{16,}|ghp_[A-Za-z0-9]{20,}|gh[ousr]_[A-Za-z0-9]{20,}|AKIA[0-9A-Z]{16}|xox[baprs]-[A-Za-z0-9-]{10,})\b/g;

/** Values that are safe to keep: empty, `${ENV}` placeholders, already-masked markers, plain booleans/numbers. */
function keepValue(v: string): boolean {
  const t = v.trim();
  return t === '' || t.startsWith('${') || t === REDACTED || t === '<redacted>' || t === '<masked>' || /^(true|false|null|\d+)$/.test(t);
}

function escapeRe(s: string): string {
  return s.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
}

/**
 * Replaces secrets with {@link REDACTED}. `literals` are exact strings to
 * remove wherever they appear (the user's own API key and MCP token);
 * entries shorter than 8 characters are ignored to avoid shredding prose.
 */
export function redactSecrets(text: string, literals: readonly string[] = []): RedactResult {
  let count = 0;
  let out = text;
  const sub = (re: RegExp, replacement: (match: string, group1: string) => string) => {
    out = out.replace(re, (m: string, g1: string) => {
      count += 1;
      return replacement(m, g1);
    });
  };
  for (const lit of literals) {
    if (lit.length >= 8) sub(new RegExp(escapeRe(lit), 'g'), () => REDACTED);
  }
  sub(PEM, () => REDACTED);
  sub(JWT, () => REDACTED);
  sub(KEY_PREFIXES, () => REDACTED);
  out = out.replace(KEYED_VALUE, (m: string, prefix: string, quote: string, value: string) => {
    if (keepValue(value)) return m;
    count += 1;
    return `${prefix}${quote}${REDACTED}`;
  });
  sub(AUTH_SCHEME, (_m, scheme) => `${scheme} ${REDACTED}`);
  return { text: out, count };
}
```

Run: `cd ui && npx vitest run src/chat/redact.test.ts` — Expected: PASS.

- [ ] **Step 4d: Add the `redact` setting to the store**

In `ui/src/chat/store.test.ts` add inside `describe('settings', …)`:

```ts
  it('round-trips the redact toggle and defaults it on', () => {
    const s = memoryStorage();
    saveSettings({ ...DEFAULT_SETTINGS, redact: false }, s);
    expect(loadSettings(s).redact).toBe(false);
    expect(loadSettings(memoryStorage({ [SETTINGS_KEY]: JSON.stringify({ model: 'm' }) })).redact).toBe(true);
  });
```

Run it — Expected: FAIL (`redact` does not exist on `ChatSettings`).

In `ui/src/chat/store.ts`:
- `ChatSettings` gains `/** Client-side secret redaction before storing/sending (see chat/redact.ts). */ redact: boolean;`
- `DEFAULT_SETTINGS` gains `redact: true,`
- `loadSettings` returns
  ```ts
    return {
      baseUrl: pick('baseUrl'),
      model: pick('model'),
      apiKey: pick('apiKey'),
      mcpToken: pick('mcpToken'),
      redact: typeof parsed.redact === 'boolean' ? parsed.redact : true,
    };
  ```
  and `pick`'s parameter type becomes `keyof Omit<ChatSettings, 'redact'>`.

Run: `cd ui && npx vitest run src/chat/store.test.ts` — Expected: PASS.

- [ ] **Step 5: Run the tests**

Run: `cd ui && npx vitest run src/chat`
Expected: PASS for store, openai, modelMatch, mcpClient, redact and loop.

- [ ] **Step 6: Lint**

Run: `cd ui && npm run lint`
Expected: no errors. (If `WRITE_TOOLS.includes` complains about `readonly string[]`, it does not — `includes` on `readonly string[]` accepts `string`.)

- [ ] **Step 7: Commit**

```bash
git add ui/src/chat/systemPrompt.ts ui/src/chat/loop.ts ui/src/chat/loop.test.ts ui/src/chat/redact.ts ui/src/chat/redact.test.ts ui/src/chat/store.ts ui/src/chat/store.test.ts
git commit -m "feat(ui): agent turn loop with read auto-run, write confirmation, round cap and secret redaction

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>"
```

---

### Task 6: `useChat` hook

**Files:**
- Create: `ui/src/chat/useChat.ts`

**Interfaces:**
- Consumes: everything from Tasks 2–5; `mcpEndpoint` from `../agentPrompts`.
- Produces:

```ts
export type ConnectionState =
  | { kind: 'no-key' }
  | { kind: 'toolless'; reason: 'no-token' | 'mcp-off' }
  | { kind: 'connecting' }
  | { kind: 'ready'; toolCount: number; scope: 'read' | 'write' }
  | { kind: 'error'; message: string };

export interface PendingConfirm { threadId: string; call: ToolCall }

export interface ChatController {
  settings: ChatSettings;
  saveSettings(next: ChatSettings): void;
  forgetCredentials(): void;
  threads: Thread[];
  activeId: string | null;
  setActive(id: string | null): void;
  newThread(): string;
  deleteThread(id: string): void;
  clearAll(): void;
  connection: ConnectionState;
  connect(): Promise<void>;
  send(threadId: string, text: string): Promise<void>;
  seedThread(seed: ThreadSeed, text: string): Promise<string>;
  stop(): void;
  pendingConfirm: PendingConfirm | null;
  resolveConfirm(run: boolean): void;
  busyThreadId: string | null;
  storageBlocked: boolean;
}

export function useChat(opts: { mcpUrl: string; mcpEnabled: boolean }): ChatController
```

No unit test (hook needs a DOM test renderer the project does not have); covered by the Playwright scenarios in Task 9. Keep every non-trivial rule out of the hook and in the tested modules.

- [ ] **Step 1: Implement `ui/src/chat/useChat.ts`**

```ts
/**
 * React state for the agent chat: settings, threads (persisted through
 * chat/store), the MCP connection, one in-flight turn at a time, and the
 * write-tool confirmation gate.
 *
 * @module chat/useChat
 */
import { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { isWriteTool, runTurn, type Provider, type ToolRunner } from './loop';
import { createMcpClient, resultText, toToolDefs, type McpClient } from './mcpClient';
import { streamChat, type ToolDef } from './openai';
import { redactSecrets } from './redact';
import {
  appendMessage,
  loadSettings,
  loadThreads,
  newThread as makeThread,
  saveSettings as persistSettings,
  saveThreads,
  upsertThread,
  type ChatSettings,
  type Thread,
  type ThreadSeed,
  type ToolCall,
} from './store';

export type ConnectionState =
  | { kind: 'no-key' }
  | { kind: 'toolless'; reason: 'no-token' | 'mcp-off' }
  | { kind: 'connecting' }
  | { kind: 'ready'; toolCount: number; scope: 'read' | 'write' }
  | { kind: 'error'; message: string };

export interface PendingConfirm {
  threadId: string;
  call: ToolCall;
}

export interface ChatController {
  settings: ChatSettings;
  saveSettings(next: ChatSettings): void;
  forgetCredentials(): void;
  threads: Thread[];
  activeId: string | null;
  setActive(id: string | null): void;
  newThread(): string;
  deleteThread(id: string): void;
  clearAll(): void;
  connection: ConnectionState;
  connect(): Promise<void>;
  send(threadId: string, text: string): Promise<void>;
  seedThread(seed: ThreadSeed, text: string): Promise<string>;
  stop(): void;
  pendingConfirm: PendingConfirm | null;
  resolveConfirm(run: boolean): void;
  busyThreadId: string | null;
  storageBlocked: boolean;
}

/** `window.localStorage`, or null where the accessor itself throws (blocked storage). */
function safeStorage(): Storage | null {
  try {
    return window.localStorage;
  } catch {
    return null;
  }
}

function newId(): string {
  const c = globalThis.crypto as Crypto | undefined;
  if (c && typeof c.randomUUID === 'function') return c.randomUUID();
  return `${Date.now().toString(36)}-${Math.random().toString(36).slice(2, 10)}`;
}

export function useChat(opts: { mcpUrl: string; mcpEnabled: boolean }): ChatController {
  const [settings, setSettings] = useState<ChatSettings>(() => loadSettings(safeStorage()));
  const [threads, setThreads] = useState<Thread[]>(() => loadThreads(safeStorage()));
  const [activeId, setActiveId] = useState<string | null>(() => threads[0]?.id ?? null);
  const [connection, setConnection] = useState<ConnectionState>({ kind: 'connecting' });
  const [pendingConfirm, setPendingConfirm] = useState<PendingConfirm | null>(null);
  const [busyThreadId, setBusyThreadId] = useState<string | null>(null);
  const [storageBlocked, setStorageBlocked] = useState(false);

  const mcpRef = useRef<McpClient | null>(null);
  const toolsRef = useRef<ToolDef[]>([]);
  const abortRef = useRef<AbortController | null>(null);
  const confirmRef = useRef<((ok: boolean) => void) | null>(null);
  const threadsRef = useRef(threads);
  threadsRef.current = threads;

  useEffect(() => {
    const out = saveThreads(threads, safeStorage());
    if (!out.ok) setStorageBlocked(true);
    else if (out.dropped > 0 && out.threads.length !== threads.length) setThreads(out.threads);
  }, [threads]);

  const saveSettings = useCallback((next: ChatSettings) => {
    setSettings(next);
    persistSettings(next, safeStorage());
    mcpRef.current = null;
    toolsRef.current = [];
  }, []);

  const forgetCredentials = useCallback(() => {
    saveSettings({ ...settings, apiKey: '', mcpToken: '' });
  }, [saveSettings, settings]);

  const connect = useCallback(async () => {
    if (settings.apiKey === '') {
      setConnection({ kind: 'no-key' });
      return;
    }
    if (!opts.mcpEnabled) {
      toolsRef.current = [];
      setConnection({ kind: 'toolless', reason: 'mcp-off' });
      return;
    }
    if (settings.mcpToken === '') {
      toolsRef.current = [];
      setConnection({ kind: 'toolless', reason: 'no-token' });
      return;
    }
    setConnection({ kind: 'connecting' });
    try {
      const client = createMcpClient({ url: opts.mcpUrl, token: settings.mcpToken });
      const tools = await client.listTools();
      mcpRef.current = client;
      toolsRef.current = toToolDefs(tools);
      const scope = tools.some((t) => isWriteTool(t.name)) ? 'write' : 'read';
      setConnection({ kind: 'ready', toolCount: tools.length, scope });
    } catch (e) {
      mcpRef.current = null;
      toolsRef.current = [];
      setConnection({ kind: 'error', message: e instanceof Error ? e.message : String(e) });
    }
  }, [opts.mcpEnabled, opts.mcpUrl, settings.apiKey, settings.mcpToken]);

  useEffect(() => {
    void connect();
  }, [connect]);

  // Client-side secret redaction (chat/redact.ts) over everything stored or
  // sent; the user's own key/token are removed as literals wherever they appear.
  const redact = useCallback(
    (text: string) => (settings.redact ? redactSecrets(text, [settings.apiKey, settings.mcpToken]).text : text),
    [settings.redact, settings.apiKey, settings.mcpToken],
  );

  const updateThread = useCallback((t: Thread) => {
    setThreads((list) => upsertThread(list, t));
  }, []);

  const newThread = useCallback((): string => {
    const t = makeThread(newId(), Date.now());
    setThreads((list) => upsertThread(list, t));
    setActiveId(t.id);
    return t.id;
  }, []);

  const deleteThread = useCallback((id: string) => {
    setThreads((list) => list.filter((t) => t.id !== id));
    setActiveId((cur) => (cur === id ? null : cur));
  }, []);

  const clearAll = useCallback(() => {
    setThreads([]);
    setActiveId(null);
  }, []);

  const stop = useCallback(() => {
    abortRef.current?.abort();
    confirmRef.current?.(false);
  }, []);

  const resolveConfirm = useCallback((run: boolean) => {
    confirmRef.current?.(run);
  }, []);

  const runOn = useCallback(
    async (thread: Thread) => {
      if (busyThreadId) return;
      const ctl = new AbortController();
      abortRef.current = ctl;
      setBusyThreadId(thread.id);
      const provider: Provider = {
        stream: (messages, tools, signal) => streamChat(settings, messages, tools, signal),
      };
      const mcp = mcpRef.current;
      const tools: ToolRunner | null =
        mcp && toolsRef.current.length > 0
          ? {
              tools: toolsRef.current,
              call: async (name, args, signal) => {
                const r = await mcp.callTool(name, args, signal);
                return { text: resultText(r), isError: !!r.isError };
              },
            }
          : null;
      try {
        await runTurn(thread, {
          provider,
          tools,
          signal: ctl.signal,
          redact,
          hooks: {
            onThread: updateThread,
            confirm: (call) =>
              new Promise<boolean>((resolve) => {
                confirmRef.current = (ok) => {
                  confirmRef.current = null;
                  setPendingConfirm(null);
                  resolve(ok);
                };
                setPendingConfirm({ threadId: thread.id, call });
              }),
          },
        });
      } finally {
        abortRef.current = null;
        setBusyThreadId(null);
        setPendingConfirm(null);
      }
    },
    [busyThreadId, settings, updateThread, redact],
  );

  const send = useCallback(
    async (threadId: string, text: string) => {
      // A thread created in the same tick (ChatPanel: `activeId ?? newThread()`)
      // is not in `threadsRef` yet; build it here so the send is not lost.
      const base = threadsRef.current.find((t) => t.id === threadId) ?? makeThread(threadId, Date.now());
      if (text.trim() === '') return;
      const next = appendMessage(base, { role: 'user', content: redact(text) }, Date.now());
      updateThread(next);
      await runOn(next);
    },
    [runOn, updateThread, redact],
  );

  const seedThread = useCallback(
    async (seed: ThreadSeed, text: string): Promise<string> => {
      const t = appendMessage(makeThread(newId(), Date.now(), seed), { role: 'user', content: redact(text) }, Date.now());
      updateThread(t);
      setActiveId(t.id);
      await runOn(t);
      return t.id;
    },
    [runOn, updateThread, redact],
  );

  return useMemo(
    () => ({
      settings,
      saveSettings,
      forgetCredentials,
      threads,
      activeId,
      setActive: setActiveId,
      newThread,
      deleteThread,
      clearAll,
      connection,
      connect,
      send,
      seedThread,
      stop,
      pendingConfirm,
      resolveConfirm,
      busyThreadId,
      storageBlocked,
    }),
    [
      settings, saveSettings, forgetCredentials, threads, activeId, newThread, deleteThread, clearAll,
      connection, connect, send, seedThread, stop, pendingConfirm, resolveConfirm, busyThreadId, storageBlocked,
    ],
  );
}
```

- [ ] **Step 2: Type-check and lint**

Run: `cd ui && npx tsc -b && npm run lint`
Expected: clean. (The `react-hooks/exhaustive-deps` rule may flag `runOn`'s `busyThreadId` dependency — it is intentional and listed.)

- [ ] **Step 3: Commit**

```bash
git add ui/src/chat/useChat.ts
git commit -m "feat(ui): useChat hook — settings, threads, MCP connection, confirmation gate

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>"
```

---

### Task 7: Chat panel components and app wiring

**Files:**
- Create: `ui/src/components/chat/ThreadList.tsx`, `ui/src/components/chat/MessageList.tsx`, `ui/src/components/chat/ToolCallCard.tsx`, `ui/src/components/chat/ChatSettingsForm.tsx`, `ui/src/components/ChatPanel.tsx`
- Modify: `ui/src/components/Sidebar.tsx` (props ~line 60-80, button block ~line 556-575), `ui/src/commands.ts` (CommandContext + list), `ui/src/App.tsx` (state, commandCtx, Sidebar props, render), `ui/src/components/AgentPanel.tsx` (link line)

**Interfaces:**
- Consumes: `ChatController`, `ConnectionState`, `PendingConfirm` from `../chat/useChat`; `Thread`, `ChatMessage`, `ToolCall`, `ChatSettings` from `../chat/store`; `Dialog`, `DialogButton`, `DialogField` from `./Dialog`.
- Produces: `ChatPanel({ open, onClose, chat, mcpStatus })`; `Sidebar` prop `onOpenChat: () => void`; `CommandContext.openChat: () => void`; command id `open-chat`; `AgentPanel` prop `onOpenChat: () => void`.

- [ ] **Step 1: `ui/src/components/chat/ToolCallCard.tsx`**

```tsx
import { useState } from 'react';
import { ChevronDown, ChevronRight, Loader2 } from 'lucide-react';
import type { ChatMessage, ToolCall } from '../../chat/store';
import { isWriteTool } from '../../chat/loop';

type ToolMessage = Extract<ChatMessage, { role: 'tool' }>;

interface ToolCallCardProps {
  call: ToolCall;
  /** The stored result, once the call finished (done/declined/error). */
  result: ToolMessage | undefined;
  /** True while this exact call awaits Run/Skip. */
  awaitingConfirm: boolean;
  onRun: () => void;
  onSkip: () => void;
}

function prettyArgs(raw: string): string {
  try {
    return JSON.stringify(JSON.parse(raw), null, 2);
  } catch {
    return raw;
  }
}

const statusColor: Record<ToolMessage['status'], string> = {
  done: 'var(--success)',
  declined: 'var(--warning)',
  error: 'var(--error)',
};

export function ToolCallCard({ call, result, awaitingConfirm, onRun, onSkip }: ToolCallCardProps) {
  const [open, setOpen] = useState(false);
  const write = isWriteTool(call.name);
  return (
    <div
      data-testid={`tool-call-${call.name}`}
      style={{
        border: '1px solid var(--border)',
        borderRadius: 'var(--radius-sm)',
        background: 'var(--surface-input)',
        padding: '6px 8px',
        fontSize: 'var(--text-2xs)',
        display: 'flex',
        flexDirection: 'column',
        gap: 4,
      }}
    >
      <div className="flex items-center justify-between" style={{ gap: 8 }}>
        <span style={{ fontFamily: 'var(--font-mono)', color: 'var(--text-primary)' }}>
          {write ? 'write · ' : ''}
          {call.name}
        </span>
        {result ? (
          <span style={{ color: statusColor[result.status] }}>{result.status}</span>
        ) : awaitingConfirm ? (
          <span style={{ color: 'var(--warning)' }}>awaiting confirmation</span>
        ) : (
          <span className="flex items-center gap-1" style={{ color: 'var(--text-muted)' }}>
            <Loader2 size={11} className="animate-spin" /> running
          </span>
        )}
      </div>
      <pre style={{ margin: 0, whiteSpace: 'pre-wrap', wordBreak: 'break-all', color: 'var(--text-secondary)', fontFamily: 'var(--font-mono)' }}>
        {prettyArgs(call.arguments)}
      </pre>
      {awaitingConfirm && (
        <div className="flex gap-2">
          <button onClick={onRun} style={{ padding: '3px 10px', borderRadius: 'var(--radius-sm)', background: 'var(--accent)', color: '#fff', border: 'none' }}>
            Run
          </button>
          <button onClick={onSkip} style={{ padding: '3px 10px', borderRadius: 'var(--radius-sm)', background: 'transparent', color: 'var(--text-primary)', border: '1px solid var(--border)' }}>
            Skip
          </button>
        </div>
      )}
      {result && (
        <>
          <button
            onClick={() => setOpen((o) => !o)}
            className="flex items-center gap-1"
            style={{ background: 'transparent', border: 'none', color: 'var(--text-muted)', padding: 0, alignSelf: 'flex-start' }}
          >
            {open ? <ChevronDown size={11} /> : <ChevronRight size={11} />} result
          </button>
          {open && (
            <pre style={{ margin: 0, whiteSpace: 'pre-wrap', wordBreak: 'break-all', maxHeight: 240, overflowY: 'auto', fontFamily: 'var(--font-mono)' }}>
              {result.content}
            </pre>
          )}
          {result.status === 'declined' && !open && <span style={{ color: 'var(--text-muted)' }}>{result.content}</span>}
        </>
      )}
    </div>
  );
}
```

- [ ] **Step 2: `ui/src/components/chat/MessageList.tsx`**

```tsx
import { useEffect, useRef } from 'react';
import type { ChatMessage, Thread } from '../../chat/store';
import type { PendingConfirm } from '../../chat/useChat';
import { ToolCallCard } from './ToolCallCard';

interface MessageListProps {
  thread: Thread;
  pendingConfirm: PendingConfirm | null;
  onResolveConfirm: (run: boolean) => void;
}

const bubble = (role: 'user' | 'assistant'): React.CSSProperties => ({
  alignSelf: role === 'user' ? 'flex-end' : 'flex-start',
  maxWidth: '85%',
  padding: '8px 10px',
  borderRadius: 'var(--radius-sm)',
  background: role === 'user' ? 'var(--accent-soft, var(--surface-input))' : 'var(--surface-input)',
  border: '1px solid var(--border)',
  fontSize: 'var(--text-xs)',
  color: 'var(--text-primary)',
  whiteSpace: 'pre-wrap',
  wordBreak: 'break-word',
});

export function MessageList({ thread, pendingConfirm, onResolveConfirm }: MessageListProps) {
  const endRef = useRef<HTMLDivElement>(null);
  useEffect(() => {
    endRef.current?.scrollIntoView({ block: 'end' });
  }, [thread.messages]);

  const results = new Map<string, Extract<ChatMessage, { role: 'tool' }>>();
  for (const m of thread.messages) if (m.role === 'tool') results.set(m.toolCallId, m);

  return (
    <div style={{ display: 'flex', flexDirection: 'column', gap: 8, padding: '4px 2px' }} data-testid="chat-messages">
      {thread.messages.map((m, i) => {
        if (m.role === 'user') return <div key={i} style={bubble('user')} data-role="user">{m.content}</div>;
        if (m.role === 'tool') return null;
        return (
          <div key={i} style={{ display: 'flex', flexDirection: 'column', gap: 6, alignSelf: 'stretch' }}>
            {m.content !== '' && <div style={bubble('assistant')} data-role="assistant">{m.content}</div>}
            {m.error && (
              <div style={{ ...bubble('assistant'), color: 'var(--error)', borderColor: 'var(--error)' }} data-role="error">
                {m.error}
              </div>
            )}
            {m.toolCalls?.map((c) => (
              <ToolCallCard
                key={c.id}
                call={c}
                result={results.get(c.id)}
                awaitingConfirm={pendingConfirm?.threadId === thread.id && pendingConfirm.call.id === c.id}
                onRun={() => onResolveConfirm(true)}
                onSkip={() => onResolveConfirm(false)}
              />
            ))}
          </div>
        );
      })}
      <div ref={endRef} />
    </div>
  );
}
```

- [ ] **Step 3: `ui/src/components/chat/ThreadList.tsx`**

```tsx
import { Plus, Trash2 } from 'lucide-react';
import type { Thread } from '../../chat/store';

interface ThreadListProps {
  threads: Thread[];
  activeId: string | null;
  onSelect: (id: string) => void;
  onNew: () => void;
  onDelete: (id: string) => void;
  onClearAll: () => void;
}

function relative(ts: number, now = Date.now()): string {
  const s = Math.max(0, Math.round((now - ts) / 1000));
  if (s < 60) return 'just now';
  if (s < 3600) return `${Math.floor(s / 60)} min ago`;
  if (s < 86400) return `${Math.floor(s / 3600)} h ago`;
  return `${Math.floor(s / 86400)} d ago`;
}

export function ThreadList({ threads, activeId, onSelect, onNew, onDelete, onClearAll }: ThreadListProps) {
  return (
    <div style={{ width: 220, flexShrink: 0, display: 'flex', flexDirection: 'column', gap: 6, borderRight: '1px solid var(--border)', paddingRight: 10 }}>
      <button
        onClick={onNew}
        className="flex items-center justify-center gap-1"
        style={{ padding: '6px 0', borderRadius: 'var(--radius-sm)', background: 'var(--surface-input)', border: '1px solid var(--border)', color: 'var(--text-primary)', fontSize: 'var(--text-xs)' }}
      >
        <Plus size={12} /> New chat
      </button>
      <div style={{ flex: 1, overflowY: 'auto', display: 'flex', flexDirection: 'column', gap: 2, maxHeight: '52vh' }} data-testid="chat-threads">
        {threads.length === 0 && <p style={{ fontSize: 'var(--text-2xs)', color: 'var(--text-muted)', margin: '8px 0' }}>No chats yet.</p>}
        {threads.map((t) => (
          <div
            key={t.id}
            className="flex items-center justify-between"
            style={{
              gap: 4,
              padding: '5px 6px',
              borderRadius: 'var(--radius-sm)',
              background: t.id === activeId ? 'var(--surface-input)' : 'transparent',
            }}
          >
            <button
              onClick={() => onSelect(t.id)}
              className="text-left"
              style={{ flex: 1, minWidth: 0, background: 'transparent', border: 'none', color: 'var(--text-primary)', fontSize: 'var(--text-2xs)' }}
            >
              <div style={{ overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap' }}>{t.title}</div>
              <div style={{ color: 'var(--text-muted)' }}>{relative(t.updatedAt)}</div>
            </button>
            <button aria-label={`Delete chat ${t.title}`} onClick={() => onDelete(t.id)} style={{ background: 'transparent', border: 'none', color: 'var(--text-muted)' }}>
              <Trash2 size={11} />
            </button>
          </div>
        ))}
      </div>
      {threads.length > 0 && (
        <button
          onClick={onClearAll}
          style={{ padding: '4px 0', background: 'transparent', border: 'none', color: 'var(--text-muted)', fontSize: 'var(--text-2xs)' }}
        >
          Clear all chats
        </button>
      )}
    </div>
  );
}
```

- [ ] **Step 4: `ui/src/components/chat/ChatSettingsForm.tsx`**

```tsx
import { useState } from 'react';
import { DialogButton, DialogField } from '../Dialog';
import { ProviderError, listModels } from '../../chat/openai';
import { rankModels } from '../../chat/modelMatch';
import type { ChatSettings } from '../../chat/store';
import type { ConnectionState } from '../../chat/useChat';

interface ChatSettingsFormProps {
  settings: ChatSettings;
  connection: ConnectionState;
  onSave: (next: ChatSettings) => void;
  onForget: () => void;
  onDone: () => void;
}

export function connectionLabel(c: ConnectionState): string {
  switch (c.kind) {
    case 'no-key':
      return 'Enter an API key to start';
    case 'toolless':
      return c.reason === 'mcp-off' ? 'No tools — MCP is off on this gateway' : 'No tools — no MCP token set';
    case 'connecting':
      return 'Connecting to MCP…';
    case 'ready':
      return `${c.toolCount} tools · ${c.scope} scope`;
    case 'error':
      return `MCP error: ${c.message}`;
  }
}

export function ChatSettingsForm({ settings, connection, onSave, onForget, onDone }: ChatSettingsFormProps) {
  const [draft, setDraft] = useState<ChatSettings>(settings);
  const [models, setModels] = useState<string[]>([]);
  const [modelsNote, setModelsNote] = useState<string>('');
  const [suggestionsOpen, setSuggestionsOpen] = useState(false);
  const [highlight, setHighlight] = useState(0);
  const set = (k: keyof ChatSettings) => (v: string) => setDraft((d) => ({ ...d, [k]: v }));

  // Closest matches to what is typed, from the loaded ids (empty until "Load models").
  const suggestions = rankModels(draft.model, models);
  const pickModel = (m: string | undefined) => {
    if (m === undefined) return;
    setDraft((d) => ({ ...d, model: m }));
    setSuggestionsOpen(false);
  };

  // Loads the provider's own GET /models into the combobox. The field stays
  // free text: servers without that endpoint (or with a different auth
  // model) still work by typing the name.
  const loadModels = async () => {
    setModelsNote('Loading…');
    try {
      const ids = await listModels(draft);
      setModels(ids);
      setSuggestionsOpen(true);
      setHighlight(0);
      setModelsNote(ids.length === 0 ? 'The provider returned no models.' : `${ids.length} models — type to see the closest matches.`);
    } catch (e) {
      setModels([]);
      setModelsNote(e instanceof ProviderError ? `Could not load models: ${e.status} ${e.body}` : `Could not load models: ${String(e)}`);
    }
  };

  return (
    <div style={{ display: 'flex', flexDirection: 'column', gap: 10 }} data-testid="chat-settings">
      <p style={{ fontSize: 'var(--text-2xs)', color: 'var(--text-muted)', margin: 0 }}>
        Everything here stays in this browser's local storage. The API key is sent only to the base URL below; the MCP token only to this gateway's MCP endpoint.
      </p>
      <DialogField label="Base URL" value={draft.baseUrl} onChange={set('baseUrl')} placeholder="https://api.openai.com/v1" mono />
      <label style={{ display: 'flex', flexDirection: 'column', gap: 4, fontSize: 'var(--text-xs)', color: 'var(--text-secondary)' }}>
        <span className="flex items-center justify-between">
          Model
          <button
            type="button"
            onClick={() => void loadModels()}
            disabled={draft.apiKey === ''}
            title={draft.apiKey === '' ? 'Enter an API key first' : 'Fetch the model list from the provider (GET /models)'}
            style={{ background: 'transparent', border: 'none', color: draft.apiKey === '' ? 'var(--text-muted)' : 'var(--accent)', fontSize: 'var(--text-2xs)', padding: 0 }}
          >
            Load models
          </button>
        </span>
        {/* Combobox: free-text input + a ranked dropdown of the loaded model
            ids (closest matches first, see chat/modelMatch.ts). Arrow keys
            move, Enter picks, Escape closes; clicking an option picks it. */}
        <div style={{ position: 'relative' }}>
          <input
            role="combobox"
            aria-expanded={suggestionsOpen && suggestions.length > 0}
            aria-controls="chat-model-options"
            aria-autocomplete="list"
            value={draft.model}
            onChange={(e) => {
              setDraft((d) => ({ ...d, model: e.target.value }));
              setSuggestionsOpen(true);
              setHighlight(0);
            }}
            onFocus={() => setSuggestionsOpen(true)}
            onBlur={() => setSuggestionsOpen(false)}
            onKeyDown={(e) => {
              if (suggestions.length === 0) return;
              if (e.key === 'ArrowDown') {
                e.preventDefault();
                setSuggestionsOpen(true);
                setHighlight((h) => Math.min(h + 1, suggestions.length - 1));
              } else if (e.key === 'ArrowUp') {
                e.preventDefault();
                setHighlight((h) => Math.max(h - 1, 0));
              } else if (e.key === 'Enter' && suggestionsOpen) {
                e.preventDefault();
                pickModel(suggestions[highlight]);
              } else if (e.key === 'Escape') {
                setSuggestionsOpen(false);
              }
            }}
            placeholder="model name"
            aria-label="Model"
            autoComplete="off"
            style={{ width: '100%', boxSizing: 'border-box', padding: '6px 8px', borderRadius: 'var(--radius-sm)', border: '1px solid var(--border)', background: 'var(--surface-input)', color: 'var(--text-primary)', fontFamily: 'var(--font-mono)' }}
          />
          {suggestionsOpen && suggestions.length > 0 && (
            <ul
              id="chat-model-options"
              role="listbox"
              data-testid="chat-model-options"
              style={{
                position: 'absolute',
                left: 0,
                right: 0,
                top: '100%',
                zIndex: 5,
                margin: '2px 0 0',
                padding: 2,
                listStyle: 'none',
                maxHeight: 200,
                overflowY: 'auto',
                background: 'var(--surface)',
                border: '1px solid var(--border)',
                borderRadius: 'var(--radius-sm)',
                boxShadow: 'var(--shadow-lg)',
              }}
            >
              {suggestions.map((m, i) => (
                <li
                  key={m}
                  role="option"
                  aria-selected={i === highlight}
                  // mousedown (not click) so the input's blur does not close the list first.
                  onMouseDown={(e) => {
                    e.preventDefault();
                    pickModel(m);
                  }}
                  onMouseEnter={() => setHighlight(i)}
                  style={{
                    padding: '4px 6px',
                    borderRadius: 'var(--radius-sm)',
                    cursor: 'pointer',
                    fontFamily: 'var(--font-mono)',
                    fontSize: 'var(--text-2xs)',
                    background: i === highlight ? 'var(--surface-input)' : 'transparent',
                    color: 'var(--text-primary)',
                  }}
                >
                  {m}
                </li>
              ))}
            </ul>
          )}
        </div>
        {modelsNote && <span style={{ fontSize: 'var(--text-2xs)', color: 'var(--text-muted)' }}>{modelsNote}</span>}
      </label>
      <label style={{ display: 'flex', flexDirection: 'column', gap: 4, fontSize: 'var(--text-xs)', color: 'var(--text-secondary)' }}>
        API key
        <input
          type="password"
          value={draft.apiKey}
          onChange={(e) => setDraft((d) => ({ ...d, apiKey: e.target.value }))}
          autoComplete="off"
          aria-label="API key"
          style={{ padding: '6px 8px', borderRadius: 'var(--radius-sm)', border: '1px solid var(--border)', background: 'var(--surface-input)', color: 'var(--text-primary)', fontFamily: 'var(--font-mono)' }}
        />
      </label>
      <label style={{ display: 'flex', flexDirection: 'column', gap: 4, fontSize: 'var(--text-xs)', color: 'var(--text-secondary)' }}>
        MCP token (read or write scope, from system.yaml)
        <input
          type="password"
          value={draft.mcpToken}
          onChange={(e) => setDraft((d) => ({ ...d, mcpToken: e.target.value }))}
          autoComplete="off"
          aria-label="MCP token"
          style={{ padding: '6px 8px', borderRadius: 'var(--radius-sm)', border: '1px solid var(--border)', background: 'var(--surface-input)', color: 'var(--text-primary)', fontFamily: 'var(--font-mono)' }}
        />
      </label>
      <label className="flex items-center gap-2" style={{ fontSize: 'var(--text-xs)', color: 'var(--text-secondary)' }}>
        <input
          type="checkbox"
          checked={draft.redact}
          onChange={(e) => setDraft((d) => ({ ...d, redact: e.target.checked }))}
          aria-label="Redact secrets before sending"
        />
        Redact secrets before sending (tokens, cookies, passwords, keys, and your own API key / MCP token)
      </label>
      <div style={{ fontSize: 'var(--text-2xs)', color: 'var(--text-muted)' }} data-testid="chat-connection">{connectionLabel(connection)}</div>
      <div className="flex justify-between">
        <DialogButton variant="danger" onClick={onForget}>
          Forget credentials
        </DialogButton>
        <div className="flex gap-2">
          {/* Saving re-runs the MCP connect (useChat's connect effect keys on
              the key/token), so "Test connection" is a save that stays on
              the form and lets the connection line above update. */}
          <DialogButton variant="ghost" onClick={() => onSave(draft)}>
            Test connection
          </DialogButton>
          <DialogButton variant="ghost" onClick={onDone}>
            Back
          </DialogButton>
          <DialogButton
            onClick={() => {
              onSave(draft);
              onDone();
            }}
          >
            Save
          </DialogButton>
        </div>
      </div>
    </div>
  );
}
```

- [ ] **Step 5: `ui/src/components/ChatPanel.tsx`**

```tsx
import { useEffect, useState } from 'react';
import { Settings, Square } from 'lucide-react';
import { Dialog, DialogButton } from './Dialog';
import { ChatSettingsForm, connectionLabel } from './chat/ChatSettingsForm';
import { MessageList } from './chat/MessageList';
import { ThreadList } from './chat/ThreadList';
import type { ChatController } from '../chat/useChat';
import type { McpStatus } from '../types';

interface ChatPanelProps {
  open: boolean;
  onClose: () => void;
  chat: ChatController;
  /** For the toolless explanations; null while loading. */
  mcpStatus: McpStatus | null;
}

export function ChatPanel({ open, onClose, chat, mcpStatus }: ChatPanelProps) {
  const [showSettings, setShowSettings] = useState(false);
  const [draft, setDraft] = useState('');

  // With no API key the panel opens on the settings form.
  useEffect(() => {
    if (open && chat.connection.kind === 'no-key') setShowSettings(true);
  }, [open, chat.connection.kind]);

  const active = chat.threads.find((t) => t.id === chat.activeId) ?? null;
  const busy = chat.busyThreadId !== null;

  const submit = () => {
    const text = draft.trim();
    if (!text || busy || chat.connection.kind === 'no-key') return;
    const id = chat.activeId ?? chat.newThread();
    setDraft('');
    void chat.send(id, text);
  };

  return (
    <Dialog
      open={open}
      title="Chat"
      width={1040}
      onClose={onClose}
      footer={
        <DialogButton variant="ghost" onClick={onClose}>
          Close
        </DialogButton>
      }
    >
      <div style={{ display: 'flex', gap: 12, minHeight: 420 }}>
        <ThreadList
          threads={chat.threads}
          activeId={chat.activeId}
          onSelect={(id) => {
            chat.setActive(id);
            setShowSettings(false);
          }}
          onNew={() => {
            chat.newThread();
            setShowSettings(false);
          }}
          onDelete={chat.deleteThread}
          onClearAll={chat.clearAll}
        />
        <div style={{ flex: 1, minWidth: 0, display: 'flex', flexDirection: 'column', gap: 8 }}>
          <div className="flex items-center justify-between" style={{ gap: 8 }}>
            <span style={{ fontSize: 'var(--text-2xs)', color: 'var(--text-muted)' }} data-testid="chat-connection-line">
              {chat.settings.model || 'no model'} · {connectionLabel(chat.connection)}
              {mcpStatus && !mcpStatus.compiled && ' (built without MCP)'}
              {!chat.settings.redact && ' · secret redaction off'}
              {chat.storageBlocked && ' · storage blocked: chats will not survive a reload'}
            </span>
            <button
              aria-label="Chat settings"
              onClick={() => setShowSettings((s) => !s)}
              style={{ background: 'transparent', border: 'none', color: 'var(--text-primary)' }}
            >
              <Settings size={14} />
            </button>
          </div>
          {showSettings ? (
            <ChatSettingsForm
              settings={chat.settings}
              connection={chat.connection}
              onSave={chat.saveSettings}
              onForget={chat.forgetCredentials}
              onDone={() => setShowSettings(false)}
            />
          ) : (
            <>
              <div style={{ flex: 1, overflowY: 'auto', maxHeight: '52vh' }}>
                {active ? (
                  <MessageList thread={active} pendingConfirm={chat.pendingConfirm} onResolveConfirm={chat.resolveConfirm} />
                ) : (
                  <p style={{ fontSize: 'var(--text-sm)', color: 'var(--text-muted)' }}>
                    Start a new chat, or use "Ask agent" from a trace or the policy editor.
                  </p>
                )}
              </div>
              <div className="flex" style={{ gap: 6 }}>
                <textarea
                  value={draft}
                  onChange={(e) => setDraft(e.target.value)}
                  onKeyDown={(e) => {
                    if (e.key === 'Enter' && !e.shiftKey) {
                      e.preventDefault();
                      submit();
                    }
                  }}
                  placeholder={chat.connection.kind === 'no-key' ? 'Enter an API key in settings first' : 'Ask about this gateway… (Enter to send, Shift+Enter for a new line)'}
                  aria-label="Message"
                  rows={2}
                  style={{ flex: 1, resize: 'vertical', padding: '6px 8px', borderRadius: 'var(--radius-sm)', border: '1px solid var(--border)', background: 'var(--surface-input)', color: 'var(--text-primary)', fontSize: 'var(--text-xs)' }}
                />
                {busy ? (
                  <DialogButton variant="ghost" onClick={chat.stop}>
                    <span className="flex items-center gap-1"><Square size={11} /> Stop</span>
                  </DialogButton>
                ) : (
                  <DialogButton onClick={submit} disabled={draft.trim() === '' || chat.connection.kind === 'no-key'}>
                    Send
                  </DialogButton>
                )}
              </div>
            </>
          )}
        </div>
      </div>
    </Dialog>
  );
}
```

- [ ] **Step 6: Sidebar button**

In `ui/src/components/Sidebar.tsx`:
- Add `MessageSquare` to the lucide import on line 9.
- In the props interface after `mcpEnabled: boolean;` add: `/** Opens the in-UI agent chat. */ onOpenChat: () => void;`
- Destructure `onOpenChat` next to `onOpenAgent` (~line 122).
- Directly after the `Agent` button block (ends ~line 575) add:

```tsx
        <button
          onClick={onOpenChat}
          aria-label="Chat"
          title="Chat with an AI agent about this gateway (your own OpenAI-compatible API key, stored in this browser)"
          className="w-full flex items-center justify-center gap-1.5 transition-colors"
          style={{
            padding: '7px 0',
            borderRadius: 'var(--radius-sm)',
            fontSize: 'var(--text-xs)',
            fontWeight: 'var(--weight-medium)' as never,
            background: 'var(--surface-input)',
            color: 'var(--text-primary)',
            border: '1px solid var(--border)',
          }}
          onMouseEnter={(e) => (e.currentTarget.style.filter = 'brightness(1.08)')}
          onMouseLeave={(e) => (e.currentTarget.style.filter = 'none')}
        >
          <MessageSquare size={12} />
          Chat
        </button>
```

- [ ] **Step 7: Command palette entry**

In `ui/src/commands.ts`, add to `CommandContext` after `openAgentPanel`: `/** Opens the in-UI agent chat. */ openChat: () => void;` and add to the list after `open-agent-panel`:

```ts
    { id: 'open-chat', title: 'Open Chat (AI agent)', run: (c) => c.openChat() },
```

If `ui/src/commands.test.ts` (or a palette test) asserts the exact command list or count, update it to include `open-chat`.

- [ ] **Step 8: AgentPanel link**

In `ui/src/components/AgentPanel.tsx`, add prop `/** Opens the in-UI chat. */ onOpenChat: () => void;` to `AgentPanelProps`, destructure it, and insert as the first child inside the scrolling column (before `{!enabled ? …}`):

```tsx
        <p style={{ fontSize: 'var(--text-2xs)', color: 'var(--text-muted)', margin: 0 }}>
          Prefer chatting here? Open{' '}
          <button onClick={onOpenChat} style={{ background: 'transparent', border: 'none', color: 'var(--accent)', padding: 0, fontSize: 'inherit' }}>
            Chat
          </button>{' '}
          and bring your own OpenAI-compatible API key.
        </p>
```

- [ ] **Step 9: App wiring**

In `ui/src/App.tsx`:
- Imports: `import { ChatPanel } from './components/ChatPanel';` and `import { useChat } from './chat/useChat';` and `import { mcpEndpoint } from './agentPrompts';` (add to the existing agentPrompts import if one exists — `withMcpHint` is already imported from there).
- State, next to `agentOpen` (~line 180): `const [chatOpen, setChatOpen] = useState(false);`
- After `mcpStatus` state: 
  ```ts
  const chat = useChat({
    mcpUrl: mcpEndpoint(window.location.origin, mcpStatus?.path ?? '/mcp'),
    mcpEnabled: mcpStatus?.enabled ?? false,
  });
  ```
- In `commandCtx` add `openChat: () => setChatOpen(true),` (no new deps needed — setter is stable).
- Sidebar: add `onOpenChat={() => setChatOpen(true)}` after `mcpEnabled=…`.
- AgentPanel: add `onOpenChat={() => { setAgentOpen(false); setChatOpen(true); }}`.
- Render after `<AgentPanel …/>`:
  ```tsx
      <ChatPanel open={chatOpen} onClose={() => setChatOpen(false)} chat={chat} mcpStatus={mcpStatus} />
  ```

- [ ] **Step 10: Build, lint, unit tests**

Run: `cd ui && npm run lint && npm test && npm run build`
Expected: clean build. Then `cargo build` (the embedded UI is rebuilt via rust-embed; if the build script requires `ui/dist`, `npm run build` produced it).

- [ ] **Step 11: Manual smoke (optional but recommended)**

Run the gateway with the e2e fixture config (`e2e/fixtures/system.yaml` has MCP on) or your local config with `admin.mcp.enabled: true`, open the UI, footer → Chat → settings → enter a real key and the MCP token → Save. The connection line should read `N tools · read scope` (or write). Ask "list my routes" and watch a `list_routes` card run.

- [ ] **Step 12: Commit**

```bash
git add ui/src/components/chat ui/src/components/ChatPanel.tsx ui/src/components/Sidebar.tsx ui/src/commands.ts ui/src/components/AgentPanel.tsx ui/src/App.tsx
git commit -m "feat(ui): Chat panel with threads, tool-call cards, settings and footer/palette entry points

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>"
```

---

### Task 8: "Ask agent" entry points

**Files:**
- Modify: `ui/src/App.tsx` (`copyPrompt` area ~line 686-736; `promptDialog` state line 188; prompt dialog footer ~line 1275-1300; `commandCtx`; GraphCanvas props ~line 977; DebugPanel props ~line 1318)
- Modify: `ui/src/components/TraceViewer.tsx` (`TraceViewerProps`, the "Why this port?" block ~line 298-308, `TraceHeader` props and buttons ~line 369-425)
- Modify: `ui/src/components/DebugPanel.tsx` (props ~line 31-32; call sites ~line 347-353 and 485-493)
- Modify: `ui/src/components/GraphCanvas.tsx` (props ~line 129-130; toolbar ~line 889-900)
- Modify: `ui/src/commands.ts` (CommandContext + entries)
- Modify: `ui/src/components/AgentPanel.tsx` (prompt library row)

**Interfaces:**
- Produces in App: `askAgent(name: string, args: Record<string, string>): Promise<void>`; `promptDialog` state becomes `{ name; args; mode: 'copy' | 'ask' }`; `CommandContext.agentPrompt(name, mode: 'copy' | 'ask')`.
- `TraceViewer` gains `onAskAgent?: (nodeId: string) => void`; `TraceHeader` gains `onAskAgent?: (prompt: 'explain_trace' | 'why_this_response') => void`.
- `DebugPanel` gains `onAskAgent: (name: string, args: Record<string, string>) => void`.
- `GraphCanvas` gains `onAskAgentReview?: () => void`.
- `AgentPanel` gains `onAskPromptWithArgs: (name: string, args: PromptArgDef[]) => void`.

- [ ] **Step 1: App — `askAgent` and dialog mode**

In `ui/src/App.tsx`:

Change line 188 to:
```ts
  const [promptDialog, setPromptDialog] = useState<null | { name: string; args: PromptArgDef[]; mode: 'copy' | 'ask' }>(null);
```

After `copyPrompt` add:
```ts
  /**
   * Renders a named agent prompt and starts a chat thread with it (the
   * "Ask agent" counterpart of {@link copyPrompt}). No MCP hint line: the
   * chat has the tools itself when a token is set.
   */
  const askAgent = useCallback(
    async (name: string, args: Record<string, string>) => {
      try {
        const r = await api.renderPrompt(name, args);
        setChatOpen(true);
        await chat.seedThread({ prompt: name, args }, r.text);
      } catch (e) {
        const p = parseApiError(e);
        handlePanelError('Could not build the agent prompt', p.error || p.raw);
      }
    },
    [chat, handlePanelError],
  );
```

Replace `agentPrompt` with a two-mode version:
```ts
  const agentPrompt = useCallback(
    (name: 'review_policy' | 'design_policy' | 'design_supernode' | 'design_route', mode: 'copy' | 'ask' = 'copy') => {
      const go = mode === 'ask' ? askAgent : copyPrompt;
      if (name === 'review_policy') {
        if (!selectedPolicy) {
          notify({ tone: 'warning', title: 'Open a policy first' });
          return;
        }
        void go('review_policy', { policy_name: selectedPolicy.name });
        return;
      }
      setPromptValues({});
      setPromptDialog({ name, args: DESIGN_PROMPT_ARGS[name], mode });
    },
    [selectedPolicy, copyPrompt, askAgent, notify],
  );
```

Replace `copyPromptWithArgs` with:
```ts
  const promptWithArgs = useCallback(
    (mode: 'copy' | 'ask') => (name: string, args: PromptArgDef[]) => {
      if (args.every((a) => !a.required)) {
        void (mode === 'ask' ? askAgent : copyPrompt)(name, {});
        return;
      }
      setPromptValues({});
      setPromptDialog({ name, args, mode });
    },
    [copyPrompt, askAgent],
  );
  const copyPromptWithArgs = useMemo(() => promptWithArgs('copy'), [promptWithArgs]);
  const askPromptWithArgs = useMemo(() => promptWithArgs('ask'), [promptWithArgs]);
```

In the prompt dialog footer (the `Copy prompt` DialogButton): replace `void copyPrompt(name, values);` with
```ts
                void (promptDialog!.mode === 'ask' ? askAgent : copyPrompt)(name, values);
```
(read `mode` before `setPromptDialog(null)` — destructure `const { name, args, mode } = promptDialog!;` and use `mode`), and the label with `{promptDialog?.mode === 'ask' ? 'Ask agent' : 'Copy prompt'}`.

In `commandCtx`: `agentPrompt` stays (its signature now has the optional `mode`).

Pass to children:
- `<DebugPanel … onCopyPrompt={copyPrompt} onAskAgent={askAgent} />`
- `<GraphCanvas … onReviewWithAgent={() => agentPrompt('review_policy')} onAskAgentReview={() => agentPrompt('review_policy', 'ask')} />`
- `<AgentPanel … onCopyPromptWithArgs={copyPromptWithArgs} onAskPromptWithArgs={askPromptWithArgs} … />`

- [ ] **Step 2: commands.ts**

Change `agentPrompt` in `CommandContext` to:
```ts
  /** Copies (default) or asks in chat a policy-authoring prompt; design_* open a goal dialog first. */
  agentPrompt: (name: 'review_policy' | 'design_policy' | 'design_supernode' | 'design_route', mode?: 'copy' | 'ask') => void;
```
Add after the four `agent-*` entries:
```ts
    { id: 'agent-ask-review-policy', title: 'Agent: ask to review this policy', when: (c) => c.editorOpen, run: (c) => c.agentPrompt('review_policy', 'ask') },
    { id: 'agent-ask-design-policy', title: 'Agent: ask to design a policy…', run: (c) => c.agentPrompt('design_policy', 'ask') },
    { id: 'agent-ask-design-supernode', title: 'Agent: ask to design a supernode…', run: (c) => c.agentPrompt('design_supernode', 'ask') },
    { id: 'agent-ask-design-route', title: 'Agent: ask to design a route…', run: (c) => c.agentPrompt('design_route', 'ask') },
```

- [ ] **Step 3: TraceViewer / TraceHeader**

In `ui/src/components/TraceViewer.tsx` add `import { MessageSquare } from 'lucide-react';` (keep existing imports).

`TraceViewerProps`: add `/** When set, shows an "Ask agent" icon beside "Why this port?" that opens the chat with the same question. */ onAskAgent?: (nodeId: string) => void;` and destructure it.

Right after the "Why this port?" `</button>` (inside the same `{onCopyPrompt && (…)}` fragment — wrap both buttons in `<>…</>`), add:
```tsx
                {onAskAgent && (
                  <button
                    onClick={() => onAskAgent(step.node_id)}
                    aria-label="Ask agent why this port"
                    title={`Ask the agent in chat why ${step.node_id} exited on port ${step.port ?? 'error'}`}
                    style={{ ...headerButton, marginLeft: 4 }}
                    onMouseEnter={(e) => (e.currentTarget.style.filter = 'brightness(1.08)')}
                    onMouseLeave={(e) => (e.currentTarget.style.filter = 'none')}
                  >
                    <MessageSquare size={11} />
                  </button>
                )}
```
Wrap the eyebrow row's right side in a `<div className="flex items-center">` so both buttons sit together.

`TraceHeader`: add prop `/** When set, adds "Ask agent" icons next to the copy buttons. */ onAskAgent?: (prompt: 'explain_trace' | 'why_this_response') => void;`. After the "Copy as agent prompt" button add:
```tsx
              {onAskAgent && (
                <button
                  onClick={() => onAskAgent('explain_trace')}
                  aria-label="Ask agent to explain this trace"
                  title="Ask the agent in chat to explain this whole trace"
                  style={headerButton}
                  onMouseEnter={(e) => (e.currentTarget.style.filter = 'brightness(1.08)')}
                  onMouseLeave={(e) => (e.currentTarget.style.filter = 'none')}
                >
                  <MessageSquare size={11} />
                </button>
              )}
```
and after the `Why {status}?` button:
```tsx
              {onAskAgent && (
                <button
                  onClick={() => onAskAgent('why_this_response')}
                  aria-label={`Ask agent why ${trace.status}`}
                  title={`Ask the agent in chat why the client got ${trace.status}`}
                  style={headerButton}
                  onMouseEnter={(e) => (e.currentTarget.style.filter = 'brightness(1.08)')}
                  onMouseLeave={(e) => (e.currentTarget.style.filter = 'none')}
                >
                  <MessageSquare size={11} />
                </button>
              )}
```

- [ ] **Step 4: DebugPanel**

Add prop `/** Starts a chat thread seeded with the named prompt (the "Ask agent" counterpart of onCopyPrompt). */ onAskAgent: (name: string, args: Record<string, string>) => void;`, destructure it, and at both call sites pass:
```tsx
                    <TraceHeader
                      trace={detail}
                      onCopyToSandbox={() => copyTraceToSandbox(detail)}
                      onCopyPrompt={(p) => onCopyPrompt(p, { trace_id: detail.id })}
                      onAskAgent={(p) => onAskAgent(p, { trace_id: detail.id })}
                    />
                    <TraceViewer
                      key={detail.id}
                      trace={detail}
                      onCopyPrompt={(nodeId) => onCopyPrompt('why_this_port', { trace_id: detail.id, node_id: nodeId })}
                      onAskAgent={(nodeId) => onAskAgent('why_this_port', { trace_id: detail.id, node_id: nodeId })}
                    />
```
(and the same with `result` in the sandbox tab).

- [ ] **Step 5: GraphCanvas**

Add prop `/** Asks the agent in chat to review this policy; omitted hides the icon. Policy mode only. */ onAskAgentReview?: () => void;`, destructure, add `MessageSquare` to the lucide import, and right after the "Review with agent" button:
```tsx
            {kind === 'policy' && onAskAgentReview && (
              <button
                onClick={onAskAgentReview}
                aria-label="Ask agent to review this policy"
                title="Ask the agent in chat to review this policy"
                style={{ ...toolbarButtonStyle('var(--surface-input)'), color: 'var(--text-primary)', border: '1px solid var(--border)' }}
                onMouseEnter={(e) => (e.currentTarget.style.filter = 'brightness(1.08)')}
                onMouseLeave={(e) => (e.currentTarget.style.filter = 'none')}
              >
                <MessageSquare size={13} />
              </button>
            )}
```

- [ ] **Step 6: AgentPanel prompt library**

Add prop `/** Starts a chat with the named prompt (asks for required arguments first). */ onAskPromptWithArgs: (name: string, args: PromptArgDef[]) => void;`, import `MessageSquare`, and next to each row's Copy button add:
```tsx
                <button
                  aria-label={`Ask agent ${p.name}`}
                  onClick={() => onAskPromptWithArgs(p.name, p.arguments)}
                  className="flex items-center gap-1"
                  style={{ flexShrink: 0, fontSize: 'var(--text-2xs)', color: 'var(--text-primary)', background: 'transparent', border: 'none' }}
                >
                  <MessageSquare size={11} /> Ask
                </button>
```
(wrap the two buttons in a `<span className="flex items-center gap-2">`).

- [ ] **Step 7: Build, lint, tests**

Run: `cd ui && npm run lint && npm test && npm run build`
Expected: clean. Fix any `commands.test.ts`/palette test expecting the old command list.

- [ ] **Step 8: Run the existing e2e MCP and palette scenarios to prove nothing regressed**

Run: `cargo build --release && cd e2e && npx playwright test tests/mcp.spec.ts tests/command-palette.spec.ts tests/debug.spec.ts`
Expected: all pass (the copy buttons and their labels are unchanged).

- [ ] **Step 9: Commit**

```bash
git add ui/src/App.tsx ui/src/commands.ts ui/src/components/TraceViewer.tsx ui/src/components/DebugPanel.tsx ui/src/components/GraphCanvas.tsx ui/src/components/AgentPanel.tsx
git commit -m "feat(ui): \"Ask agent\" actions beside every copy-as-agent-prompt spot

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>"
```

---

### Task 9: Playwright scenarios and testbook

**Files:**
- Create: `e2e/tests/chat.spec.ts`
- Modify: `e2e/E2E_TESTBOOK.md` (new section before "## Deliberately out of scope")

The provider is mocked with `page.route` on a base URL under the admin origin (`${ADMIN_URL}/fake-openai/v1`), so no CORS is involved and interception is deterministic. The MCP calls are real, which also exercises Task 1's same-origin rule in a browser (the fixture has no `allowed_origins`).

- [ ] **Step 1: Write `e2e/tests/chat.spec.ts`**

```ts
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
    await debug.getByRole('button', {name: 'Ask agent why this port'}).click();

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
```

- [ ] **Step 2: Run the new spec**

Run: `cargo build --release && cd e2e && npx playwright test tests/chat.spec.ts`
Expected: 3 passed. If the `Ask agent why this port` button is not found, the Debug row selector or the aria-label from Task 8 Step 3 drifted — fix the label, not the test. If MCP answers `403 origin_not_allowed`, Task 1 is not in the release binary you built.

- [ ] **Step 3: Testbook**

In `e2e/E2E_TESTBOOK.md`, before `## Deliberately out of scope`, add:

```markdown
## Chat (`tests/chat.spec.ts`)

The in-UI agent chat. The OpenAI-compatible provider is a `page.route` fake under the admin origin (`/fake-openai/v1`), scripted by the last message; the MCP tool calls are real (and prove the same-origin `Origin` rule, since the fixture lists no `allowed_origins`). Settings are pre-seeded into `localStorage` by an init script.

| ID | Steps | Expected |
|----|-------|----------|
| E2E-CHAT-01 | **Browser.** Sandbox-run the first fixture policy, Debug → first trace → **Ask agent why this port**; then reload, footer → **Chat**, reopen the thread | The Chat dialog opens on a thread titled `why_this_port · …`; the first user bubble inlines the policy name; a `list_policies` tool card ends `done` without confirmation; the assistant reply is shown; the request the provider saw carried a `system` message and the gateway's tool schemas; after reload the thread and reply are still there |
| E2E-CHAT-02 | **Browser.** Chat → **New chat** → send "please write a policy for me" → **Skip** on the `put_policy` card | The card shows `awaiting confirmation` with **Run**/**Skip**; after Skip it shows `declined`, the model's follow-up text renders, the provider received `{"role":"tool","content":"Declined by the user."}`, and `GET /api/policies/e2e-chat-tmp` is `404` |
| E2E-CHAT-03 | **Browser.** Chat → New chat → send a message containing `Authorization: Bearer supersecrettoken123` and the write MCP token → **Clear all chats** | The user bubble, the request the provider received, and `featherbit.chat.threads` all contain `[REDACTED]` and neither secret; after clearing, the thread list shows "No chats yet.", `featherbit.chat.threads` has zero threads, and `featherbit.chat.settings` still holds the API key |
```

- [ ] **Step 4: Run the whole e2e suite once**

Run: `cd e2e && npm test`
Expected: everything green (the `E2E-SESS-*` scenarios skip without `FEATHERBIT_TEST_REDIS_URL`; ACME live tests skip without Pebble).

- [ ] **Step 5: Commit**

```bash
git add e2e/tests/chat.spec.ts e2e/E2E_TESTBOOK.md
git commit -m "test(e2e): chat panel scenarios with a scripted provider and real MCP tools

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>"
```

---

### Task 10: Documentation and knowledge graph

**Files:**
- Modify: `website/docs/guides/mcp.md` (new section after "## Connecting a client"; Security notes bullet)
- Modify: `website/docs/reference/roadmap.md:25`
- Modify: `CLAUDE.md` (MCP bullet in "What This Project Is"; the UI paragraph under "Not Yet Implemented")
- Modify: `docs/superpowers/specs/2026-09-11-agent-chat-design.md` (two wording fixes, see Step 4)

- [ ] **Step 1: MCP guide**

Insert after the "## Connecting a client" section (before "## Tools, resources, prompts"):

```markdown
## Chat in the UI

The web UI has a **Chat** panel (footer button, or `Ctrl+K` → "Open Chat") that talks to an OpenAI-compatible Chat Completions endpoint **from your browser** and uses this gateway's MCP tools during the conversation. The gateway itself still runs no model and holds no provider key.

Settings (gear icon in the panel) are stored in this browser's local storage under `featherbit.chat.settings`:

| Field | Meaning |
|---|---|
| Base URL | `https://api.openai.com/v1` by default; any compatible server works (Azure, OpenRouter, a local Ollama, …) |
| Model | Free text; **Load models** fetches the provider's `GET /models` list into a suggestion dropdown, so nothing is hardcoded |
| API key | Sent only to the base URL above |
| MCP token | One of `admin.mcp.tokens`; sent only to this gateway's MCP endpoint. Leave empty for a toolless chat |

Threads live under `featherbit.chat.threads` (50 newest kept, tool results truncated at 32 000 characters). Each thread has **Delete**; **Clear all chats** removes them all; **Forget credentials** clears the key and token but keeps base URL and model. Nothing in the chat is sent to the gateway's Admin API.

**Secrets.** The gateway already keeps most secrets out of what the chat can see: traces redact sensitive headers, query parameters and message keys when they are captured, MCP tools mask consumer credentials, and config is served with raw `${ENV}` placeholders. The chat adds a client-side pass on top (**Redact secrets before sending**, on by default): before any text is stored or sent to the provider — seeded prompts, what you type, tool results — it replaces `Bearer`/`Basic` credentials, `Cookie` values, values of secret-looking keys (`password`, `secret`, `api_key`, `*_token`, `private_key`, …), JWTs, PEM private keys, well-known key prefixes, and your own API key and MCP token with `[REDACTED]`. It is a heuristic, not a guarantee: keep genuinely sensitive request bodies out of traces you hand to a third-party model, and turn the toggle off only when you need the model to see a real value (the connection line says when it is off).

**Tools.** With a token set, the panel loads `tools/list` and hands the schemas to the model. Read tools run as soon as the model asks. Write tools (`put_*`, `delete_*`, `reload_config`) render a card with **Run** and **Skip**; Skip returns "Declined by the user." to the model so it can propose something else. A turn stops after 16 tool rounds; **Stop** aborts the current request.

**Ask agent.** Beside every "Copy as agent prompt" action — the trace header, "Why this port?" on a trace step, "Review with agent" in the policy toolbar, the `Agent: ask …` palette entries and the Agent panel's prompt library — an "Ask agent" button starts a thread seeded with that prompt, data inlined, so a trace question becomes a conversation you keep asking into.

**Origins.** Browsers send `Origin` on every POST, so the MCP endpoint accepts a request whose `Origin` authority equals its `Host` (the UI calling the gateway it was served from) even with an empty `allowed_origins`. The Vite dev server on another port still needs listing. Self-hosted providers must allow the admin origin in their own CORS configuration (for Ollama: `OLLAMA_ORIGINS`).
```

Update the Security notes bullet about `Origin` to:

```markdown
- A request carrying an `Origin` header is refused unless it is same-origin (its authority equals the request's `Host`) or listed in `allowed_origins` (DNS-rebinding defence — and either way the bearer token is still required). Non-browser agents send none.
```

- [ ] **Step 2: Roadmap**

In `website/docs/reference/roadmap.md` line 25, replace `Follow-ups: stdio transport, consumer writes, `listChanged` notifications, an optional bring-your-own-LLM chat panel.` with:

```
The UI's **Chat** panel talks to a bring-your-own OpenAI-compatible endpoint from the browser and uses these MCP tools mid-conversation (reads auto-run, writes ask first; threads and settings in browser local storage). Follow-ups: stdio transport, consumer writes, `listChanged` notifications, an Anthropic provider for the chat, a session-only key mode.
```

- [ ] **Step 3: CLAUDE.md**

In the `MCP server for agents` bullet, append one sentence: `The web UI's **Chat** panel (`ui/src/chat/` — store, OpenAI streaming client, browser MCP client, turn loop, `useChat`; `ui/src/components/ChatPanel.tsx`) is a bring-your-own-key OpenAI-compatible chat that calls these MCP tools from the browser, reads auto-run and writes gated by Run/Skip, threads/settings in localStorage (`featherbit.chat.*`); the MCP `Origin` check accepts same-origin requests so the embedded UI works with an empty `allowed_origins`.`

In the UI paragraph (the one listing the Stores editor and Sessions panel), append: `, plus a **Chat** panel (footer button / `Ctrl+K`) and "Ask agent" icons beside every "Copy as agent prompt" action that seed a chat thread with that prompt.`

- [ ] **Step 4: Spec wording fixes**

In `docs/superpowers/specs/2026-09-11-agent-chat-design.md`: in the Limits table change `| Stored tool result | 32 KB, truncated with a `…[truncated N bytes]` marker |` to `| Stored tool result | 32 000 characters, truncated with a `…[truncated N chars]` marker |`; in §2's `ChatMessage` type add `error?: string` to the assistant variant with a trailing comment `// provider failure line; skipped on replay`.

- [ ] **Step 5: Docs build and graph update**

Run: `cd website && npm run build` — Expected: builds. Then `graphify update .` from the repo root — Expected: graph refreshed without errors.

- [ ] **Step 6: Commit**

```bash
git add website/docs/guides/mcp.md website/docs/reference/roadmap.md CLAUDE.md docs/superpowers/specs/2026-09-11-agent-chat-design.md graphify-out
git commit -m "docs(mcp): Chat in the UI guide, roadmap and CLAUDE.md entries

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>"
```

---

### Task 11: Final verification

- [ ] **Step 1: Full Rust suite**

Run: `cargo test`
Expected: all pass.

- [ ] **Step 2: Full UI checks**

Run: `cd ui && npm run lint && npm test && npm run build`
Expected: clean.

- [ ] **Step 3: Full e2e**

Run: `cargo build --release && cd e2e && npm test`
Expected: green, including `E2E-MCP-01..03` and `E2E-CHAT-01..03`.

- [ ] **Step 4: Report**

Summarize to Francesco: branch `feature/agent-chat` (based on `feature/mcp-server`), the commits, what was verified, and that the PR target should be `feature/mcp-server` (or `develop` once that branch merges). Do not open the PR without their go-ahead.
