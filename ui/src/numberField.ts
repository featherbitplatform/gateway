/**
 * Parsing for numeric config fields that may also hold an environment
 * placeholder.
 *
 * Plugin config is env-resolved at graph-compile time, and a value that is
 * exactly one `${NAME}` / `${NAME:-default}` placeholder comes back typed:
 * `port: "${BACKEND_PORT}"` compiles to the number the variable holds
 * (see `interpolate_env_json` in src/config/loader.rs). So a numeric field
 * stores either a number or such a placeholder string, and nothing else.
 *
 * @module numberField
 */

/** Exactly one placeholder, the shape the backend coerces to a number. Mirrors the regex in src/config/loader.rs. */
const PLACEHOLDER = /^\$\{[A-Za-z_][A-Za-z0-9_]*(?::-(?:[^}\\]|\\.)*)?\}$/;

/** True for a string the backend resolves and types as a whole value. */
export function isEnvPlaceholder(text: string): boolean {
  return PLACEHOLDER.test(text);
}

export type NumberFieldParse =
  /** Store this value (`undefined` = clear the key). */
  | { ok: true; value: number | string | undefined }
  /** Keep showing the text but do not store it. */
  | { ok: false };

/**
 * What typing `text` into a numeric field should store: a number, an env
 * placeholder (kept as a string), `undefined` for an empty field, or nothing
 * at all for anything else (a half-typed `${BACK`, `12abc`).
 */
export function parseNumberField(text: string): NumberFieldParse {
  const t = text.trim();
  if (t === '') return { ok: true, value: undefined };
  if (isEnvPlaceholder(t)) return { ok: true, value: t };
  // Number('') and Number(' ') are 0, and Number('0x10') is 16: accept only
  // plain decimal notation, like a number input would.
  if (/^[+-]?(\d+\.?\d*|\.\d+)([eE][+-]?\d+)?$/.test(t)) {
    const n = Number(t);
    if (Number.isFinite(n)) return { ok: true, value: n };
  }
  return { ok: false };
}

/** The text a stored value shows as (numbers and placeholder strings alike). */
export function numberFieldText(value: unknown): string {
  if (typeof value === 'number') return Number.isFinite(value) ? String(value) : '';
  if (typeof value === 'string') return value;
  return '';
}
