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
    if (open && chat.connection.kind === 'no-key') {
      // eslint-disable-next-line react-hooks/set-state-in-effect
      setShowSettings(true);
    }
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
