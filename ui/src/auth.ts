/**
 * Admin API credentials held by the browser, and the sign-in state around them.
 *
 * Credentials are a `user:pass` pair sent as HTTP Basic on every Admin API
 * call. They live in `sessionStorage` (gone when the tab closes) unless the
 * operator ticks "Remember me", which puts them in `localStorage` instead.
 * There is no built-in fallback: with nothing stored, requests go out
 * without an `Authorization` header and the gateway answers 401, which is
 * what brings up the sign-in screen.
 *
 * Every request is also marked with {@link UI_CLIENT_HEADER}, so the
 * gateway's 401 carries no `WWW-Authenticate` challenge and the browser
 * never pops its native Basic Auth dialog over the sign-in screen.
 *
 * @module auth
 */

/** Storage key for the `user:pass` pair (same key in both storages). */
export const CREDENTIALS_KEY = 'gw_credentials';

/** Mirrors `UI_CLIENT_HEADER` in src/admin/auth.rs. */
export const UI_CLIENT_HEADER = 'X-Featherbit-Client';

function storages(): Storage[] {
  const out: Storage[] = [];
  try {
    out.push(window.sessionStorage);
  } catch {
    /* blocked */
  }
  try {
    out.push(window.localStorage);
  } catch {
    /* blocked */
  }
  return out;
}

// In-memory copy: used when storage is blocked, and the fastest read.
let current: string | null = null;
let loaded = false;

/** The stored `user:pass`, or null when signed out. */
export function getCredentials(): string | null {
  if (!loaded) {
    loaded = true;
    for (const s of storages()) {
      try {
        const v = s.getItem(CREDENTIALS_KEY);
        if (v) {
          current = v;
          break;
        }
      } catch {
        /* blocked */
      }
    }
  }
  return current;
}

/** The username half of the stored credentials, for display. */
export function getUsername(): string | null {
  const c = getCredentials();
  return c === null ? null : c.slice(0, c.indexOf(':') === -1 ? c.length : c.indexOf(':'));
}

/** Stores credentials for this tab only, or across sessions with `remember`. */
export function saveCredentials(username: string, password: string, remember: boolean): void {
  const value = `${username}:${password}`;
  current = value;
  loaded = true;
  const [session, local] = [safe(() => window.sessionStorage), safe(() => window.localStorage)];
  const keep = remember ? local : session;
  const drop = remember ? session : local;
  try {
    drop?.removeItem(CREDENTIALS_KEY);
  } catch {
    /* blocked */
  }
  try {
    keep?.setItem(CREDENTIALS_KEY, value);
  } catch {
    /* blocked: the in-memory copy still works until a reload */
  }
}

/** Forgets the credentials everywhere. */
export function clearCredentials(): void {
  current = null;
  loaded = true;
  for (const s of storages()) {
    try {
      s.removeItem(CREDENTIALS_KEY);
    } catch {
      /* blocked */
    }
  }
}

function safe<T>(f: () => T): T | null {
  try {
    return f();
  } catch {
    return null;
  }
}

/** Base64 of the UTF-8 bytes (plain `btoa` throws on non-Latin-1 input). */
function base64Utf8(text: string): string {
  let bin = '';
  for (const b of new TextEncoder().encode(text)) bin += String.fromCharCode(b);
  return btoa(bin);
}

/** `Authorization: Basic …` for a `user:pass` pair. */
export function basicAuth(credentials: string): string {
  return `Basic ${base64Utf8(credentials)}`;
}

/** Headers every Admin API call carries: the UI marker, plus Basic auth when signed in. */
export function authHeaders(credentials: string | null = getCredentials()): Record<string, string> {
  const h: Record<string, string> = { [UI_CLIENT_HEADER]: 'ui' };
  if (credentials !== null) h.Authorization = basicAuth(credentials);
  return h;
}

// Sign-in state listeners, registered by the LoginGate.
type Listener = () => void;
const unauthorizedListeners = new Set<Listener>();
const signedOutListeners = new Set<Listener>();
const signedInListeners = new Set<Listener>();

function on(set: Set<Listener>, l: Listener): () => void {
  set.add(l);
  return () => {
    set.delete(l);
  };
}

/** Called when any Admin API call comes back 401. Returns an unsubscribe. */
export const onUnauthorized = (l: Listener) => on(unauthorizedListeners, l);
/** Called after {@link signOut}. Returns an unsubscribe. */
export const onSignedOut = (l: Listener) => on(signedOutListeners, l);
/** Called after a successful sign-in, so a still-mounted editor can re-fetch. Returns an unsubscribe. */
export const onSignedIn = (l: Listener) => on(signedInListeners, l);

/** Reports a 401 from the Admin API. */
export function reportUnauthorized(): void {
  for (const l of unauthorizedListeners) l();
}

/** Reports a successful sign-in. */
export function reportSignedIn(): void {
  for (const l of signedInListeners) l();
}

/** Forgets the credentials and returns to the sign-in screen. */
export function signOut(): void {
  clearCredentials();
  for (const l of signedOutListeners) l();
}

/** Test hook: drops the in-memory copy so the next read goes back to storage. */
export function resetForTests(): void {
  current = null;
  loaded = false;
  unauthorizedListeners.clear();
  signedOutListeners.clear();
  signedInListeners.clear();
}
