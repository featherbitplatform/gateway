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
