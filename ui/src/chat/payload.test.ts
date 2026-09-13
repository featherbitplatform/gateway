import { describe, expect, it } from 'vitest';
import { LIFTED, formatPayload, isEmptyPayload } from './payload';

describe('isEmptyPayload', () => {
  it('is true only for nothing worth showing', () => {
    for (const raw of ['', '   ', '{}', '[]', 'null', '{ }']) expect(isEmptyPayload(raw), raw).toBe(true);
    for (const raw of ['{"a":1}', '[1]', '"text"', 'not json']) expect(isEmptyPayload(raw), raw).toBe(false);
  });
});

describe('formatPayload', () => {
  it('pretty-prints a plain object as one JSON block', () => {
    const [main, ...rest] = formatPayload('{"name":"api","dry_run":false}');
    expect(rest).toEqual([]);
    expect(main.lang).toBe('json');
    expect(main.text).toBe('{\n  "name": "api",\n  "dry_run": false\n}');
  });

  it('unwraps a nested JSON string instead of showing escaped quotes', () => {
    const raw = JSON.stringify({ name: 'p', definition: JSON.stringify({ nodes: [{ id: 'l', type: 'listener' }] }) });
    const [main] = formatPayload(raw);
    expect(main.text).not.toContain('\\"');
    expect(JSON.parse(main.text)).toEqual({ name: 'p', definition: { nodes: [{ id: 'l', type: 'listener' }] } });
  });

  it('unwraps a payload that was JSON-encoded twice', () => {
    const inner = JSON.stringify({ policies: ['a'] });
    const [main] = formatPayload(JSON.stringify(inner));
    expect(main.lang).toBe('json');
    expect(JSON.parse(main.text)).toEqual({ policies: ['a'] });
  });

  it('lifts multi-line strings into their own labelled block', () => {
    const yaml = 'nodes:\n  - id: l\n    type: listener\n';
    const blocks = formatPayload(JSON.stringify({ name: 'p', definition: yaml }));
    expect(blocks).toHaveLength(2);
    expect(JSON.parse(blocks[0].text)).toEqual({ name: 'p', definition: LIFTED });
    expect(blocks[1]).toEqual({ label: 'definition', text: yaml, lang: 'text' });
  });

  it('keeps single-line strings inline', () => {
    const [main, ...rest] = formatPayload('{"policy":"hello-policy"}');
    expect(rest).toEqual([]);
    expect(main.text).toContain('"hello-policy"');
  });

  it('renders a quoted scalar and unparseable text as plain text', () => {
    expect(formatPayload('"just words"')).toEqual([{ text: 'just words', lang: 'text' }]);
    expect(formatPayload('routes:\n  - name: x')).toEqual([{ text: 'routes:\n  - name: x', lang: 'text' }]);
    expect(formatPayload('42')).toEqual([{ text: '42', lang: 'text' }]);
  });

  it('returns no blocks for an empty payload', () => {
    expect(formatPayload('')).toEqual([]);
    expect(formatPayload('   ')).toEqual([]);
  });

  it('leaves values that merely look like text alone', () => {
    const [main] = formatPayload('{"hint":"use {\\"a\\": 1} as the shape"}');
    expect(JSON.parse(main.text).hint).toBe('use {"a": 1} as the shape');
  });
});
