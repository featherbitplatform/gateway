/**
 * Persistent notification log behind the transient toasts.
 *
 * Every toast the admin UI shows (policy saved, save rejected, store pinged…)
 * is also appended here so an outcome that flashed by — typically a red
 * "Failed to save policy" during a busy moment — can be inspected afterwards
 * in the Notifications panel, with the full server response attached.
 *
 * Pure data layer: newest-first list, capped, (de)serialized to a `Storage`
 * (localStorage per browser). Every storage access is guarded — a private
 * window, cleared site data or a blocked accessor must degrade to an empty
 * log, never break the UI.
 *
 * @module notifications
 */

/** Visual tone, mirroring {@link ToastData.tone}. */
export type NotificationTone = 'success' | 'error' | 'warning';

/** One logged notification. */
export interface NotificationEntry {
  /** Unique id (used as React key and for "open panel on this entry"). */
  id: string;
  /** Epoch milliseconds when the notification was raised. */
  ts: number;
  tone: NotificationTone;
  /** Headline, e.g. "Failed to save policy". */
  title: string;
  /** Short detail line as shown on the toast (typically `"<status>: <body>"`). */
  message?: string;
  /** Full inspectable payload (e.g. the pretty-printed server error body). */
  details?: string;
}

/** What callers supply; id and timestamp are stamped by {@link makeEntry}. */
export type NotificationInput = Omit<NotificationEntry, 'id' | 'ts'>;

/** localStorage key holding the JSON-encoded log. */
export const STORAGE_KEY = 'featherbit.notifications';

/** Upper bound on retained entries; the oldest are dropped past it. */
export const MAX_ENTRIES = 200;

const TONES: readonly NotificationTone[] = ['success', 'error', 'warning'];

/**
 * Derives the inspectable payload from a toast's message line. The api
 * client throws `"<status>: <body>"`, which toasts show verbatim; here the
 * body is pretty-printed when it is JSON so the panel shows the server's
 * error structure legibly. Messages without a status prefix carry nothing
 * beyond themselves, so they yield `undefined` (the panel falls back to the
 * message).
 */
export function detailsFromMessage(message: string | undefined): string | undefined {
  if (!message) return undefined;
  // Optional "Error: " prefix: App formats toast messages as `${e}` from the
  // thrown Error, which stringifies with its name in front.
  const m = message.match(/^(?:Error: )?(\d{3}): ([\s\S]*)$/);
  if (!m) return undefined;
  const [, status, body] = m;
  let pretty = body;
  try {
    pretty = JSON.stringify(JSON.parse(body), null, 2);
  } catch {
    // Not JSON: keep the raw body.
  }
  return `HTTP ${status}\n${pretty}`;
}

/** Builds a complete entry from caller input plus the stamp values. */
export function makeEntry(input: NotificationInput, now: number, id: string): NotificationEntry {
  const entry: NotificationEntry = { id, ts: now, tone: input.tone, title: input.title };
  if (input.message !== undefined) entry.message = input.message;
  if (input.details !== undefined) entry.details = input.details;
  return entry;
}

/** Returns a new list with `entry` first, trimmed to {@link MAX_ENTRIES}. */
export function appendEntry(list: NotificationEntry[], entry: NotificationEntry): NotificationEntry[] {
  return [entry, ...list].slice(0, MAX_ENTRIES);
}

function isEntry(v: unknown): v is NotificationEntry {
  if (typeof v !== 'object' || v === null) return false;
  const o = v as Record<string, unknown>;
  return (
    typeof o.id === 'string' &&
    typeof o.ts === 'number' &&
    TONES.includes(o.tone as NotificationTone) &&
    typeof o.title === 'string' &&
    (o.message === undefined || typeof o.message === 'string') &&
    (o.details === undefined || typeof o.details === 'string')
  );
}

/**
 * Reads the log from `storage`. Anything unreadable — no storage, a throwing
 * accessor, corrupt JSON, a non-array — yields `[]`; malformed items inside
 * an otherwise valid array are skipped.
 */
export function loadEntries(storage: Storage | null | undefined): NotificationEntry[] {
  try {
    const raw = storage?.getItem(STORAGE_KEY);
    if (!raw) return [];
    const parsed: unknown = JSON.parse(raw);
    return Array.isArray(parsed) ? parsed.filter(isEntry) : [];
  } catch {
    return [];
  }
}

/** Writes the log to `storage`; a failing or missing storage is ignored. */
export function saveEntries(list: NotificationEntry[], storage: Storage | null | undefined): void {
  try {
    storage?.setItem(STORAGE_KEY, JSON.stringify(list));
  } catch {
    // Quota exceeded, private mode, blocked storage: the in-memory log still works.
  }
}
