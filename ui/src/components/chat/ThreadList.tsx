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
