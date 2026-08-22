import { describe, expect, it } from 'vitest';
import { formatUnixTime } from './format';

describe('formatUnixTime', () => {
  it('renders a unix-seconds timestamp as a locale date-time string', () => {
    // Fixed instant; assert on the parts that are locale-stable.
    const s = formatUnixTime(1_787_356_800); // 2026-08-22T00:00:00Z
    expect(s).toMatch(/2026/);
  });
  it('renders 0 as a dash (unset)', () => {
    expect(formatUnixTime(0)).toBe('—');
  });
});
