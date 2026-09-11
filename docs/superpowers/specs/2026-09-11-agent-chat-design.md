# Agent chat in the web UI — design

**Date:** 2026-09-11
**Status:** approved design, awaiting implementation plan
**Builds on:** `2026-08-30-mcp-server-design.md` (MCP server, Agent panel, "Copy as
agent prompt")

## Summary

The MCP work shipped the gateway as an MCP *server* plus "Copy as agent prompt"
actions that hand text to an agent the user runs elsewhere. This follow-up adds
the "bring-your-own-LLM chat panel" the MCP spec and the roadmap listed: a chat
drawer in the embedded UI where the browser talks to an OpenAI-compatible Chat
Completions endpoint directly, uses the gateway's own MCP tools during the
conversation, and keeps threads and settings in the browser's local storage.
The gateway gains no LLM client, stores no provider key, and pays for no tokens.

Decisions taken during brainstorming, in order:

1. **The browser calls the provider directly** with a user-pasted API key kept in
   local storage. The gateway stays LLM-free.
2. **Tools come from the MCP endpoint** on the same admin origin, authenticated
   with an MCP bearer token the user pastes into the chat settings. The tool set
   is defined once, server-side; the token's `read`/`write` scope decides what
   the model can do. Without a token the chat still works, toolless.
3. **Reads run automatically, writes ask first.** A write tool call renders a
   card with Run / Skip; Skip feeds a "declined by the user" result back.
4. **One chat drawer with threads.** Every "Copy as agent prompt" spot gains an
   "Ask agent" action that seeds a new thread. Threads are per-browser and
   flushable (per-thread Delete, "Clear all chats").
5. **Hand-rolled clients, no new npm dependencies.** The MCP wire shape is the
   one the e2e suite already uses; the OpenAI streaming client is one function.
6. **One server change:** the MCP `Origin` check accepts same-origin requests so
   the embedded UI works with the default empty `allowed_origins`.

## Non-goals

- Any LLM client, provider key, or chat proxy inside the gateway (approach 2,
  rejected).
- Anthropic Messages API support. The provider client speaks OpenAI Chat
  Completions only; Azure, OpenRouter, Ollama and other compatible servers work
  through the configurable base URL. A second provider is a possible follow-up.
- Server-side or cross-browser persistence of threads.
- Sharing/exporting threads.
- A "remember key for this session only" mode (possible follow-up).

## 1. Architecture and data flow

Everything runs in the browser. New module `ui/src/chat/`, split into units that
are testable without React:

| File | Responsibility |
|---|---|
| `openai.ts` | `streamChat(settings, request, signal)` POSTs `{baseUrl}/chat/completions` with `stream: true`, parses the SSE chunks, yields text deltas and assembled tool calls (tool-call deltas arrive split across chunks and are reassembled by index). Nothing else knows the wire format. |
| `mcpClient.ts` | Fetch-based client for this gateway's `/mcp`: `initialize` once (session id kept in memory), `notifications/initialized`, `tools/list`, `tools/call`. Parses both plain-JSON and single-event SSE replies. Re-initializes once on a `404` (stale session). |
| `loop.ts` | `runTurn(thread, provider, mcp, hooks)`: builds system prompt + history + tool schemas, streams the reply, executes tool calls (read: immediately; write: via `hooks.confirm`), appends results, repeats until a text-only reply, abort, or the round cap. |
| `store.ts` | Threads and settings in local storage (§2). Pure functions for append/trim/title plus guarded load/save. |
| `systemPrompt.ts` | The fixed system prompt text, with a toolless variant. |

MCP tool `inputSchema` is JSON Schema, so each tool maps to an OpenAI
`{type: "function", function: {name, description, parameters}}` entry with no
translation.

Data flow for one user turn:

```
composer ──► store.append(user) ──► loop.runTurn
                                       │  streamChat ──► assistant deltas ──► UI
                                       │  tool_calls?
                                       │    read  ──► mcp.callTool ──► tool result
                                       │    write ──► hooks.confirm ──► Run: callTool / Skip: declined
                                       │  ◄── repeat (≤16 rounds)
                                       └─► store.save(thread)
```

### Server change: same-origin acceptance in `src/mcp/auth.rs`

Today any request carrying an `Origin` header not listed in
`admin.mcp.allowed_origins` is refused, and browsers always send `Origin` on
POST — even same-origin. `authenticate()` gains a second acceptance rule: the
`Origin` value's authority (host[:port]) equals the request's `Host` header
authority. The explicit allow-list keeps working (needed for the Vite dev
server on `:5173`).

Rationale: the check exists to stop *cross-site* pages (evil.com → 127.0.0.1)
from reaching the endpoint, and same-origin equality still blocks those. Under a
DNS-rebinding attack `Origin` and `Host` both name the attacker's domain, so the
rule would let the request *reach* the endpoint — but the request still needs
the bearer token, which the attacker's origin cannot read from this origin's
local storage. The token, not `Origin`, is the authentication boundary.
Documented in the `McpConfig::allowed_origins` doc comment and the MCP guide.

## 2. Storage model and settings

Two local storage keys. Every access is guarded (try/catch) as in
`ui/src/notifications.ts`: a private window or blocked storage degrades to
in-memory state with a one-time warning toast.

### `featherbit.chat.settings`

```ts
interface ChatSettings {
  baseUrl: string;   // default 'https://api.openai.com/v1'
  model: string;     // free text; default is a single constant (DEFAULT_MODEL)
  apiKey: string;    // '' → drawer opens on the settings form
  mcpToken: string;  // '' → toolless mode, stated in the thread header
  redact: boolean;   // default true — client-side secret redaction (§3)
}
```

### `featherbit.chat.threads`

Versioned envelope so a later shape change migrates instead of discarding:

```ts
interface ThreadStore { version: 1; threads: Thread[] }

interface Thread {
  id: string;
  title: string;
  createdAt: number;
  updatedAt: number;
  seed?: { prompt: string; args: Record<string, string> };
  messages: ChatMessage[];
}

type ChatMessage =
  | { role: 'user'; content: string }
  | { role: 'assistant'; content: string; toolCalls?: ToolCall[]; error?: string /* provider failure line; skipped on replay */ }
  | { role: 'tool'; toolCallId: string; name: string; status: 'done' | 'declined' | 'error'; content: string };

interface ToolCall { id: string; name: string; arguments: string /* JSON */ }
```

Messages stay close to the OpenAI wire shape so history replays without
translation (`tool` → `{role:'tool', tool_call_id, content}`).

### Limits

| Limit | Value |
|---|---|
| Threads kept | 50, oldest by `updatedAt` dropped |
| Stored tool result | 32 000 characters, truncated with a `…[truncated N chars]` marker |
| On `QuotaExceededError` | drop the oldest thread and retry; if still failing, warn once and keep in memory |

The truncated result is also what the model receives, so the stored thread and
the conversation the model actually had never diverge.

### Flushing

- Per-thread **Delete**.
- **Clear all chats** removes `featherbit.chat.threads` only.
- **Forget credentials** (settings form) clears `apiKey` and `mcpToken`, keeps
  `baseUrl` and `model`.

### Where credentials go

The API key is sent only to the configured base URL; the MCP token only to this
gateway's `/mcp`. Neither is written to the notifications log, toasts, or any
gateway endpoint. They are readable by scripts on the admin origin only (same
origin policy) and by anyone with access to the browser profile — the same
posture as `gw_credentials`, which the UI already keeps in local storage.

## 3. The tool loop, confirmation and errors

**System prompt.** Fixed text: the assistant helps operate this Featherbit
gateway (routes, node-graph policies, supernodes, plugin configs, stores,
traces, sandbox); prefer tools over guessing; when proposing config, validate
first (`validate_policy`) and describe the change before writing. The toolless
variant says the only data available is what is inlined in the conversation.

**Tool list.** Fetched with `tools/list` when the drawer connects (and after a
session re-init); cached in memory. Empty when no MCP token is set or MCP is
disabled/uncompiled; the connection line explains which.

**Streaming.** Text deltas append to the in-progress assistant bubble. A finish
with tool calls renders one tool-call card per call (name + pretty-printed
arguments).

**Read tools** (not in `WRITE_TOOLS`) run immediately: spinner, then the result
collapsed behind an expand toggle. A result the server flags `isError: true`
is stored with `status: 'error'` and returned to the model as-is so it can
adjust.

**Write tools** (`WRITE_TOOLS` in `ui/src/agentPrompts.ts`, kept in sync with
the server by E2E-MCP-02) wait: the card shows **Run** and **Skip**. Run calls
the tool and continues. Skip stores `status: 'declined'` with content
`Declined by the user.` and continues, so the model can propose an alternative.
A pending confirmation blocks its own thread only.

**Ending a turn.** Text-only reply → done. Round cap: 16 tool rounds per user
turn, then an assistant-side notice ("stopped after 16 tool rounds — send a
message to continue"). **Stop** aborts the in-flight provider or tool request
via `AbortController`; partial assistant text is kept. Provider HTTP errors
(401 bad key, 429, 5xx, network/CORS) render as an error line in the thread and
end the turn with history intact, so the user can fix settings and resend. A
stale MCP session (`404`) is re-initialized once transparently; a second
failure surfaces as a tool error.

**Secret redaction.** The gateway already keeps most secrets out of what the
chat can see: traces redact sensitive headers, query parameters and message
keys at capture time, MCP tools mask consumer credentials, and config is
served with raw `${ENV}` placeholders. The chat adds a client-side second
line of defence (`ui/src/chat/redact.ts`, on by default, `settings.redact`):
before any text is stored or sent to the provider — seeded prompts, typed
messages, tool results — it replaces with `[REDACTED]`: `Bearer`/`Basic`
credentials, `Cookie`/`Set-Cookie` values, values of secret-looking keys
(`password`, `secret`, `client_secret`, `api_key`, `*_token`, `private_key`,
`access_key`, `secret_key`, `x-api-key`, …) in JSON/YAML/header form, JWTs,
PEM private-key blocks, well-known key prefixes (`sk-…`, `ghp_…`, `AKIA…`,
`xox…`), and the literal values of the user's own API key and MCP token.
`${ENV}` placeholders and already-masked markers are left alone. Redaction
can hide a value the model needs (e.g. debugging an auth header), so the
settings form has a toggle; the connection line says when it is off.

**Titles.** Seeded threads: `<prompt> · <first arg value>` (e.g.
`why_this_port · 3f9a…` or `review_policy · api-policy`). Others: the first
user message, trimmed to 60 chars.

## 4. UI surfaces and entry points

**Chat drawer** (`ui/src/components/ChatDrawer.tsx` plus small children:
`ThreadList`, `MessageList`, `ToolCallCard`, `ChatSettingsForm`). Right-side
panel opened from a footer **Chat** button and a Ctrl+K entry.

- Left column: thread list (title, relative time), **New**, per-thread Delete,
  **Clear all chats**.
- Right column: connection line (model · N tools · scope, or "Enter settings to
  start" / "No MCP token — toolless"), messages, tool-call cards, composer with
  **Send** and **Stop**.
- Gear icon → settings form (base URL, model, API key, MCP token, **Forget
  credentials**, **Test connection** which runs `initialize` + `tools/list` and
  reports tool count / scope or the error).

The existing **Agent** dialog stays as the "connect an external agent" surface
and gains one line linking to the chat.

**Ask agent.** Every "Copy as agent prompt" location gains an **Ask agent**
action beside Copy: `TraceViewer` "Why this port?", `DebugPanel` trace prompts,
the policy toolbar "Review policy", the command palette entries, and the Agent
dialog prompt library (reusing the existing argument dialog for prompts with
required arguments). `App` gets `askAgent(name, args)`: render via
`GET /api/mcp/prompts/{name}` (no MCP hint line — the hint is for external
agents), create a seeded thread, open the drawer, send the first message. This
is how the Debug panel becomes chat-like: each trace question is a thread the
user keeps asking into.

## 5. Testing

**Vitest** (`ui/src/chat/*.test.ts`):
- `store`: append/trim to 50, tool-result truncation, quota fallback (mocked
  `setItem` throwing), title derivation, envelope version check.
- `openai`: SSE parsing incl. tool-call deltas split across chunks, `[DONE]`,
  non-2xx error surfacing, abort.
- `mcpClient`: JSON and SSE reply parsing, session header propagation,
  one-shot re-init on 404, `isError` passthrough.
- `loop`: fake provider + fake MCP — text-only turn; read tool auto-run; write
  tool waits for confirm, Run path, Skip path; round cap; abort mid-stream.

**Rust** (`src/mcp/auth.rs`): same-origin (`Origin: http://127.0.0.1:19091`
with `Host: 127.0.0.1:19091`) accepted; mismatched origin refused; explicit
allow-list still honored; no `Origin` unchanged.

**Playwright** (`e2e/tests/chat.spec.ts`, provider mocked with `page.route` on
a fake base URL):
- E2E-CHAT-01: settings saved to local storage; from a sandbox trace click
  **Ask agent** on "Why this port?"; the drawer opens with a seeded thread; the
  fake model requests `get_trace_step`, the real MCP tool answers, the fake
  model replies with text; the reply is shown and the thread survives reload.
- E2E-CHAT-02: fake model requests `put_policy`; the card shows Run / Skip;
  Skip stores a declined result and the fake model's follow-up text renders.
- E2E-CHAT-03: **Clear all chats** empties `featherbit.chat.threads`; settings
  remain.

`E2E_TESTBOOK.md` gains a "Chat" section; E2E-MCP-02/03 unchanged.

## 6. Documentation

- `website/docs/guides/mcp.md`: new "Chat in the UI" section — settings,
  where credentials live (browser only), toolless mode, write confirmation,
  the same-origin rule, and the CORS note for self-hosted OpenAI-compatible
  servers (e.g. Ollama needs `OLLAMA_ORIGINS`).
- `website/docs/reference/roadmap.md`: the MCP row's "optional
  bring-your-own-LLM chat panel" follow-up becomes implemented; new follow-ups:
  Anthropic provider, session-only key.
- `CLAUDE.md`: one sentence in the MCP bullet and the UI paragraph.
