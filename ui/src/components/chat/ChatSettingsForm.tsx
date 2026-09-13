import { useCallback, useEffect, useRef, useState } from 'react';
import { DialogButton, DialogField } from '../Dialog';
import { ProviderError, autoLoadKey, listModels } from '../../chat/openai';
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

  // Closest matches to what is typed, from the loaded ids (empty until the
  // list is loaded). Providers return dozens of models, so the list is long
  // enough to be worth scrolling rather than cut to the top few.
  const suggestions = rankModels(draft.model, models, 50);
  const pickModel = (m: string | undefined) => {
    if (m === undefined) return;
    setDraft((d) => ({ ...d, model: m }));
    setSuggestionsOpen(false);
  };

  // Keeps the arrow-key selection visible once the list is long enough to
  // scroll (providers return dozens of models).
  const listRef = useRef<HTMLUListElement>(null);
  useEffect(() => {
    if (!suggestionsOpen) return;
    listRef.current?.querySelector(`#chat-model-option-${highlight}`)?.scrollIntoView({ block: 'nearest' });
  }, [highlight, suggestionsOpen]);

  // Bumped per request so a slow reply cannot overwrite a newer one.
  const requestSeq = useRef(0);
  // The endpoint+key pair already auto-loaded, so typing does not re-fetch.
  const autoLoaded = useRef<string | null>(null);

  // Loads the provider's own GET /models into the combobox. The field stays
  // free text: servers without that endpoint (or with a different auth
  // model) still work by typing the name. An automatic load stays quiet —
  // it fills the list without opening the dropdown over what is being typed.
  const loadModels = useCallback(async (settings: ChatSettings, auto: boolean) => {
    const seq = ++requestSeq.current;
    setModelsNote('Loading models…');
    try {
      const ids = await listModels(settings);
      if (seq !== requestSeq.current) return;
      setModels(ids);
      if (!auto) {
        setSuggestionsOpen(true);
        setHighlight(0);
      }
      setModelsNote(ids.length === 0 ? 'The provider returned no models.' : `${ids.length} models — type to see the closest matches.`);
    } catch (e) {
      if (seq !== requestSeq.current) return;
      setModels([]);
      setModelsNote(e instanceof ProviderError ? `Could not load models: ${e.status} ${e.body}` : `Could not load models: ${String(e)}`);
    }
  }, []);

  // With a base URL and a key set, fetch the list on their own — once per
  // pair, and only after typing settles. Without a key nothing is fetched
  // automatically: the endpoint may or may not need auth, so that stays a
  // deliberate click.
  useEffect(() => {
    const key = autoLoadKey(draft);
    if (key === null || key === autoLoaded.current) return;
    const timer = setTimeout(() => {
      autoLoaded.current = key;
      void loadModels(draft, true);
    }, 600);
    return () => clearTimeout(timer);
  }, [draft, loadModels]);

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
            onClick={() => void loadModels(draft, false)}
            disabled={draft.baseUrl.trim() === ''}
            title={
              draft.baseUrl.trim() === ''
                ? 'Set the base URL first'
                : draft.apiKey.trim() === ''
                  ? 'Fetch the model list from the provider (GET /models). With no API key set it is sent unauthenticated, which only some endpoints allow.'
                  : 'Fetch the model list from the provider (GET /models). Loaded automatically when the base URL and key are set.'
            }
            style={{
              background: 'transparent',
              border: 'none',
              color: draft.baseUrl.trim() === '' ? 'var(--text-muted)' : 'var(--accent)',
              fontSize: 'var(--text-2xs)',
              padding: 0,
            }}
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
            aria-activedescendant={suggestionsOpen && suggestions.length > 0 ? `chat-model-option-${highlight}` : undefined}
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
                setSuggestionsOpen(true);
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
              ref={listRef}
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
                maxHeight: 260,
                overflowY: 'auto',
                overscrollBehavior: 'contain',
                background: 'var(--surface)',
                border: '1px solid var(--border)',
                borderRadius: 'var(--radius-sm)',
                boxShadow: 'var(--shadow-lg)',
              }}
            >
              {suggestions.map((m, i) => (
                <li
                  key={m}
                  id={`chat-model-option-${i}`}
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
      <label className="flex items-center gap-2" style={{ fontSize: 'var(--text-xs)', color: 'var(--text-secondary)' }}>
        <input
          type="checkbox"
          checked={draft.autoApprove}
          onChange={(e) => setDraft((d) => ({ ...d, autoApprove: e.target.checked }))}
          aria-label="Auto-run writes (settings)"
        />
        Auto-run writes: execute put_*/delete_*/reload_config and run_sandbox without the Run/Skip card
      </label>
      <div style={{ fontSize: 'var(--text-2xs)', color: 'var(--text-muted)' }} data-testid="chat-connection">{connectionLabel(connection)}</div>
      <div className="flex justify-between">
        <DialogButton
          variant="danger"
          onClick={() => {
            setDraft((d) => ({ ...d, apiKey: '', mcpToken: '' }));
            onForget();
          }}
        >
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
