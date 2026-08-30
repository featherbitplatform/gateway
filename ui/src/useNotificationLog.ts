/**
 * React state for the persistent notification log (see `notifications.ts`).
 *
 * Seeds from localStorage, writes back on every change, and tracks an
 * "unread" count — error entries raised since the panel was last opened —
 * for the sidebar bell's badge. Successes and warnings are logged but never
 * count as unread: they are a timeline, not something to act on.
 *
 * @module useNotificationLog
 */
import { useCallback, useEffect, useMemo, useState } from 'react';
import {
  appendEntry,
  loadEntries,
  makeEntry,
  saveEntries,
  type NotificationEntry,
  type NotificationInput,
} from './notifications';

/** localStorage key holding the epoch-ms timestamp of the last panel open. */
const SEEN_KEY = 'featherbit.notifications.seen';

/** `window.localStorage`, or null where the accessor itself throws (blocked storage). */
function safeStorage(): Storage | null {
  try {
    return window.localStorage;
  } catch {
    return null;
  }
}

function loadSeen(storage: Storage | null): number {
  try {
    const n = Number(storage?.getItem(SEEN_KEY) ?? 0);
    return Number.isFinite(n) ? n : 0;
  } catch {
    return 0;
  }
}

function newId(): string {
  const c = globalThis.crypto as Crypto | undefined;
  if (c && typeof c.randomUUID === 'function') return c.randomUUID();
  return `${Date.now().toString(36)}-${Math.random().toString(36).slice(2, 10)}`;
}

/** What the hook exposes. `notify`, `clear` and `markSeen` are referentially stable. */
export interface NotificationLog {
  /** Newest first. */
  entries: NotificationEntry[];
  /** Error entries newer than the last panel open. */
  unread: number;
  /** Appends an entry (stamped with id + now) and returns it. */
  notify: (input: NotificationInput) => NotificationEntry;
  /** Empties the log, including the persisted copy. */
  clear: () => void;
  /** Marks everything logged so far as seen (resets `unread`). */
  markSeen: () => void;
}

/**
 * Owns the notification log for the app. Call once (in App) and thread
 * `entries`/`unread`/`notify` down; the setters are stable so they can sit
 * in memoized callbacks (e.g. the panels' `onError`) without churning them.
 */
export function useNotificationLog(): NotificationLog {
  const storage = safeStorage();
  const [entries, setEntries] = useState<NotificationEntry[]>(() => loadEntries(storage));
  const [seen, setSeen] = useState<number>(() => loadSeen(storage));

  useEffect(() => {
    saveEntries(entries, safeStorage());
  }, [entries]);

  useEffect(() => {
    try {
      safeStorage()?.setItem(SEEN_KEY, String(seen));
    } catch {
      // Blocked storage: the badge just won't survive a reload.
    }
  }, [seen]);

  const notify = useCallback((input: NotificationInput): NotificationEntry => {
    const entry = makeEntry(input, Date.now(), newId());
    setEntries((list) => appendEntry(list, entry));
    return entry;
  }, []);

  const clear = useCallback(() => setEntries([]), []);
  const markSeen = useCallback(() => setSeen(Date.now()), []);

  // Errors only: a warning is a heads-up that usually precedes the error it
  // warns about (e.g. "Unwired ports" → the server's rejection), so counting
  // both would show "2" for one failed save.
  const unread = useMemo(
    () => entries.filter((e) => e.tone === 'error' && e.ts > seen).length,
    [entries, seen]
  );

  return { entries, unread, notify, clear, markSeen };
}
