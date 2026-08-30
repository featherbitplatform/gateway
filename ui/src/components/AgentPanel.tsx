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
