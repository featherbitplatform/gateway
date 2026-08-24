/**
 * Structured view over the Admin API client's stringly errors.
 *
 * `request<T>` throws `Error("<status>: <body>")`; this parses that shape so
 * panels can special-case 409 in_use (referrer lists), 501 (headless build),
 * and 502 (store outage) instead of dumping raw JSON into a toast.
 *
 * @module
 */

export interface ParsedApiError {
  /** HTTP status, or null when the message doesn't carry one. */
  status: number | null;
  /** The body's `error` field when it is JSON, else the raw body text. */
  error: string;
  /** The body's `referrers` list (409 in_use), else empty. */
  referrers: string[];
  /** The full original message, for fallback display. */
  raw: string;
}

/** Parses an unknown thrown value into {@link ParsedApiError}. */
export function parseApiError(e: unknown): ParsedApiError {
  const raw = e instanceof Error ? e.message : String(e);
  const m = raw.match(/^(\d{3}): ([\s\S]*)$/);
  if (!m) return { status: null, error: raw, referrers: [], raw };
  const status = Number(m[1]);
  const body = m[2];
  try {
    const json = JSON.parse(body) as { error?: unknown; referrers?: unknown };
    return {
      status,
      error: typeof json.error === 'string' ? json.error : body,
      referrers: Array.isArray(json.referrers)
        ? json.referrers.filter((r): r is string => typeof r === 'string')
        : [],
      raw,
    };
  } catch {
    return { status, error: body, referrers: [], raw };
  }
}
