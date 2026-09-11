/**
 * Ranks provider model ids against what the user has typed, for the chat
 * settings combobox. Pure string scoring — no fetching.
 *
 * @module chat/modelMatch
 */

/** 100 exact · 80 prefix · 60 substring · 30 in-order subsequence · 0 none; blank query = 1. */
export function scoreModel(query: string, id: string): number {
  const q = query.trim().toLowerCase();
  if (q === '') return 1;
  const s = id.toLowerCase();
  if (s === q) return 100;
  if (s.startsWith(q)) return 80;
  if (s.includes(q)) return 60;
  let i = 0;
  for (const ch of s) {
    if (ch === q[i]) i += 1;
    if (i === q.length) return 30;
  }
  return 0;
}

/** Best matches first (score desc, id asc), non-matches dropped, at most `limit`. */
export function rankModels(query: string, ids: string[], limit = 8): string[] {
  return ids
    .map((id) => ({ id, score: scoreModel(query, id) }))
    .filter((m) => m.score > 0)
    .sort((a, b) => b.score - a.score || a.id.localeCompare(b.id))
    .slice(0, limit)
    .map((m) => m.id);
}
