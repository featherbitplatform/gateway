/**
 * Notifications panel: the inspectable history behind the transient toasts.
 *
 * Lists every logged notification newest first (see `useNotificationLog`),
 * with an All / Errors filter, click-to-expand rows showing the full payload
 * (typically the server's error body) with a Copy button, and a Clear action.
 * Opened from the sidebar bell, an error toast's "Details" link (which
 * pre-expands that entry), or the Ctrl+K palette.
 *
 * @module components/NotificationsPanel
 */
import { useState } from 'react';
import { CircleCheck, CircleX, Copy, TriangleAlert } from 'lucide-react';
import { Dialog, DialogButton } from './Dialog';
import type { NotificationEntry, NotificationTone } from '../notifications';

interface NotificationsPanelProps {
  /** Whether the dialog is shown. */
  open: boolean;
  /** Closes the dialog. */
  onClose: () => void;
  /** The log, newest first. */
  entries: NotificationEntry[];
  /** Empties the log. */
  onClear: () => void;
  /** Entry to show expanded when the panel opens (from a toast's "Details"), or null. */
  focusId?: string | null;
}

type Filter = 'all' | 'errors';

const TONE_COLOR: Record<NotificationTone, string> = {
  success: 'var(--success)',
  warning: 'var(--warning)',
  error: 'var(--error)',
};

const TONE_ICON: Record<NotificationTone, typeof CircleCheck> = {
  success: CircleCheck,
  warning: TriangleAlert,
  error: CircleX,
};

/** "12:34:56 · 28 Aug" — absolute, since the log is a timeline. */
function formatTs(ts: number): string {
  const d = new Date(ts);
  const time = d.toLocaleTimeString(undefined, { hour: '2-digit', minute: '2-digit', second: '2-digit' });
  const day = d.toLocaleDateString(undefined, { day: 'numeric', month: 'short' });
  return `${time} · ${day}`;
}

/**
 * The notification history dialog.
 *
 * Expansion and filter state are per-open: the parent remounts the panel
 * (via `key`) each time it opens, so the `focusId` a toast asked to show is
 * simply the initial expansion and a stale state from a previous visit
 * never competes with it.
 */
export function NotificationsPanel({ open, onClose, entries, onClear, focusId = null }: NotificationsPanelProps) {
  const [filter, setFilter] = useState<Filter>('all');
  const [expandedId, setExpandedId] = useState<string | null>(focusId);
  const [copiedId, setCopiedId] = useState<string | null>(null);

  const visible = filter === 'all' ? entries : entries.filter((e) => e.tone !== 'success');

  const copy = async (entry: NotificationEntry) => {
    try {
      await navigator.clipboard.writeText(entry.details ?? entry.message ?? entry.title);
      setCopiedId(entry.id);
      setTimeout(() => setCopiedId((id) => (id === entry.id ? null : id)), 1500);
    } catch {
      // Clipboard blocked (insecure context / permissions): the text is on screen anyway.
    }
  };

  const chip = (value: Filter, label: string, count: number) => {
    const active = filter === value;
    return (
      <button
        key={value}
        onClick={() => setFilter(value)}
        aria-pressed={active}
        style={{
          padding: '3px 10px',
          borderRadius: 999,
          fontSize: 'var(--text-xs)',
          fontWeight: 'var(--weight-medium)' as never,
          background: active ? 'var(--accent-soft, var(--surface-input))' : 'transparent',
          color: active ? 'var(--text-primary)' : 'var(--text-muted)',
          border: `1px solid ${active ? 'var(--accent)' : 'var(--border)'}`,
        }}
      >
        {label}
        {/* Decorative count: kept out of the accessible name so the chip is still "Errors". */}
        <span aria-hidden="true" style={{ marginLeft: 6, fontFamily: 'var(--font-mono)', opacity: 0.8 }}>
          {count}
        </span>
      </button>
    );
  };

  return (
    <Dialog
      open={open}
      title="Notifications"
      onClose={onClose}
      width={640}
      footer={
        <>
          <DialogButton variant="ghost" onClick={onClear} disabled={entries.length === 0}>
            Clear log
          </DialogButton>
          <DialogButton onClick={onClose}>Close</DialogButton>
        </>
      }
    >
      <div className="flex items-center gap-2" style={{ marginBottom: 10 }}>
        {chip('all', 'All', entries.length)}
        {chip('errors', 'Errors', entries.filter((e) => e.tone !== 'success').length)}
        <span style={{ marginLeft: 'auto', fontSize: 'var(--text-xs)', color: 'var(--text-muted)' }}>
          Stored in this browser · newest first
        </span>
      </div>

      {visible.length === 0 ? (
        <p
          style={{
            padding: '28px 0',
            textAlign: 'center',
            fontSize: 'var(--text-sm)',
            color: 'var(--text-muted)',
            margin: 0,
          }}
        >
          {entries.length === 0 ? 'No notifications yet' : 'No errors or warnings logged'}
        </p>
      ) : (
        <div className="flex flex-col gap-1.5" style={{ maxHeight: '60vh', overflowY: 'auto' }}>
          {visible.map((entry) => {
            const Icon = TONE_ICON[entry.tone];
            const color = TONE_COLOR[entry.tone];
            const expanded = expandedId === entry.id;
            const payload = entry.details ?? entry.message;
            return (
              <div
                key={entry.id}
                style={{
                  border: '1px solid var(--border)',
                  borderLeft: `2px solid ${color}`,
                  borderRadius: 'var(--radius-sm)',
                  background: 'var(--surface-input)',
                }}
              >
                <button
                  onClick={() => setExpandedId(expanded ? null : entry.id)}
                  aria-expanded={expanded}
                  className="w-full flex items-start gap-2.5 text-left"
                  style={{ padding: '8px 10px', background: 'transparent', border: 'none', color: 'inherit' }}
                >
                  <Icon size={14} style={{ color, flexShrink: 0, marginTop: 2 }} />
                  <span className="flex-1 min-w-0">
                    <span
                      className="block"
                      style={{ fontSize: 'var(--text-sm)', fontWeight: 600, color: 'var(--text-primary)' }}
                    >
                      {entry.title}
                    </span>
                    {entry.message && !expanded && (
                      <span
                        className="block truncate"
                        style={{
                          fontFamily: 'var(--font-mono)',
                          fontSize: 'var(--text-xs)',
                          color: 'var(--text-secondary)',
                          marginTop: 2,
                        }}
                      >
                        {entry.message}
                      </span>
                    )}
                  </span>
                  <span
                    style={{
                      fontFamily: 'var(--font-mono)',
                      fontSize: 'var(--text-xs)',
                      color: 'var(--text-muted)',
                      flexShrink: 0,
                      marginTop: 2,
                    }}
                  >
                    {formatTs(entry.ts)}
                  </span>
                </button>
                {expanded && payload && (
                  <div style={{ padding: '0 10px 10px 34px' }}>
                    <div className="flex items-center justify-between" style={{ marginBottom: 4 }}>
                      <span style={{ fontSize: 'var(--text-xs)', color: 'var(--text-muted)' }}>Details</span>
                      <button
                        onClick={() => void copy(entry)}
                        className="flex items-center gap-1"
                        aria-label="Copy details"
                        style={{
                          fontSize: 'var(--text-xs)',
                          color: 'var(--text-muted)',
                          background: 'transparent',
                          border: 'none',
                        }}
                      >
                        <Copy size={12} />
                        {copiedId === entry.id ? 'Copied' : 'Copy'}
                      </button>
                    </div>
                    <pre
                      data-testid="notification-details"
                      style={{
                        margin: 0,
                        padding: '8px 10px',
                        maxHeight: 220,
                        overflow: 'auto',
                        fontFamily: 'var(--font-mono)',
                        fontSize: 'var(--text-xs)',
                        lineHeight: 1.5,
                        color: 'var(--text-secondary)',
                        background: 'var(--surface)',
                        border: '1px solid var(--border)',
                        borderRadius: 'var(--radius-sm)',
                        whiteSpace: 'pre-wrap',
                        overflowWrap: 'anywhere',
                      }}
                    >
                      {payload}
                    </pre>
                  </div>
                )}
              </div>
            );
          })}
        </div>
      )}
    </Dialog>
  );
}
