import { describe, expect, it } from 'vitest';
import { expiryTone, formatExpiresIn } from './certs';

const NOW = 1_800_000_000;
const DAY = 86_400;

describe('expiryTone', () => {
  it('is none for an unset expiry (placeholder)', () => {
    expect(expiryTone(0, NOW)).toBe('none');
  });
  it('is ok beyond 30 days, warn under 30, danger under 7 or expired', () => {
    expect(expiryTone(NOW + 60 * DAY, NOW)).toBe('ok');
    expect(expiryTone(NOW + 29 * DAY, NOW)).toBe('warn');
    expect(expiryTone(NOW + 6 * DAY, NOW)).toBe('danger');
    expect(expiryTone(NOW - 1, NOW)).toBe('danger');
  });
});

describe('formatExpiresIn', () => {
  it('renders the coarsest sensible unit', () => {
    expect(formatExpiresIn(0, NOW)).toBe('—');
    expect(formatExpiresIn(NOW - 5, NOW)).toBe('expired');
    expect(formatExpiresIn(NOW + 45 * DAY + 3600, NOW)).toBe('in 45d');
    expect(formatExpiresIn(NOW + 5 * 3600 + 90, NOW)).toBe('in 5h');
    expect(formatExpiresIn(NOW + 12 * 60 + 5, NOW)).toBe('in 12m');
    expect(formatExpiresIn(NOW + 40, NOW)).toBe('in <1m');
  });
});
