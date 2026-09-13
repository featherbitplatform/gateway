# MCP Agent UI Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make the web UI MCP-aware without ever holding a token: an **Agent** footer panel (status, endpoint, client snippets, scope explainer, prompt library) and contextual **"Copy as agent prompt"** actions in the trace viewer and the policy editor, all fed by the Basic-Auth endpoints `GET /api/mcp/status` and `GET /api/mcp/prompts[/{name}]`.

**Architecture:** Pure-logic helpers in `ui/src/agentPrompts.ts` (endpoint URL, client snippets, scope tool lists, MCP hint line — unit-tested with vitest) + one new `Dialog`-based panel wired through `App.tsx`/`Sidebar.tsx` like Sessions/Certificates + small optional callback props on `TraceViewer`/`TraceHeader` + App-level palette commands with a goal dialog. Prompt text always comes from the server so UI and MCP prompts stay byte-identical.

**Tech Stack:** React 19 + TypeScript (Vite), lucide-react icons, vitest (pure logic only — no DOM testing library), Playwright e2e (`e2e/`).

**Spec:** `docs/superpowers/specs/2026-08-30-mcp-server-design.md` (section "Web UI")

**Prerequisite:** backend plan `docs/superpowers/plans/2026-08-30-mcp-server.md` through Task 10 (the `/api/mcp/*` endpoints) on branch `feature/mcp-server`.

## Global Constraints

- Same branch (`feature/mcp-server`) unless Francesco asks otherwise. Conventional Commits (`feat(ui): …`, `test(e2e): …`, `docs: …`), no `Co-Authored-By`. Commit per task; no push/PR without go-ahead.
- Before every commit: `cd ui && npm run lint && npm test && npm run build`. The Rust binary embeds `ui/dist`, so after UI changes run `cargo build --release` before e2e (`cd e2e && npm run test:all` does both).
- No token ever reaches the browser: the UI renders `<TOKEN>` placeholders and never reads `admin.mcp.tokens`.
- Toasts/notifications are threaded down from `App.tsx` as **memoized** callbacks (`useCallback`) — never inline arrows (see the loop warning at `App.tsx:614-619`); there is no global notifier.
- Footer panels are `<Dialog>`s opened from `Sidebar.tsx` with state in `App.tsx`; e2e locates them with `getByRole('button', {name})` → `getByRole('dialog', {name: '<title>'})`.
- Step rows in `TraceViewer` are `<button>`s — never nest a button inside; put per-step actions in the detail pane.
- Scenario IDs `E2E-MCP-01..03` go in `e2e/E2E_TESTBOOK.md` with the existing table format.

## File map

| Path | Responsibility |
|---|---|
| `ui/src/types/index.ts` | `McpStatus`, `PromptDef`, `PromptArgDef`, `RenderedPrompt` |
| `ui/src/api/client.ts` | `api.mcpStatus()`, `api.listPrompts()`, `api.renderPrompt(name, args)` |
| `ui/src/agentPrompts.ts` (+ `.test.ts`) | `mcpEndpoint`, `clientSnippets`, `READ_TOOLS`, `WRITE_TOOLS`, `withMcpHint`, `promptQuery` |
| `ui/src/components/AgentPanel.tsx` | the footer dialog |
| `ui/src/components/Sidebar.tsx` | "Agent" footer button |
| `ui/src/App.tsx` | `agentOpen`/`mcpStatus` state, `copyPrompt` helper, palette commands + goal dialog |
| `ui/src/components/TraceViewer.tsx` | `onCopyPrompt` props on `TraceHeader` and `TraceViewer` |
| `ui/src/components/DebugPanel.tsx` | passes `onCopyPrompt` through |
| `ui/src/commands.ts` | `agent-review-policy`, `agent-design-policy`, `agent-design-supernode`, `agent-design-route`, `open-agent-panel` |
| `ui/src/components/GraphCanvas.tsx` | toolbar "Review with agent" button (policy mode) |
| `e2e/fixtures/system.yaml`, `e2e/tests/mcp.spec.ts`, `e2e/E2E_TESTBOOK.md` | scenarios |
| `website/docs/guides/web-ui.md`, `website/docs/guides/mcp.md` | docs |

---

### Task 1: Types, API client, and pure helpers

**Files:**
- Modify: `ui/src/types/index.ts`, `ui/src/api/client.ts`
- Create: `ui/src/agentPrompts.ts`, `ui/src/agentPrompts.test.ts`

**Interfaces:**
- Produces:
  ```ts
  export interface McpStatus { compiled: boolean; enabled: boolean; path: string; token_count: number; scopes: Array<'read' | 'write'> }
  export interface PromptArgDef { name: string; description: string; required: boolean }
  export interface PromptDef { name: string; description: string; arguments: PromptArgDef[] }
  export interface RenderedPrompt { name: string; description: string; text: string }
  api.mcpStatus(): Promise<McpStatus>
  api.listPrompts(): Promise<PromptDef[]>
  api.renderPrompt(name: string, args: Record<string, string>): Promise<RenderedPrompt>
  mcpEndpoint(origin: string, path: string): string
  clientSnippets(url: string): { claudeCode: string; mcpJson: string; curl: string }
  READ_TOOLS: readonly string[]; WRITE_TOOLS: readonly string[]
  withMcpHint(text: string): string
  promptQuery(args: Record<string, string | undefined>): string
  ```

- [ ] **Step 1: Failing tests**

Create `ui/src/agentPrompts.test.ts`:

```ts
import { describe, expect, it } from 'vitest';
import { clientSnippets, mcpEndpoint, promptQuery, READ_TOOLS, withMcpHint, WRITE_TOOLS } from './agentPrompts';

describe('mcpEndpoint', () => {
  it('joins origin and path without doubling slashes', () => {
    expect(mcpEndpoint('http://localhost:9090', '/mcp')).toBe('http://localhost:9090/mcp');
    expect(mcpEndpoint('http://localhost:9090/', '/agent')).toBe('http://localhost:9090/agent');
  });
});

describe('clientSnippets', () => {
  const s = clientSnippets('http://gw:9090/mcp');
  it('claude code snippet uses http transport and a token placeholder', () => {
    expect(s.claudeCode).toContain('claude mcp add --transport http featherbit http://gw:9090/mcp');
    expect(s.claudeCode).toContain('Authorization: Bearer <TOKEN>');
  });
  it('mcpServers json is valid JSON with the url', () => {
    const parsed = JSON.parse(s.mcpJson);
    expect(parsed.mcpServers.featherbit.url).toBe('http://gw:9090/mcp');
    expect(parsed.mcpServers.featherbit.headers.Authorization).toBe('Bearer <TOKEN>');
  });
  it('curl snippet sends an initialize request', () => {
    expect(s.curl).toContain('"method":"initialize"');
    expect(s.curl).toContain('text/event-stream');
  });
});

describe('scope tool lists', () => {
  it('are disjoint and non-empty', () => {
    expect(READ_TOOLS.length).toBeGreaterThan(10);
    expect(WRITE_TOOLS.length).toBeGreaterThan(5);
    for (const t of WRITE_TOOLS) expect(READ_TOOLS).not.toContain(t);
    for (const t of WRITE_TOOLS) expect(/^(put_|delete_|reload_config$)/.test(t)).toBe(true);
  });
});

describe('withMcpHint', () => {
  it('appends the hint once', () => {
    const out = withMcpHint('body');
    expect(out.startsWith('body\n\n')).toBe(true);
    expect(out).toContain('featherbit');
    expect(out).toContain('get_trace_step');
  });
});

describe('promptQuery', () => {
  it('encodes present args only', () => {
    expect(promptQuery({ trace_id: 'a b', node_id: undefined })).toBe('trace_id=a+b');
    expect(promptQuery({})).toBe('');
  });
});
```

- [ ] **Step 2: Run to fail**

Run: `cd ui && npx vitest run src/agentPrompts.test.ts`
Expected: fails — module missing.

- [ ] **Step 3: Implement**

`ui/src/types/index.ts` — append:

```ts
/** `GET /api/mcp/status` — never includes token values. */
export interface McpStatus {
  compiled: boolean;
  enabled: boolean;
  path: string;
  token_count: number;
  scopes: Array<'read' | 'write'>;
}

/** One argument of a precompiled agent prompt. */
export interface PromptArgDef {
  name: string;
  description: string;
  required: boolean;
}

/** A precompiled agent prompt, from `GET /api/mcp/prompts`. */
export interface PromptDef {
  name: string;
  description: string;
  arguments: PromptArgDef[];
}

/** A rendered prompt, from `GET /api/mcp/prompts/{name}`. */
export interface RenderedPrompt {
  name: string;
  description: string;
  text: string;
}
```

`ui/src/api/client.ts` — import the types and add to the `api` object:

```ts
  /** `GET /api/mcp/status` — MCP availability; answers even when MCP is off. */
  mcpStatus: () => request<McpStatus>('/api/mcp/status'),
  /** `GET /api/mcp/prompts` — the precompiled agent prompts. */
  listPrompts: () => request<{ prompts: PromptDef[] }>('/api/mcp/prompts').then((r) => r.prompts),
  /** `GET /api/mcp/prompts/{name}?…` — a prompt rendered with live data. */
  renderPrompt: (name: string, args: Record<string, string>) => {
    const q = new URLSearchParams(args).toString();
    return request<RenderedPrompt>(`/api/mcp/prompts/${name}${q ? '?' + q : ''}`);
  },
```

Create `ui/src/agentPrompts.ts`:

```ts
/**
 * Pure helpers behind the Agent panel and the "Copy as agent prompt"
 * actions. No fetching here — see api/client.ts.
 */

/** The MCP endpoint URL for this browser's admin origin. */
export function mcpEndpoint(origin: string, path: string): string {
  return origin.replace(/\/+$/, '') + (path.startsWith('/') ? path : `/${path}`);
}

/** Ready-to-paste client configs; `<TOKEN>` is for the user to fill in. */
export function clientSnippets(url: string): { claudeCode: string; mcpJson: string; curl: string } {
  const claudeCode = `claude mcp add --transport http featherbit ${url} \\\n  --header "Authorization: Bearer <TOKEN>"`;
  const mcpJson = JSON.stringify(
    { mcpServers: { featherbit: { type: 'http', url, headers: { Authorization: 'Bearer <TOKEN>' } } } },
    null,
    2,
  );
  const init = JSON.stringify({
    jsonrpc: '2.0',
    id: 1,
    method: 'initialize',
    params: { protocolVersion: '2025-03-26', capabilities: {}, clientInfo: { name: 'curl', version: '0' } },
  });
  const curl = `curl -s -X POST ${url} \\\n  -H "Authorization: Bearer <TOKEN>" \\\n  -H "Accept: application/json, text/event-stream" \\\n  -H "Content-Type: application/json" \\\n  -d '${init}'`;
  return { claudeCode, mcpJson, curl };
}

/** Tools a `read` token can call (kept in sync with the server by E2E-MCP-02). */
export const READ_TOOLS: readonly string[] = [
  'list_node_types', 'get_node_type', 'list_vars', 'get_status', 'export_config',
  'list_routes', 'get_route', 'list_policies', 'get_policy', 'list_supernodes', 'get_supernode',
  'list_plugin_configs', 'get_plugin_config', 'list_stores', 'list_consumers', 'get_consumer',
  'validate_policy', 'validate_supernode', 'list_traces', 'get_trace', 'get_trace_step', 'run_sandbox',
];

/** Tools that additionally need a `write` token. */
export const WRITE_TOOLS: readonly string[] = [
  'put_route', 'delete_route', 'put_policy', 'delete_policy', 'put_supernode', 'delete_supernode',
  'put_plugin_config', 'delete_plugin_config', 'put_store', 'delete_store', 'reload_config',
];

const MCP_HINT =
  'If the `featherbit` MCP server is connected, prefer its tools (get_trace_step, get_node_type, validate_policy, run_sandbox) over the data inlined above.';

/** Appends the one-line MCP hint to a rendered prompt. */
export function withMcpHint(text: string): string {
  return `${text.replace(/\s+$/, '')}\n\n${MCP_HINT}\n`;
}

/** Query string for `GET /api/mcp/prompts/{name}` from possibly-undefined args. */
export function promptQuery(args: Record<string, string | undefined>): string {
  const q = new URLSearchParams();
  for (const [k, v] of Object.entries(args)) if (v !== undefined && v !== '') q.set(k, v);
  return q.toString();
}
```

- [ ] **Step 4: Run, lint, commit**

Run: `cd ui && npm test && npm run lint`
Expected: pass.

```bash
git add ui/src/types/index.ts ui/src/api/client.ts ui/src/agentPrompts.ts ui/src/agentPrompts.test.ts
git commit -m "feat(ui): MCP status/prompt API bindings and agent snippet helpers"
```

---

### Task 2: Agent panel + footer button + App wiring

**Files:**
- Create: `ui/src/components/AgentPanel.tsx`
- Modify: `ui/src/components/Sidebar.tsx` (props interface `:14-75`, destructure `:89-118`, footer JSX after the Certificates button `:550-569`), `ui/src/App.tsx`

**Interfaces:**
- `AgentPanel` props: `{ open: boolean; onClose: () => void; status: McpStatus | null; onCopy: (label: string, text: string) => void; onError: (title: string, message: string) => void }`. `onCopy` is App's clipboard+toast helper (Task 3 reuses it).
- `Sidebar` gains `onOpenAgent: () => void; mcpEnabled: boolean`.
- App: `agentOpen`, `mcpStatus: McpStatus | null` (fetched next to `debugConfig` in `loadData`, advisory try/catch), `copyText(label, text)` memoized helper.

- [ ] **Step 1: The panel**

Create `ui/src/components/AgentPanel.tsx`:

```tsx
import { useEffect, useState } from 'react';
import { Copy } from 'lucide-react';
import { Dialog, DialogButton } from './Dialog';
import { api } from '../api/client';
import { parseApiError } from '../apiError';
import { clientSnippets, mcpEndpoint, READ_TOOLS, WRITE_TOOLS } from '../agentPrompts';
import type { McpStatus, PromptDef } from '../types';

/** Props for {@link AgentPanel}. */
interface AgentPanelProps {
  /** Whether the dialog is shown. */
  open: boolean;
  /** Closes the dialog. */
  onClose: () => void;
  /** `GET /api/mcp/status`, or null while loading / unavailable. */
  status: McpStatus | null;
  /** Copies text to the clipboard and toasts (owned by App). */
  onCopy: (label: string, text: string) => void;
  /** Surfaces errors through the app's toast. */
  onError: (title: string, message: string) => void;
}

const pre: React.CSSProperties = {
  margin: 0,
  padding: 10,
  borderRadius: 'var(--radius-sm)',
  background: 'var(--surface-input)',
  border: '1px solid var(--border)',
  fontFamily: 'var(--font-mono)',
  fontSize: 'var(--text-2xs)',
  whiteSpace: 'pre-wrap',
  wordBreak: 'break-all',
};

function Snippet({ title, text, onCopy }: { title: string; text: string; onCopy: (l: string, t: string) => void }) {
  return (
    <div style={{ display: 'flex', flexDirection: 'column', gap: 4 }}>
      <div className="flex items-center justify-between">
        <span style={{ fontSize: 'var(--text-xs)', color: 'var(--text-secondary)' }}>{title}</span>
        <button
          aria-label={`Copy ${title}`}
          onClick={() => onCopy(title, text)}
          className="flex items-center gap-1"
          style={{ fontSize: 'var(--text-2xs)', color: 'var(--text-primary)', background: 'transparent', border: 'none' }}
        >
          <Copy size={11} /> Copy
        </button>
      </div>
      <pre style={pre}>{text}</pre>
    </div>
  );
}

/** Shown when MCP is off or not compiled in. */
function DisabledNotice({ status }: { status: McpStatus | null }) {
  return (
    <div style={{ padding: '8px 0', display: 'flex', flexDirection: 'column', gap: 8 }}>
      <p style={{ fontSize: 'var(--text-sm)', color: 'var(--text-secondary)', margin: 0 }}>
        {status && !status.compiled
          ? 'This gateway build has no MCP support (built without the mcp feature).'
          : 'The MCP server is off. Enable it in system.yaml with at least one scoped token and restart the gateway.'}
      </p>
      <pre style={pre}>{`admin:\n  mcp:\n    enabled: \${FEATHERBIT_MCP_ENABLED:-false}\n    tokens:\n      - token: \${FEATHERBIT_MCP_READ_TOKEN}\n        scope: read\n      - token: \${FEATHERBIT_MCP_WRITE_TOKEN}\n        scope: write`}</pre>
      <p style={{ fontSize: 'var(--text-2xs)', color: 'var(--text-muted)', margin: 0 }}>
        "Copy as agent prompt" in the Debug panel and the policy editor works regardless — the copied text inlines the data.
      </p>
    </div>
  );
}

export function AgentPanel({ open, onClose, status, onCopy, onError }: AgentPanelProps) {
  const [prompts, setPrompts] = useState<PromptDef[]>([]);

  useEffect(() => {
    if (!open) return;
    // eslint-disable-next-line react-hooks/set-state-in-effect
    api.listPrompts().then(setPrompts).catch((e) => onError('Failed to load prompts', parseApiError(e).error));
  }, [open, onError]);

  const enabled = !!status?.enabled;
  const url = mcpEndpoint(window.location.origin, status?.path ?? '/mcp');
  const snippets = clientSnippets(url);

  return (
    <Dialog
      open={open}
      title="Agent"
      width={720}
      onClose={onClose}
      footer={
        <DialogButton variant="ghost" onClick={onClose}>
          Close
        </DialogButton>
      }
    >
      <div style={{ maxHeight: '64vh', overflowY: 'auto', display: 'flex', flexDirection: 'column', gap: 14 }}>
        {!enabled ? (
          <DisabledNotice status={status} />
        ) : (
          <>
            <div>
              <div style={{ fontSize: 'var(--text-xs)', color: 'var(--text-secondary)' }}>MCP endpoint</div>
              <code data-testid="mcp-endpoint" style={{ fontFamily: 'var(--font-mono)', fontSize: 'var(--text-sm)' }}>
                {url}
              </code>
              <div style={{ fontSize: 'var(--text-2xs)', color: 'var(--text-muted)', marginTop: 2 }}>
                {status?.token_count} token{status?.token_count === 1 ? '' : 's'} configured ({status?.scopes.join(', ') || 'none'}). Tokens live in system.yaml — paste yours where the snippets say &lt;TOKEN&gt;.
              </div>
            </div>
            <Snippet title="Claude Code" text={snippets.claudeCode} onCopy={onCopy} />
            <Snippet title="mcpServers JSON (Claude Desktop, Cursor, Windsurf)" text={snippets.mcpJson} onCopy={onCopy} />
            <Snippet title="curl smoke test" text={snippets.curl} onCopy={onCopy} />
            <div>
              <div style={{ fontSize: 'var(--text-xs)', color: 'var(--text-secondary)', marginBottom: 4 }}>Scopes</div>
              <p style={{ fontSize: 'var(--text-2xs)', color: 'var(--text-muted)', margin: '0 0 4px' }}>
                <b>read</b>: {READ_TOOLS.join(', ')}
              </p>
              <p style={{ fontSize: 'var(--text-2xs)', color: 'var(--text-muted)', margin: 0 }}>
                <b>write</b> (also everything above): {WRITE_TOOLS.join(', ')}
              </p>
            </div>
          </>
        )}
        <div>
          <div style={{ fontSize: 'var(--text-xs)', color: 'var(--text-secondary)', marginBottom: 4 }}>Prompt library</div>
          <ul style={{ margin: 0, paddingLeft: 16, display: 'flex', flexDirection: 'column', gap: 4 }}>
            {prompts.map((p) => (
              <li key={p.name} style={{ fontSize: 'var(--text-2xs)', color: 'var(--text-muted)' }}>
                <code style={{ fontFamily: 'var(--font-mono)', color: 'var(--text-primary)' }}>{p.name}</code>(
                {p.arguments.map((a) => (a.required ? a.name : `${a.name}?`)).join(', ')}) — {p.description}
              </li>
            ))}
          </ul>
          <p style={{ fontSize: 'var(--text-2xs)', color: 'var(--text-muted)', marginTop: 6 }}>
            Trace prompts are one click away in the Debug panel; policy prompts in the editor toolbar and the Ctrl+K palette.
          </p>
        </div>
      </div>
    </Dialog>
  );
}
```

- [ ] **Step 2: Sidebar button**

In `ui/src/components/Sidebar.tsx`: add `Bot` to the lucide import; add to `SidebarProps`

```tsx
  /** Opens the Agent (MCP) panel. */
  onOpenAgent: () => void;
  /** Whether the MCP server is on (dims the button when off, like Debug). */
  mcpEnabled: boolean;
```

destructure both, and insert **before** the Certificates button:

```tsx
        <button
          onClick={onOpenAgent}
          aria-label="Agent"
          title={mcpEnabled ? 'Connect an AI agent over MCP; copy prompts' : 'MCP is off — set admin.mcp.enabled in system.yaml and restart'}
          className="w-full flex items-center justify-center gap-1.5 transition-colors"
          style={{
            padding: '7px 0',
            borderRadius: 'var(--radius-sm)',
            fontSize: 'var(--text-xs)',
            fontWeight: 'var(--weight-medium)' as never,
            background: 'var(--surface-input)',
            color: mcpEnabled ? 'var(--text-primary)' : 'var(--text-muted)',
            border: '1px solid var(--border)',
          }}
          onMouseEnter={(e) => (e.currentTarget.style.filter = 'brightness(1.08)')}
          onMouseLeave={(e) => (e.currentTarget.style.filter = 'none')}
        >
          <Bot size={12} />
          Agent
        </button>
```

- [ ] **Step 3: App wiring**

In `ui/src/App.tsx`:
- `import { AgentPanel } from './components/AgentPanel';` and `import type { McpStatus } from './types';` (merge with existing type imports).
- State next to the certs state: `const [agentOpen, setAgentOpen] = useState(false); const [mcpStatus, setMcpStatus] = useState<McpStatus | null>(null);`
- In `loadData`, after the debug config try/catch:
  ```tsx
    try {
      setMcpStatus(await api.mcpStatus());
    } catch {
      setMcpStatus(null);
    }
  ```
- Memoized clipboard helper next to `handlePanelError`:
  ```tsx
  const copyText = useCallback(
    async (label: string, text: string) => {
      try {
        await navigator.clipboard.writeText(text);
        notify({ tone: 'success', title: 'Copied to clipboard', message: label });
      } catch (e) {
        notify({ tone: 'error', title: 'Copy failed', message: `${e}` });
      }
    },
    [notify],
  );
  ```
- Sidebar props: `onOpenAgent={() => setAgentOpen(true)} mcpEnabled={mcpStatus?.enabled ?? false}`.
- Render next to `<CertificatesPanel …/>`:
  ```tsx
      <AgentPanel open={agentOpen} onClose={() => setAgentOpen(false)} status={mcpStatus} onCopy={copyText} onError={handlePanelError} />
  ```

- [ ] **Step 4: Verify and commit**

Run: `cd ui && npm run lint && npm test && npm run build`, then `cargo build --release` and open the UI: the footer shows **Agent**; with MCP off the panel shows the YAML snippet and the prompt library; with MCP on (set `FEATHERBIT_MCP_ENABLED=true` + tokens) it shows the endpoint and snippets and "Copy" toasts.

```bash
git add ui/src/components/AgentPanel.tsx ui/src/components/Sidebar.tsx ui/src/App.tsx
git commit -m "feat(ui): Agent panel with MCP status, client snippets, scopes and prompt library"
```

---

### Task 3: "Copy as agent prompt" in the trace viewer and the policy editor

**Files:**
- Modify: `ui/src/components/TraceViewer.tsx` (`TraceHeader` `:339-391`, detail pane `:226-333`), `ui/src/components/DebugPanel.tsx:344-346`, `ui/src/App.tsx`, `ui/src/commands.ts`, `ui/src/components/GraphCanvas.tsx` (toolbar `:835-920`)

**Interfaces:**
- `TraceHeader` gains `onCopyPrompt?: (prompt: 'explain_trace' | 'why_this_response') => void`; `TraceViewer` gains `onCopyPrompt?: (nodeId: string) => void` (renders a "Why this port?" button in the detail pane).
- `DebugPanel` gains `onCopyPrompt: (name: string, args: Record<string, string>) => void` and passes closures down.
- App: `copyPrompt = useCallback(async (name, args) => { const r = await api.renderPrompt(name, args); await copyText(r.description, withMcpHint(r.text)); }, [copyText])` with error → `handlePanelError`.
- `CommandContext` gains `agentPrompt: (name: 'review_policy' | 'design_policy' | 'design_supernode' | 'design_route') => void; openAgentPanel: () => void`.
- `GraphCanvas` gains `onReviewWithAgent?: () => void` (policy mode toolbar button).

- [ ] **Step 1: TraceViewer/TraceHeader props**

In `TraceHeader`, add the prop and, right after the `onCopyToSandbox` block (inside the same flex row), two buttons using the same style object as "Copy to sandbox":

```tsx
        {onCopyPrompt && (
          <>
            <button onClick={() => onCopyPrompt('explain_trace')} title="Copy a prompt asking an agent to explain this whole trace" style={headerButton}>
              Copy as agent prompt
            </button>
            <button onClick={() => onCopyPrompt('why_this_response')} title={`Copy a prompt asking why the client got ${trace.status}`} style={headerButton}>
              Why {trace.status}?
            </button>
          </>
        )}
```

Extract the existing inline style of "Copy to sandbox" into `const headerButton: React.CSSProperties = { … }` at module level and reuse it for all three (behavior of the existing button unchanged).

In `TraceViewer`, add `onCopyPrompt?: (nodeId: string) => void` to `TraceViewerProps` and, in the detail pane beside the `Changes` eyebrow (`:278-280`), render:

```tsx
            {onCopyPrompt && (
              <button
                onClick={() => onCopyPrompt(step.node_id)}
                title={`Copy a prompt asking why ${step.node_id} exited on port ${step.port ?? 'error'}`}
                style={headerButton}
              >
                Why this port?
              </button>
            )}
```

- [ ] **Step 2: DebugPanel + App**

`DebugPanel.tsx`: add `onCopyPrompt: (name: string, args: Record<string, string>) => void` to props; where the detail renders:

```tsx
                    <TraceHeader
                      trace={detail}
                      onCopyToSandbox={() => copyTraceToSandbox(detail)}
                      onCopyPrompt={(p) => onCopyPrompt(p, { trace_id: detail.id })}
                    />
                    <TraceViewer key={detail.id} trace={detail} onCopyPrompt={(nodeId) => onCopyPrompt('why_this_port', { trace_id: detail.id, node_id: nodeId })} />
```

`App.tsx`:

```tsx
  const copyPrompt = useCallback(
    async (name: string, args: Record<string, string>) => {
      try {
        const r = await api.renderPrompt(name, args);
        await copyText(r.description, withMcpHint(r.text));
      } catch (e) {
        const p = parseApiError(e);
        handlePanelError('Could not build the agent prompt', p.error || p.raw);
      }
    },
    [copyText, handlePanelError],
  );
```

and pass `onCopyPrompt={copyPrompt}` to `<DebugPanel>`. Import `withMcpHint` from `./agentPrompts`.

- [ ] **Step 3: Palette commands and goal dialog**

`commands.ts` — add to `CommandContext`:

```ts
  /** Copies a policy-authoring prompt for the agent (opens a goal dialog for design_*). */
  agentPrompt: (name: 'review_policy' | 'design_policy' | 'design_supernode' | 'design_route') => void;
  /** Opens the Agent (MCP) panel. */
  openAgentPanel: () => void;
```

and to `buildCommands()`:

```ts
    { id: 'open-agent-panel', title: 'Open Agent panel (MCP)', run: (c) => c.openAgentPanel() },
    { id: 'agent-review-policy', title: 'Agent: copy "review this policy" prompt', when: (c) => c.editorOpen, run: (c) => c.agentPrompt('review_policy') },
    { id: 'agent-design-policy', title: 'Agent: copy "design a policy" prompt…', run: (c) => c.agentPrompt('design_policy') },
    { id: 'agent-design-supernode', title: 'Agent: copy "design a supernode" prompt…', run: (c) => c.agentPrompt('design_supernode') },
    { id: 'agent-design-route', title: 'Agent: copy "design a route" prompt…', run: (c) => c.agentPrompt('design_route') },
```

`App.tsx` — state `const [goalDialog, setGoalDialog] = useState<null | 'design_policy' | 'design_supernode' | 'design_route'>(null); const [goalText, setGoalText] = useState('');` and a handler:

```tsx
  const agentPrompt = useCallback(
    (name: 'review_policy' | 'design_policy' | 'design_supernode' | 'design_route') => {
      if (name === 'review_policy') {
        if (!selectedPolicy) {
          notify({ tone: 'warning', title: 'Open a policy first' });
          return;
        }
        void copyPrompt('review_policy', { policy_name: selectedPolicy.name });
        return;
      }
      setGoalText('');
      setGoalDialog(name);
    },
    [selectedPolicy, copyPrompt, notify],
  );
```

Add `agentPrompt` and `openAgentPanel: () => setAgentOpen(true)` to the memoized `commandCtx` object **and its dependency array** (`App.tsx:640-671`). Render the goal dialog next to the other dialogs:

```tsx
      <Dialog
        open={goalDialog !== null}
        title="Describe the goal for the agent"
        onClose={() => setGoalDialog(null)}
        footer={
          <>
            <DialogButton variant="ghost" onClick={() => setGoalDialog(null)}>Cancel</DialogButton>
            <DialogButton
              disabled={!goalText.trim()}
              onClick={() => {
                const name = goalDialog!;
                setGoalDialog(null);
                void copyPrompt(name, { goal: goalText.trim() });
              }}
            >
              Copy prompt
            </DialogButton>
          </>
        }
      >
        <DialogField label="Goal" value={goalText} onChange={setGoalText} placeholder="rate-limit /api by API key, 100 req/min, 429 on excess" autoFocus />
      </Dialog>
```

- [ ] **Step 4: Toolbar button**

`GraphCanvas.tsx`: add prop `onReviewWithAgent?: () => void`; in the floating toolbar, after the Add Node button, render when `kind === 'policy' && onReviewWithAgent`:

```tsx
            {kind === 'policy' && onReviewWithAgent && (
              <button
                onClick={onReviewWithAgent}
                title="Copy a prompt asking an agent to review this policy"
                style={{ ...toolbarButtonStyle('var(--surface-input)'), color: 'var(--text-primary)', border: '1px solid var(--border)' }}
                onMouseEnter={(e) => (e.currentTarget.style.filter = 'brightness(1.08)')}
                onMouseLeave={(e) => (e.currentTarget.style.filter = 'none')}
              >
                <Bot size={13} />
                Review with agent
              </button>
            )}
```

(`Bot` from lucide-react.) In App pass `onReviewWithAgent={() => agentPrompt('review_policy')}` to `<GraphCanvas>`.

- [ ] **Step 5: Verify and commit**

Run: `cd ui && npm run lint && npm test && npm run build && cargo build --release` (from repo root for cargo). Manually: with debug on, run the sandbox, open a trace → "Copy as agent prompt", "Why 200?", select a step → "Why this port?" each toast "Copied to clipboard" and the clipboard ends with the MCP hint line. Ctrl+K → "Agent: copy "design a policy" prompt…" opens the goal dialog.

```bash
git add ui/src
git commit -m "feat(ui): copy-as-agent-prompt actions in the trace viewer, policy toolbar and command palette"
```

---

### Task 4: e2e scenarios and testbook

**Files:**
- Modify: `e2e/fixtures/system.yaml`, `e2e/E2E_TESTBOOK.md`
- Create: `e2e/tests/mcp.spec.ts`

- [ ] **Step 1: Enable MCP in the fixture**

Append to `e2e/fixtures/system.yaml` under `admin:` (the suite is single-run, static config — same reasoning as the `debug:` comment there):

```yaml
  # MCP is on for the whole e2e run (static config, like debug above). The
  # disabled path is covered by unit tests in src/admin/mod.rs and src/mcp.
  mcp:
    enabled: true
    tokens:
      - token: e2e-read-token-0123456789
        scope: read
        name: e2e-reader
      - token: e2e-write-token-0123456789
        scope: write
```

- [ ] **Step 2: The spec**

Create `e2e/tests/mcp.spec.ts`:

```ts
import {expect, test} from '@playwright/test';
import {ADMIN_URL} from '../playwright.config';
import {adminApi} from '../helpers/admin';

const READ = 'e2e-read-token-0123456789';
const WRITE = 'e2e-write-token-0123456789';
const MCP = `${ADMIN_URL}/mcp`;

/** Minimal MCP client over Streamable HTTP: one JSON-RPC call per request (stateless from our side). */
async function rpc(token: string | null, method: string, params: unknown, id = 1, session?: string) {
  const headers: Record<string, string> = {
    'content-type': 'application/json',
    accept: 'application/json, text/event-stream',
  };
  if (token) headers.authorization = `Bearer ${token}`;
  if (session) headers['mcp-session-id'] = session;
  const res = await fetch(MCP, {method: 'POST', headers, body: JSON.stringify({jsonrpc: '2.0', id, method, params})});
  const text = await res.text();
  // The server may answer as plain JSON or as a single SSE event.
  const data = text.trim().startsWith('{') ? text : text.split('\n').filter((l) => l.startsWith('data:')).map((l) => l.slice(5).trim()).join('');
  return {status: res.status, session: res.headers.get('mcp-session-id') ?? session, body: data ? JSON.parse(data) : null};
}

const INIT = {protocolVersion: '2025-03-26', capabilities: {}, clientInfo: {name: 'e2e', version: '0'}};

async function initialized(token: string) {
  const init = await rpc(token, 'initialize', INIT);
  expect(init.status).toBe(200);
  await fetch(MCP, {method: 'POST', headers: {'content-type': 'application/json', accept: 'application/json, text/event-stream', authorization: `Bearer ${token}`, ...(init.session ? {'mcp-session-id': init.session} : {})}, body: JSON.stringify({jsonrpc: '2.0', method: 'notifications/initialized'})});
  return init.session;
}

test.describe('MCP server', () => {
  test('E2E-MCP-01: bearer scopes gate the tool list; anonymous and Basic Auth are refused', async () => {
    const anon = await rpc(null, 'initialize', INIT);
    expect(anon.status).toBe(401);

    const basic = await fetch(MCP, {method: 'POST', headers: {'content-type': 'application/json', accept: 'application/json, text/event-stream', authorization: 'Basic ' + Buffer.from('admin:admin').toString('base64')}, body: JSON.stringify({jsonrpc: '2.0', id: 1, method: 'initialize', params: INIT})});
    expect(basic.status).toBe(401);

    const rs = await initialized(READ);
    const readTools = await rpc(READ, 'tools/list', {}, 2, rs);
    const readNames = readTools.body.result.tools.map((t: {name: string}) => t.name);
    expect(readNames).toContain('get_policy');
    expect(readNames.some((n: string) => n.startsWith('put_'))).toBe(false);

    const ws = await initialized(WRITE);
    const writeTools = await rpc(WRITE, 'tools/list', {}, 2, ws);
    const writeNames = writeTools.body.result.tools.map((t: {name: string}) => t.name);
    expect(writeNames).toContain('put_policy');

    const forbidden = await rpc(READ, 'tools/call', {name: 'put_policy', arguments: {name: 'x', definition: {}}}, 3, rs);
    expect(forbidden.body.result.isError).toBe(true);
    expect(JSON.parse(forbidden.body.result.content[0].text).code).toBe('forbidden');

    // An MCP token does not open the Admin API.
    const api = await fetch(`${ADMIN_URL}/api/policies`, {headers: {authorization: `Bearer ${WRITE}`}});
    expect(api.status).toBe(401);
  });

  test('E2E-MCP-02: Agent panel shows the endpoint, snippets and a scope list matching tools/list', async ({page}) => {
    await page.goto('/');
    await page.getByRole('button', {name: 'Agent'}).click();
    const panel = page.getByRole('dialog', {name: 'Agent'});
    await expect(panel.getByTestId('mcp-endpoint')).toHaveText(`${ADMIN_URL}/mcp`);
    await expect(panel.getByText('claude mcp add --transport http featherbit')).toBeVisible();
    await expect(panel.getByText('<TOKEN>').first()).toBeVisible();
    await expect(panel.getByText('why_this_port')).toBeVisible();

    const ws = await initialized(WRITE);
    const all = await rpc(WRITE, 'tools/list', {}, 2, ws);
    const serverNames: string[] = all.body.result.tools.map((t: {name: string}) => t.name).sort();
    const scopeText = await panel.getByText(/^read:/).textContent();
    const writeText = await panel.getByText(/^write/).textContent();
    const uiNames = `${scopeText} ${writeText}`.match(/[a-z_]+_[a-z_]+/g)!.filter((n) => serverNames.includes(n) || n.includes('_'));
    for (const n of serverNames) expect(uiNames, `UI scope explainer lists ${n}`).toContain(n);
  });

  test('E2E-MCP-03: "Why this port?" on a trace step copies a prompt naming the node, the port and the MCP hint', async ({page, context}) => {
    await context.grantPermissions(['clipboard-read', 'clipboard-write']);
    const api = await adminApi();
    // Make a trace via the sandbox against the fixture's first policy.
    const policies = await (await api.get('/api/policies')).json();
    const policy = policies[0].name as string;
    const run = await api.post('/api/debug/sandbox', {data: {policy, context: {method: 'GET', path: '/'}}});
    expect(run.ok()).toBeTruthy();
    const traceId = (await run.json()).stored_trace_id as string;

    await page.goto('/');
    await page.getByRole('button', {name: 'Debug'}).click();
    const debug = page.getByRole('dialog', {name: 'Debug'});
    await debug.getByText(traceId.slice(0, 8)).first().click().catch(async () => {
      // The list shows method/path rather than ids: open the newest row.
      await debug.getByRole('button').filter({hasText: policy}).first().click();
    });
    await debug.getByRole('button', {name: 'Why this port?'}).click();
    await expect(page.getByText('Copied to clipboard')).toBeVisible();
    const text = await page.evaluate(() => navigator.clipboard.readText());
    expect(text).toContain('exit on port');
    expect(text).toContain('If the `featherbit` MCP server is connected');
    expect(text).toContain(policy);
    await api.dispose();
  });
});
```

Adjust the E2E-MCP-03 trace-row locator to how `DebugPanel` actually renders rows (`DebugPanel.tsx:309-339` — buttons with method/path/status text): prefer the newest row via `.first()` after the list loads. If the trace list polls, wait with `await expect(debug.getByRole('button').filter({hasText: policy}).first()).toBeVisible()` first. In E2E-MCP-02, keep the assertion simple if the regex extraction is brittle: for each `serverNames` entry assert `panel.getByText(name, {exact: false})` is visible.

- [ ] **Step 3: Testbook**

Append a section to `e2e/E2E_TESTBOOK.md` (before the last section, matching the existing format):

```markdown
## MCP server (`tests/mcp.spec.ts`)

The fixture `system.yaml` enables `admin.mcp` for the whole run with a `read` and a `write` token (the disabled path is unit-tested in `src/admin/mod.rs` and `src/mcp/server.rs`).

| ID | Scenario | Expected |
|---|---|---|
| E2E-MCP-01 | `initialize` without a token and with Basic Auth; `tools/list` with the read and the write token; `tools/call put_policy` with the read token; `GET /api/policies` with an MCP token | `401` / `401`; the read list has `get_policy` and no `put_*`; the write list has `put_policy`; the read-token write call is a tool error `{"code":"forbidden"}`; the Admin API answers `401` |
| E2E-MCP-02 | **Browser.** Footer → **Agent** | The dialog shows the endpoint `http://127.0.0.1:19091/mcp`, a Claude Code snippet with `<TOKEN>`, the prompt library (`why_this_port`), and a scope explainer listing every tool name returned by `tools/list` |
| E2E-MCP-03 | **Browser.** Sandbox-run the first fixture policy, open the trace in Debug, click **Why this port?** | Toast "Copied to clipboard"; the clipboard text contains `exit on port`, the policy name, and the MCP hint line |
```

- [ ] **Step 4: Run and commit**

Run: `cd e2e && npm run test:all -- -g E2E-MCP` then the whole suite `npm test`.
Expected: the three new scenarios pass; nothing else regresses.

```bash
git add e2e/fixtures/system.yaml e2e/tests/mcp.spec.ts e2e/E2E_TESTBOOK.md
git commit -m "test(e2e): MCP scopes over HTTP, Agent panel, and copy-as-agent-prompt scenarios"
```

---

### Task 5: Docs and wrap-up

**Files:**
- Modify: `website/docs/guides/web-ui.md`, `website/docs/guides/mcp.md`

- [ ] **Step 1: Web UI guide**

In `website/docs/guides/web-ui.md`, add a section (near the Debug/Sessions panel sections):

```markdown
## Agent panel and agent prompts

The footer's **Agent** button opens the MCP connection panel: the endpoint URL, copy-paste client configs (Claude Code, `mcpServers` JSON, curl) with a `<TOKEN>` placeholder you fill from your `system.yaml` tokens, a read/write scope explainer, and the library of precompiled prompts. With MCP disabled it shows the config to set instead.

You do not need a connected agent to use the prompts: in the Debug panel a trace has **Copy as agent prompt** and **Why <status>?**, and a selected step has **Why this port?**; the policy editor's toolbar has **Review with agent**, and the Ctrl+K palette has *Agent: copy "design a policy/supernode/route" prompt…* (asks for the goal). Each copies a prompt with the relevant data inlined — paste it into any chat — plus a line telling a connected agent to prefer the live MCP tools. See [MCP server for agents](./mcp.md).
```

In `website/docs/guides/mcp.md`, under "Connecting a client", add: "The web UI's **Agent** panel (footer) shows these snippets with your actual endpoint, and the Debug panel / policy editor offer **Copy as agent prompt** actions that produce the same prompts with data inlined." (if Task 11 of the backend plan already wrote a similar sentence, merge rather than duplicate).

- [ ] **Step 2: Build docs, full verification, commit**

Run: `cd website && npm run build`; `cd ui && npm run lint && npm test && npm run build`; `cargo test --locked`; `cd e2e && npm run test:all`; `graphify update .`.

```bash
git add website/docs/guides/web-ui.md website/docs/guides/mcp.md graphify-out
git commit -m "docs(ui): Agent panel and copy-as-agent-prompt actions"
```

Report to Francesco with the branch state and offer to open the PR against `develop`.
