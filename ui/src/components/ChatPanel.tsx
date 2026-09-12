import { useState } from 'react';
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
  // null = automatic (settings form when there is no API key yet); a
  // boolean is an explicit override from the gear button or a navigation
  // action, until the panel is closed and the automatic behaviour resumes.
  const [settingsToggle, setSettingsToggle] = useState<boolean | null>(null);
  const [draft, setDraft] = useState('');
  const showSettings = settingsToggle ?? chat.connection.kind === 'no-key';

  const active = chat.threads.find((t) => t.id === chat.activeId) ?? null;
  const busy = chat.busyThreadId !== null;

  const submit = () => {
    const text = draft.trim();
    if (!text || busy || chat.connection.kind === 'no-key') return;
    const id = chat.activeId ?? chat.newThread();
    setDraft('');
    void chat.send(id, text);
  };

  const close = () => {
    setSettingsToggle(null);
    onClose();
  };

  return (
    <Dialog
      open={open}
      title="Chat"
      width={1040}
      onClose={close}
      footer={
        <DialogButton variant="ghost" onClick={close}>
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
            setSettingsToggle(false);
          }}
          onNew={() => {
            chat.newThread();
            setSettingsToggle(false);
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
              {chat.settings.autoApprove && ' · auto-run writes ON'}
              {chat.storageBlocked && ' · storage blocked: chats will not survive a reload'}
            </span>
            <label
              className="flex items-center gap-1"
              title={
                chat.connection.kind === 'ready' && chat.connection.scope === 'read'
                  ? 'Your MCP token is read-only: there are no writes to approve'
                  : 'Run write tools and run_sandbox without asking for Run/Skip'
              }
              style={{ fontSize: 'var(--text-2xs)', color: 'var(--text-secondary)', whiteSpace: 'nowrap' }}
            >
              <input
                type="checkbox"
                checked={chat.settings.autoApprove}
                disabled={chat.connection.kind === 'ready' && chat.connection.scope === 'read'}
                onChange={(e) => chat.saveSettings({ ...chat.settings, autoApprove: e.target.checked })}
                aria-label="Auto-run writes"
              />
              Auto-run writes
            </label>
            <button
              aria-label="Chat settings"
              onClick={() => setSettingsToggle(!showSettings)}
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
              onDone={() => setSettingsToggle(false)}
            />
          ) : (
            <>
              <div style={{ flex: 1, overflowY: 'auto', maxHeight: '52vh' }}>
                {active ? (
                  <MessageList
                    thread={active}
                    pendingConfirm={chat.pendingConfirm}
                    onResolveConfirm={chat.resolveConfirm}
                    busy={chat.busyThreadId === active.id}
                  />
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
