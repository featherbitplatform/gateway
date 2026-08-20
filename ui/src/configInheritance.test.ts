import { describe, expect, it } from 'vitest';
import { applyEdit, classifyKey, displayValue, mergeEffective } from './configInheritance';

describe('displayValue', () => {
  it('prefers the local value over inherited and default', () => {
    expect(displayValue({ burst: 5 }, { burst: 10 }, 'burst', 1)).toBe(5);
  });

  it('falls back to the inherited value when no local key exists', () => {
    expect(displayValue({}, { burst: 10 }, 'burst', 1)).toBe(10);
  });

  it('falls back to the field default when neither layer has the key', () => {
    expect(displayValue({}, {}, 'burst', 1)).toBe(1);
  });

  it('returns undefined when no layer has the key and no default is given', () => {
    expect(displayValue({}, {}, 'burst')).toBeUndefined();
  });

  it('treats an explicit local null as present (it wins over inherited)', () => {
    expect(displayValue({ burst: null }, { burst: 10 }, 'burst', 1)).toBeNull();
  });
});

describe('classifyKey', () => {
  it('classifies a local key shadowing an inherited key as override', () => {
    expect(classifyKey({ burst: 5 }, { burst: 10 }, 'burst')).toBe('override');
  });

  it('classifies a local key absent from the inherited config as added', () => {
    expect(classifyKey({ burst: 5 }, {}, 'burst')).toBe('added');
  });

  it('classifies an inherited-only key as inherited', () => {
    expect(classifyKey({}, { burst: 10 }, 'burst')).toBe('inherited');
  });

  it('classifies a key in neither layer as default', () => {
    expect(classifyKey({}, {}, 'burst')).toBe('default');
  });
});

describe('applyEdit', () => {
  it('writes a value that differs from the inherited one into local config', () => {
    expect(applyEdit({}, { burst: 10 }, 'burst', 5)).toEqual({ burst: 5 });
  });

  it('drops the local key when the new value equals the inherited scalar', () => {
    expect(applyEdit({ burst: 5 }, { burst: 10 }, 'burst', 10)).toEqual({});
  });

  it('drops the local key when the new value deep-equals an inherited object', () => {
    const inherited = { headers: [{ name: 'X-A', value: '1' }] };
    const local = { headers: [{ name: 'X-B', value: '2' }] };
    expect(applyEdit(local, inherited, 'headers', [{ name: 'X-A', value: '1' }])).toEqual({});
  });

  it('keeps a value that differs from the inherited object', () => {
    const inherited = { headers: [{ name: 'X-A', value: '1' }] };
    const next = applyEdit({}, inherited, 'headers', [{ name: 'X-A', value: '2' }]);
    expect(next).toEqual({ headers: [{ name: 'X-A', value: '2' }] });
  });

  it('removes the key entirely when the new value is undefined', () => {
    expect(applyEdit({ burst: 5 }, {}, 'burst', undefined)).toEqual({});
  });

  it('keeps a value with no inherited counterpart (an added key)', () => {
    expect(applyEdit({}, {}, 'burst', 5)).toEqual({ burst: 5 });
  });

  it('does not treat null as equal to a missing inherited key', () => {
    expect(applyEdit({}, {}, 'burst', null)).toEqual({ burst: null });
  });

  it('preserves other local keys and does not mutate its inputs', () => {
    const local = { burst: 5 };
    const next = applyEdit(local, {}, 'rate', 100);
    expect(next).toEqual({ burst: 5, rate: 100 });
    expect(local).toEqual({ burst: 5 });
  });
});

describe('mergeEffective', () => {
  it('overlays local keys on top of inherited keys', () => {
    expect(mergeEffective({ burst: 10, rate: 100 }, { burst: 5 })).toEqual({
      burst: 5,
      rate: 100,
    });
  });

  it('lets an explicit local null win over an inherited value', () => {
    expect(mergeEffective({ burst: 10 }, { burst: null })).toEqual({ burst: null });
  });

  it('returns local as-is when there is nothing inherited', () => {
    expect(mergeEffective({}, { burst: 5 })).toEqual({ burst: 5 });
  });
});
