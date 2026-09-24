import { describe, expect, it } from 'vitest';
import { moveBy, moveTo } from './routeOrder';

const abc = ['a', 'b', 'c'];

describe('moveTo', () => {
  it('moves an item up or down to the insertion gap', () => {
    expect(moveTo(abc, 2, 0)).toEqual(['c', 'a', 'b']);
    expect(moveTo(abc, 0, 3)).toEqual(['b', 'c', 'a']);
    expect(moveTo(abc, 0, 2)).toEqual(['b', 'a', 'c']);
  });

  it('returns null when the item would not move or the indices are out of range', () => {
    expect(moveTo(abc, 1, 1)).toBeNull();
    expect(moveTo(abc, 1, 2)).toBeNull();
    expect(moveTo(abc, 3, 0)).toBeNull();
    expect(moveTo(abc, 0, 4)).toBeNull();
  });
});

describe('moveBy', () => {
  it('swaps with the neighbour and stops at the ends', () => {
    expect(moveBy(abc, 1, -1)).toEqual(['b', 'a', 'c']);
    expect(moveBy(abc, 1, 1)).toEqual(['a', 'c', 'b']);
    expect(moveBy(abc, 0, -1)).toBeNull();
    expect(moveBy(abc, 2, 1)).toBeNull();
  });
});
