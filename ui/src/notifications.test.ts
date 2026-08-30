import { describe, expect, it } from 'vitest';
import {
  MAX_ENTRIES,
  STORAGE_KEY,
  appendEntry,
  detailsFromMessage,
  loadEntries,
  makeEntry,
  saveEntries,
  type NotificationEntry,
} from './notifications';

/** Minimal in-memory Storage double (only what the store touches). */
function memoryStorage(seed: Record<string, string> = {}): Storage {
  const data = new Map(Object.entries(seed));
  return {
    getItem: (k) => data.get(k) ?? null,
    setItem: (k, v) => void data.set(k, v),
    removeItem: (k) => void data.delete(k),
    clear: () => data.clear(),
    key: (i) => [...data.keys()][i] ?? null,
    get length() {
      return data.size;
    },
  };
}

function entry(n: number, tone: NotificationEntry['tone'] = 'success'): NotificationEntry {
  return makeEntry({ tone, title: `t${n}` }, n, `id-${n}`);
}

describe('makeEntry', () => {
  it('stamps the id and timestamp and keeps optional message/details', () => {
    const e = makeEntry(
      { tone: 'error', title: 'Failed to save policy', message: '400: {"error":"x"}', details: '{"error":"x"}' },
      1700000000000,
      'abc',
    );
    expect(e).toEqual({
      id: 'abc',
      ts: 1700000000000,
      tone: 'error',
      title: 'Failed to save policy',
      message: '400: {"error":"x"}',
      details: '{"error":"x"}',
    });
  });
});

describe('appendEntry', () => {
  it('prepends so the newest entry is first', () => {
    const list = appendEntry(appendEntry([], entry(1)), entry(2));
    expect(list.map((e) => e.id)).toEqual(['id-2', 'id-1']);
  });

  it('caps the log at MAX_ENTRIES, dropping the oldest', () => {
    let list: NotificationEntry[] = [];
    for (let i = 0; i < MAX_ENTRIES + 5; i++) list = appendEntry(list, entry(i));
    expect(list).toHaveLength(MAX_ENTRIES);
    expect(list[0].id).toBe(`id-${MAX_ENTRIES + 4}`);
    expect(list[list.length - 1].id).toBe('id-5');
  });
});

describe('detailsFromMessage', () => {
  it('pretty-prints a JSON error body behind the "<status>: <body>" toast message', () => {
    const msg = '400: {"error":"policy \'p\': output port \'redirect\' must be wired"}';
    expect(detailsFromMessage(msg)).toBe(
      'HTTP 400\n{\n  "error": "policy \'p\': output port \'redirect\' must be wired"\n}',
    );
  });

  it('tolerates the "Error: " prefix a stringified Error carries', () => {
    // App builds toast messages as `${e}` from the thrown Error, so the api
    // client's "<status>: <body>" arrives as "Error: <status>: <body>".
    expect(detailsFromMessage('Error: 400: {"error":"x"}')).toBe('HTTP 400\n{\n  "error": "x"\n}');
  });

  it('keeps a non-JSON body verbatim under its status line', () => {
    expect(detailsFromMessage('502: upstream said no')).toBe('HTTP 502\nupstream said no');
  });

  it('returns undefined when the message carries no status prefix or is absent', () => {
    expect(detailsFromMessage('Copied to clipboard')).toBeUndefined();
    expect(detailsFromMessage(undefined)).toBeUndefined();
  });
});

describe('loadEntries / saveEntries', () => {
  it('round-trips through storage under STORAGE_KEY', () => {
    const storage = memoryStorage();
    const list = [entry(2, 'error'), entry(1)];
    saveEntries(list, storage);
    expect(storage.getItem(STORAGE_KEY)).not.toBeNull();
    expect(loadEntries(storage)).toEqual(list);
  });

  it('returns an empty log when nothing is stored', () => {
    expect(loadEntries(memoryStorage())).toEqual([]);
  });

  it('returns an empty log on corrupt JSON and drops malformed items', () => {
    expect(loadEntries(memoryStorage({ [STORAGE_KEY]: '{not json' }))).toEqual([]);
    const mixed = JSON.stringify([entry(1), { id: 'x' }, 'nope', null, entry(2, 'warning')]);
    expect(loadEntries(memoryStorage({ [STORAGE_KEY]: mixed })).map((e) => e.id)).toEqual(['id-1', 'id-2']);
  });

  it('survives a storage that throws or is unavailable', () => {
    const throwing = {
      getItem: () => {
        throw new Error('blocked');
      },
      setItem: () => {
        throw new Error('blocked');
      },
    } as unknown as Storage;
    expect(loadEntries(throwing)).toEqual([]);
    expect(() => saveEntries([entry(1)], throwing)).not.toThrow();
    expect(loadEntries(null)).toEqual([]);
    expect(() => saveEntries([entry(1)], null)).not.toThrow();
  });
});
