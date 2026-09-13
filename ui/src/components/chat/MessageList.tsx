import { useEffect, useRef } from 'react';
import { Loader2 } from 'lucide-react';
import { buildRenderItems } from '../../chat/attempts';
import type { Thread } from '../../chat/store';
import type { PendingConfirm } from '../../chat/useChat';
import { Markdown } from './Markdown';
import { ToolCallCard } from './ToolCallCard';

interface MessageListProps {
  thread: Thread;
  pendingConfirm: PendingConfirm | null;
  onResolveConfirm: (run: boolean) => void;
  /** True while a turn runs on this thread: shows a "thinking" indicator until text or a tool card appears. */
  busy: boolean;
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

export function MessageList({ thread, pendingConfirm, onResolveConfirm, busy }: MessageListProps) {
  const endRef = useRef<HTMLDivElement>(null);
  useEffect(() => {
    endRef.current?.scrollIntoView({ block: 'end' });
  }, [thread.messages]);

  // Retries of a failed tool fold into one card (see chat/attempts.ts).
  const items = buildRenderItems(thread.messages);

  return (
    <div style={{ display: 'flex', flexDirection: 'column', gap: 8, padding: '4px 2px' }} data-testid="chat-messages">
      {items.map((item, i) => {
        if (item.kind === 'user') {
          return (
            <div key={i} style={bubble('user')} data-role="user">
              {item.content}
            </div>
          );
        }
        if (item.kind === 'assistant') {
          return (
            <div key={i} style={{ display: 'flex', flexDirection: 'column', gap: 6, alignSelf: 'stretch' }}>
              {item.content !== '' && (
                <div style={{ ...bubble('assistant'), whiteSpace: 'normal' }} data-role="assistant">
                  <Markdown text={item.content} />
                </div>
              )}
              {item.error && (
                <div style={{ ...bubble('assistant'), color: 'var(--error)', borderColor: 'var(--error)' }} data-role="error">
                  {item.error}
                </div>
              )}
            </div>
          );
        }
        const current = item.group.attempts[item.group.attempts.length - 1];
        return (
          <ToolCallCard
            key={current.call.id}
            group={item.group}
            awaitingConfirm={pendingConfirm?.threadId === thread.id && pendingConfirm.call.id === current.call.id}
            onRun={() => onResolveConfirm(true)}
            onSkip={() => onResolveConfirm(false)}
          />
        );
      })}
      {busy && !streamingText(thread) && (
        <div
          className="flex items-center gap-2"
          data-testid="chat-thinking"
          style={{ alignSelf: 'flex-start', padding: '6px 10px', fontSize: 'var(--text-2xs)', color: 'var(--text-muted)' }}
        >
          <Loader2 size={12} className="animate-spin" style={{ color: 'var(--accent)' }} />
          thinking…
        </div>
      )}
      <div ref={endRef} />
    </div>
  );
}

/** True while the last message is an assistant bubble that already has text (the stream is visible). */
function streamingText(thread: Thread): boolean {
  const last = thread.messages.at(-1);
  return last?.role === 'assistant' && last.content !== '' && !last.toolCalls?.length;
}
