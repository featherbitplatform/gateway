/**
 * Sessions panel: browse and revoke server-side sessions for a declared
 * redis/valkey store.
 *
 * Rendered inside the shared {@link Dialog}, mirroring DebugPanel's traces
 * tab (stacked cards, load-on-open effect, no polling — a Refresh button
 * suffices since a revocation list has no liveness requirement).
 *
 * @module components/SessionsPanel
 */
import { useCallback, useEffect, useState } from 'react';
import type { CSSProperties } from 'react';
import { X } from 'lucide-react';
import { Dialog, DialogButton } from './Dialog';
import { formatUnixTime } from '../format';
import { api } from '../api/client';
import { parseApiError } from '../apiError';
import type { SessionPage, StoreConfig } from '../types';

/** Props for {@link SessionsPanel}. */
interface SessionsPanelProps {
  /** Whether the dialog is shown. */
  open: boolean;
  /** Closes the dialog. */
  onClose: () => void;
  /** Declared stores; the store select's options (first preselected). */
  stores: StoreConfig[];
  /** Surfaces errors through the app's toast. */
  onError: (title: string, message: string) => void;
}

const inputStyle: CSSProperties = {
  padding: '6px 10px',
  borderRadius: 'var(--radius-sm)',
  fontFamily: 'var(--font-mono)',
  fontSize: 'var(--text-xs)',
  background: 'var(--surface-input)',
  color: 'var(--text-primary)',
  border: '1px solid var(--border)',
};

/** Panel shown instead of the session list when the build lacks redis-store support. */
function HeadlessNotice() {
  return (
    <div style={{ padding: '8px 0' }}>
      <p style={{ fontSize: 'var(--text-sm)', color: 'var(--text-secondary)', margin: 0 }}>
        This gateway build has no redis-store support — sessions require the default build.
      </p>
    </div>
  );
}

/**
 * List/filter/revoke surface for one store's server-side sessions.
 *
 * Loads on open and on store/subject-filter change; a Refresh button covers
 * everything else, since (unlike Debug's live traces) a revocation list has
 * no reason to poll. Disabled (via `HeadlessNotice`) when the gateway build
 * has no redis-store support, detected from a 501 on the first list call.
 */
export function SessionsPanel({ open, onClose, stores, onError }: SessionsPanelProps) {
  const [store, setStore] = useState('');
  const [subjectInput, setSubjectInput] = useState('');
  // Applied filter — set only by the Apply button, so listSessions() is not
  // refired on every keystroke.
  const [subject, setSubject] = useState('');
  const [page, setPage] = useState<SessionPage | null>(null);
  const [headless, setHeadless] = useState(false);

  // Revoke-all-for-subject: two-step arm/confirm, plus the inline
  // "Revoked N sessions" flash shown in place of a toast (a single revoke's
  // feedback is just the row disappearing; bulk revoke gets one line here).
  const [armed, setArmed] = useState(false);
  const [revokedMessage, setRevokedMessage] = useState<string | null>(null);

  // Derived, not effect-synced: falls back to the first declared store
  // whenever `store` is unset or points at a store that no longer exists
  // (e.g. deleted out from under the panel while it's open).
  const activeStore = store && stores.some((s) => s.name === store) ? store : (stores[0]?.name ?? '');

  // Resets belong with the interaction that invalidates them, not a
  // separate effect watching state this component itself owns.
  const disarm = () => {
    setArmed(false);
    setRevokedMessage(null);
  };

  const refresh = useCallback(async () => {
    if (!open || !activeStore) return;
    try {
      const result = await api.listSessions({ store: activeStore, subject: subject || undefined, limit: 50 });
      setPage(result);
      setHeadless(false);
    } catch (e) {
      const parsed = parseApiError(e);
      if (parsed.status === 501) setHeadless(true);
      else if (parsed.status === 404) onError('Unknown store', activeStore);
      else onError('Failed to load sessions', parsed.error);
    }
  }, [open, activeStore, subject, onError]);

  // Load on open, and again whenever the store or applied subject filter
  // changes (both flow through refresh's own dependencies). Mirrors
  // DebugPanel's identical load-on-open effect; the setState calls inside
  // `refresh` all land after an `await`, not synchronously in this effect —
  // the compiler-based `set-state-in-effect` rule flags the fire-and-forget
  // call anyway (reproducible even on DebugPanel's own pattern in isolation,
  // so this is a rule false-positive rather than a real anti-pattern here).
  useEffect(() => {
    if (!open || !activeStore) return;
    // eslint-disable-next-line react-hooks/set-state-in-effect
    refresh();
  }, [open, activeStore, refresh]);

  const applyFilter = () => {
    setSubject(subjectInput.trim());
    disarm();
  };

  const loadMore = async () => {
    if (!activeStore || !page?.next_cursor) return;
    try {
      const result = await api.listSessions({
        store: activeStore,
        subject: subject || undefined,
        limit: 50,
        cursor: page.next_cursor,
      });
      setPage((prev) =>
        prev ? { sessions: [...prev.sessions, ...result.sessions], next_cursor: result.next_cursor } : result,
      );
    } catch (e) {
      onError('Failed to load more sessions', parseApiError(e).error);
    }
  };

  const revoke = async (id: string) => {
    try {
      await api.deleteSession(activeStore, id);
      await refresh();
    } catch (e) {
      onError('Failed to revoke session', parseApiError(e).error);
    }
  };

  const handleRevokeAll = async () => {
    if (!armed) {
      setArmed(true);
      return;
    }
    try {
      const result = await api.deleteSessionsBySubject(activeStore, subject);
      setRevokedMessage(`Revoked ${result.revoked} sessions`);
      setArmed(false);
      await refresh();
    } catch (e) {
      setArmed(false);
      onError('Failed to revoke sessions', parseApiError(e).error);
    }
  };

  const noStores = stores.length === 0;
  const sessions = page?.sessions ?? [];

  return (
    <Dialog
      open={open}
      title="Sessions"
      width={640}
      onClose={onClose}
      footer={
        <>
          {revokedMessage && !headless && !noStores && (
            <span
              style={{ fontSize: 'var(--text-xs)', color: 'var(--text-secondary)', marginRight: 'auto' }}
            >
              {revokedMessage}
            </span>
          )}
          {!headless && !noStores && (
            <DialogButton variant="danger" disabled={!subject} onClick={handleRevokeAll}>
              {armed ? `Confirm revoke all for "${subject}"` : 'Revoke all for subject…'}
            </DialogButton>
          )}
          <DialogButton variant="ghost" onClick={onClose}>
            Close
          </DialogButton>
        </>
      }
    >
      {headless ? (
        <HeadlessNotice />
      ) : noStores ? (
        <p style={{ fontSize: 'var(--text-sm)', color: 'var(--text-secondary)', margin: 0 }}>
          No stores declared — create one in the sidebar first.
        </p>
      ) : (
        <>
          <div className="flex items-center" style={{ gap: 8, marginBottom: 12 }}>
            <select
              aria-label="Session store"
              value={activeStore}
              onChange={(e) => {
                setStore(e.target.value);
                disarm();
              }}
              style={{ ...inputStyle, appearance: 'auto' }}
            >
              {stores.map((s) => (
                <option key={s.name} value={s.name}>
                  {s.name}
                </option>
              ))}
            </select>
            <input
              aria-label="Filter by subject"
              placeholder="subject"
              value={subjectInput}
              onChange={(e) => setSubjectInput(e.target.value)}
              onKeyDown={(e) => {
                if (e.key === 'Enter') applyFilter();
              }}
              style={{ ...inputStyle, flex: 1 }}
            />
            <DialogButton variant="ghost" onClick={applyFilter}>
              Apply
            </DialogButton>
            <DialogButton variant="ghost" onClick={refresh}>
              Refresh
            </DialogButton>
          </div>

          <div
            style={{
              maxHeight: '52vh',
              overflowY: 'auto',
              display: 'flex',
              flexDirection: 'column',
              gap: 4,
            }}
          >
            {sessions.length === 0 && (
              <p style={{ fontSize: 'var(--text-xs)', color: 'var(--text-muted)' }}>
                {subject ? 'No sessions match this subject.' : 'No sessions in this store.'}
              </p>
            )}
            {sessions.map((s) => (
              <div
                key={s.id}
                className="flex items-center justify-between"
                style={{
                  padding: '8px 10px',
                  borderRadius: 'var(--radius-sm)',
                  background: 'var(--surface-raised)',
                  border: '1px solid var(--border-subtle)',
                }}
              >
                <div className="min-w-0">
                  <div
                    className="truncate"
                    style={{ fontFamily: 'var(--font-mono)', fontSize: 'var(--text-xs)', color: 'var(--text-primary)' }}
                  >
                    {s.subject || '(no subject)'} · {s.plugin}
                  </div>
                  <div style={{ fontSize: 'var(--text-2xs)', color: 'var(--text-muted)', marginTop: 2 }}>
                    {s.policy && `${s.policy} · `}created {formatUnixTime(s.created_at)} · expires{' '}
                    {formatUnixTime(s.expires_at)}
                  </div>
                </div>
                <button
                  onClick={() => revoke(s.id)}
                  aria-label={`Revoke session ${s.id}`}
                  className="flex items-center justify-center rounded transition-all"
                  style={{ width: 24, height: 24, flexShrink: 0, color: 'var(--error)' }}
                >
                  <X size={14} />
                </button>
              </div>
            ))}
          </div>

          {page?.next_cursor && (
            <div style={{ marginTop: 10 }}>
              <DialogButton variant="ghost" onClick={loadMore}>
                Load more
              </DialogButton>
            </div>
          )}
        </>
      )}
    </Dialog>
  );
}
