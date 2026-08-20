import { describe, expect, it } from 'vitest';
import { validatePortName } from './portNameValidation';

describe('validatePortName', () => {
  it('accepts a fresh kebab name', () => {
    expect(validatePortName('denied', ['input', 'output', 'error'], 'output')).toBeNull();
  });
  it('rejects reserved ids', () => {
    for (const r of ['input', 'error', 'in', 'out', 'success']) {
      expect(validatePortName(r, [], 'output')).toMatch(/reserved/);
    }
  });
  it('rejects empty, slash, and duplicate ids', () => {
    expect(validatePortName('', [], 'output')).toMatch(/required/i);
    expect(validatePortName('a/b', [], 'output')).toMatch(/'\/'/);
    expect(validatePortName('denied', ['denied'], 'output')).toMatch(/already exists/);
  });
  it('allows keeping your own id on rename', () => {
    expect(validatePortName('denied', ['denied', 'output'], 'output', 'denied')).toBeNull();
  });
  it('reserves per kind: error boundaries may not shadow output specials and vice versa', () => {
    expect(validatePortName('output', [], 'error')).toMatch(/reserved/);
    expect(validatePortName('error', [], 'output')).toMatch(/reserved/);
    // each kind's own special id is legal (dupes are caught by takenIds)
    expect(validatePortName('error', [], 'error')).toBeNull();
    expect(validatePortName('output', [], 'output')).toBeNull();
  });
});
