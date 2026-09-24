import { describe, expect, it } from 'vitest';
import { isEnvPlaceholder, numberFieldText, parseNumberField } from './numberField';

describe('parseNumberField', () => {
  it('stores numbers as numbers', () => {
    expect(parseNumberField('8080')).toEqual({ ok: true, value: 8080 });
    expect(parseNumberField(' 0.25 ')).toEqual({ ok: true, value: 0.25 });
    expect(parseNumberField('-1')).toEqual({ ok: true, value: -1 });
    expect(parseNumberField('1e3')).toEqual({ ok: true, value: 1000 });
  });

  it('stores a whole env placeholder as a string', () => {
    expect(parseNumberField('${BACKEND_PORT}')).toEqual({ ok: true, value: '${BACKEND_PORT}' });
    expect(parseNumberField('${BACKEND_PORT:-3000}')).toEqual({ ok: true, value: '${BACKEND_PORT:-3000}' });
  });

  it('clears the key when empty', () => {
    expect(parseNumberField('')).toEqual({ ok: true, value: undefined });
    expect(parseNumberField('   ')).toEqual({ ok: true, value: undefined });
  });

  it('stores nothing for partial or mixed input', () => {
    for (const bad of ['${BACK', '${BACKEND_PORT}0', 'port', '12abc', '0x10', '${1BAD}', '${A}:${B}', 'Infinity']) {
      expect(parseNumberField(bad), bad).toEqual({ ok: false });
    }
  });
});

describe('numberFieldText / isEnvPlaceholder', () => {
  it('shows numbers and placeholders as typed, anything else as empty', () => {
    expect(numberFieldText(3000)).toBe('3000');
    expect(numberFieldText('${PORT}')).toBe('${PORT}');
    expect(numberFieldText(undefined)).toBe('');
    expect(numberFieldText(NaN)).toBe('');
  });

  it('matches only a single whole placeholder', () => {
    expect(isEnvPlaceholder('${PORT:-80}')).toBe(true);
    expect(isEnvPlaceholder('x${PORT}')).toBe(false);
  });
});
