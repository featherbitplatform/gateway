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

// eslint-disable-next-line react-refresh/only-export-components -- ChatPanel imports this alongside the component; not worth a separate file for one helper.
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
