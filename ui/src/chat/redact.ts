/**
 * Client-side secret redaction for the agent chat — a second line of
 * defence behind the gateway's own capture-time trace redaction and
 * credential masking. Runs over everything the chat stores or sends:
 * seeded prompts, typed messages, tool results.
 *
 * @module chat/redact
 */

export const REDACTED = '[REDACTED]';

export interface RedactResult {
  text: string;
  /** Number of replacements made. */
  count: number;
}

/** Keys whose values are secrets wherever they appear (JSON, YAML, headers). */
const SECRET_KEYS = [
  'password', 'passwd', 'pass', 'secret', 'client_secret', 'client-secret',
  'api_key', 'apikey', 'api-key', 'x-api-key', 'x-auth-token',
  'access_token', 'refresh_token', 'id_token', 'auth_token', 'session_token', 'token', 'bearer',
  'private_key', 'private-key', 'secret_key', 'secret-key', 'access_key', 'access-key',
  'authorization', 'proxy-authorization', 'cookie', 'set-cookie',
];

const KEY_ALT = SECRET_KEYS.map((k) => k.replace(/-/g, '\\-')).join('|');

/**
 * `key: value`, `"key": "value"`, `key=value` — the value runs to a delimiter.
 * The lookbehind rejects only an alphanumeric left neighbour, so a *suffix*
 * match still fires on prefixed keys (`db_password`, `oauth_client_secret`,
 * `custom-api-key`, `csrf_token`) while `token_count`/`passthrough`/`bypass`
 * stay untouched — those fail on the key's own right-hand boundary (`[:=]`)
 * rather than on the lookbehind.
 */
const KEYED_VALUE = new RegExp(`(?<![A-Za-z0-9])((?:"|')?(?:${KEY_ALT})(?:"|')?\\s*[:=]\\s*)("?)([^"\\r\\n,}]*)`, 'gi');
const AUTH_SCHEME = /\b(Bearer|Basic|Digest|Token)\s+([A-Za-z0-9\-._~+/=]{8,})/g;
const JWT = /\beyJ[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}\b/g;
const PEM = /-----BEGIN [A-Z ]*PRIVATE KEY-----[\s\S]*?-----END [A-Z ]*PRIVATE KEY-----/g;
const KEY_PREFIXES = /\b(sk-[A-Za-z0-9_-]{16,}|ghp_[A-Za-z0-9]{20,}|gh[ousr]_[A-Za-z0-9]{20,}|AKIA[0-9A-Z]{16}|xox[baprs]-[A-Za-z0-9-]{10,})\b/g;

/** Values that are safe to keep: empty, `${ENV}` placeholders, already-masked markers, plain booleans/numbers. */
function keepValue(v: string): boolean {
  const t = v.trim();
  return t === '' || t.startsWith('${') || t === REDACTED || t === '<redacted>' || t === '<masked>' || /^(true|false|null|\d+)$/.test(t);
}

function escapeRe(s: string): string {
  return s.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
}

/**
 * Replaces secrets with {@link REDACTED}. `literals` are exact strings to
 * remove wherever they appear (the user's own API key and MCP token);
 * entries shorter than 8 characters are ignored to avoid shredding prose.
 */
export function redactSecrets(text: string, literals: readonly string[] = []): RedactResult {
  let count = 0;
  let out = text;
  const sub = (re: RegExp, replacement: (match: string, group1: string) => string) => {
    out = out.replace(re, (m: string, g1: string) => {
      count += 1;
      return replacement(m, g1);
    });
  };
  for (const lit of literals) {
    if (lit.length >= 8) sub(new RegExp(escapeRe(lit), 'g'), () => REDACTED);
  }
  sub(PEM, () => REDACTED);
  sub(JWT, () => REDACTED);
  sub(KEY_PREFIXES, () => REDACTED);
  out = out.replace(KEYED_VALUE, (m: string, prefix: string, quote: string, value: string) => {
    if (keepValue(value)) return m;
    count += 1;
    return `${prefix}${quote}${REDACTED}`;
  });
  sub(AUTH_SCHEME, (_m, scheme) => `${scheme} ${REDACTED}`);
  return { text: out, count };
}
