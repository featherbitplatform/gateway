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
  it('draws the 30-day and 7-day bands on exact boundaries', () => {
    expect(expiryTone(NOW + 30 * DAY, NOW)).toBe('ok');
    expect(expiryTone(NOW + 30 * DAY - 1, NOW)).toBe('warn');
    expect(expiryTone(NOW + 7 * DAY, NOW)).toBe('warn');
    expect(expiryTone(NOW + 7 * DAY - 1, NOW)).toBe('danger');
    expect(expiryTone(NOW, NOW)).toBe('danger');
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
  it('rounds down at the day/hour/minute unit boundaries', () => {
    expect(formatExpiresIn(NOW, NOW)).toBe('expired');
    expect(formatExpiresIn(NOW + DAY, NOW)).toBe('in 1d');
    expect(formatExpiresIn(NOW + DAY - 1, NOW)).toBe('in 23h');
    expect(formatExpiresIn(NOW + 3600, NOW)).toBe('in 1h');
    expect(formatExpiresIn(NOW + 3599, NOW)).toBe('in 59m');
    expect(formatExpiresIn(NOW + 60, NOW)).toBe('in 1m');
    expect(formatExpiresIn(NOW + 59, NOW)).toBe('in <1m');
  });
});
