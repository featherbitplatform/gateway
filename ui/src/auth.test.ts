import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import {
  authHeaders,
  basicAuth,
  CREDENTIALS_KEY,
  getCredentials,
  getUsername,
  onSignedOut,
  onUnauthorized,
  reportUnauthorized,
  resetForTests,
  saveCredentials,
  signOut,
  UI_CLIENT_HEADER,
} from './auth';

function memoryStorage(): Storage {
  const m = new Map<string, string>();
  return {
    getItem: (k) => m.get(k) ?? null,
    setItem: (k, v) => void m.set(k, v),
    removeItem: (k) => void m.delete(k),
    clear: () => m.clear(),
    key: (i) => [...m.keys()][i] ?? null,
    get length() {
      return m.size;
    },
  };
}

let session: Storage;
let local: Storage;

beforeEach(() => {
  session = memoryStorage();
  local = memoryStorage();
  vi.stubGlobal('window', { sessionStorage: session, localStorage: local });
  resetForTests();
});
afterEach(() => vi.unstubAllGlobals());

describe('credentials', () => {
  it('has no fallback: signed out sends only the UI marker', () => {
    expect(getCredentials()).toBeNull();
    expect(authHeaders()).toEqual({ [UI_CLIENT_HEADER]: 'ui' });
  });

  it('keeps credentials for the tab only unless remembered', () => {
    saveCredentials('ops', 's3cret', false);
    expect(session.getItem(CREDENTIALS_KEY)).toBe('ops:s3cret');
    expect(local.getItem(CREDENTIALS_KEY)).toBeNull();

    saveCredentials('ops', 's3cret', true);
    expect(local.getItem(CREDENTIALS_KEY)).toBe('ops:s3cret');
    expect(session.getItem(CREDENTIALS_KEY)).toBeNull();
    expect(getUsername()).toBe('ops');
    expect(authHeaders().Authorization).toBe(`Basic ${btoa('ops:s3cret')}`);
  });

  it('reads remembered credentials after a reload', () => {
    local.setItem(CREDENTIALS_KEY, 'a:b');
    expect(getCredentials()).toBe('a:b');
  });

  it('encodes non-Latin-1 passwords as UTF-8 instead of throwing', () => {
    expect(basicAuth('ops:pässwörd€')).toBe('Basic b3BzOnDDpHNzd8O2cmTigqw=');
  });

  it('sign out clears both storages and notifies', () => {
    saveCredentials('a', 'b', true);
    session.setItem(CREDENTIALS_KEY, 'stale:x');
    const out = vi.fn();
    onSignedOut(out);
    signOut();
    expect(getCredentials()).toBeNull();
    expect(local.getItem(CREDENTIALS_KEY)).toBeNull();
    expect(session.getItem(CREDENTIALS_KEY)).toBeNull();
    expect(out).toHaveBeenCalledOnce();
  });

  it('reports 401s to subscribers until they unsubscribe', () => {
    const l = vi.fn();
    const off = onUnauthorized(l);
    reportUnauthorized();
    off();
    reportUnauthorized();
    expect(l).toHaveBeenCalledOnce();
  });
});
