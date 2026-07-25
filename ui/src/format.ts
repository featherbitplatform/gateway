/**
 * Shared display formatters.
 *
 * Lives outside the component files so components only export components
 * (react-refresh requires it for hot reload to work).
 *
 * @module format
 */

/** Render a microsecond duration as `123µs` / `1.2ms`. */
export function formatDuration(us: number): string {
  if (us < 1000) return `${us}µs`;
  return `${(us / 1000).toFixed(1)}ms`;
}
