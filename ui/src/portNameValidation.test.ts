import { describe, expect, it } from 'vitest';
import { validatePortName } from './portNameValidation';

describe('validatePortName', () => {
  it('accepts a fresh kebab name', () => {
    expect(validatePortName('denied', ['input', 'output', 'error'])).toBeNull();
  });
  it('rejects reserved ids', () => {
    for (const r of ['input', 'error', 'in', 'out', 'success']) {
      expect(validatePortName(r, [])).toMatch(/reserved/);
    }
  });
  it('rejects empty, slash, and duplicate ids', () => {
    expect(validatePortName('', [])).toMatch(/required/i);
    expect(validatePortName('a/b', [])).toMatch(/'\/'/);
    expect(validatePortName('denied', ['denied'])).toMatch(/already exists/);
  });
  it('allows keeping your own id on rename', () => {
    expect(validatePortName('denied', ['denied', 'output'], 'denied')).toBeNull();
  });
});
