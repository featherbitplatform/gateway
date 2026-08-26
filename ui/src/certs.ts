/**
 * Pure helpers for the Certificates panel (kept out of the component file so
 * react-refresh keeps working and vitest can import them without a DOM).
 *
 * @module certs
 */

export type ExpiryTone = 'none' | 'ok' | 'warn' | 'danger';

const DAY = 86_400;

/** Colour band for a certificate expiry: amber under 30 days, red under 7 (or expired). */
export function expiryTone(notAfter: number, nowSecs: number): ExpiryTone {
  if (!notAfter) return 'none';
  const left = notAfter - nowSecs;
  if (left < 7 * DAY) return 'danger';
  if (left < 30 * DAY) return 'warn';
  return 'ok';
}

/** `in 45d` / `in 5h` / `in 12m` / `in <1m` / `expired` / `—` (unset). */
export function formatExpiresIn(notAfter: number, nowSecs: number): string {
  if (!notAfter) return '—';
  const left = notAfter - nowSecs;
  if (left <= 0) return 'expired';
  if (left >= DAY) return `in ${Math.floor(left / DAY)}d`;
  if (left >= 3600) return `in ${Math.floor(left / 3600)}h`;
  if (left >= 60) return `in ${Math.floor(left / 60)}m`;
  return 'in <1m';
}
