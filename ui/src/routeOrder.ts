/**
 * Route priority helpers. The gateway matches routes top to bottom and the
 * first match wins, so the sidebar's order *is* the priority; reordering
 * sends the full new name list to `PUT /api/routes`.
 *
 * @module routeOrder
 */

/**
 * Moves the item at `from` so it lands before the item currently at `insertAt`
 * (`insertAt === names.length` = the end). Returns null when nothing moves, so
 * callers can skip a no-op API call.
 */
export function moveTo(names: readonly string[], from: number, insertAt: number): string[] | null {
  if (from < 0 || from >= names.length || insertAt < 0 || insertAt > names.length) return null;
  // Dropping onto the gap just above or below itself leaves it in place.
  if (insertAt === from || insertAt === from + 1) return null;
  const next = [...names];
  const [item] = next.splice(from, 1);
  next.splice(insertAt > from ? insertAt - 1 : insertAt, 0, item);
  return next;
}

/** Swaps the item at `index` with its neighbour one step up (`-1`) or down (`+1`); null at the ends. */
export function moveBy(names: readonly string[], index: number, delta: -1 | 1): string[] | null {
  return moveTo(names, index, delta === -1 ? index - 1 : index + 2);
}
