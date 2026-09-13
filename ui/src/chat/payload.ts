/**
 * Formats a tool call's arguments or result for display.
 *
 * Two things make a raw payload unreadable as one JSON block:
 *
 * - **Double encoding.** Our write tools accept a definition as a JSON or YAML
 *   *string*, so a model often sends `{"definition": "{\"nodes\": []}"}`.
 *   Pretty-printing that verbatim shows `\"` escapes instead of structure.
 * - **Multi-line strings.** A YAML definition or an `export_config` result is
 *   a string full of `\n`, which JSON escapes onto one line.
 *
 * So: unwrap strings that are themselves JSON, and lift multi-line strings out
 * into their own blocks. The transformation is display-only — what was sent is
 * unchanged.
 *
 * @module chat/payload
 */

/** One rendered block: the main JSON (or text), then any lifted string field. */
export interface PayloadBlock {
  /** Field name, for a block lifted out of the JSON. */
  label?: string;
  text: string;
  lang: 'json' | 'text';
}

/** Marker left in the JSON where a multi-line string was lifted out. */
export const LIFTED = '…(shown below)';

/** Nothing worth rendering: blank, `{}`, `[]`, `null`. */
export function isEmptyPayload(raw: string): boolean {
  const t = raw.trim();
  if (t === '' || t === 'null') return true;
  try {
    const v: unknown = JSON.parse(t);
    return v === null || (typeof v === 'object' && Object.keys(v as object).length === 0);
  } catch {
    return false;
  }
}

/** JSON.parse that returns undefined instead of throwing. */
function tryParse(text: string): unknown {
  try {
    return JSON.parse(text) as unknown;
  } catch {
    return undefined;
  }
}

/** Only objects and arrays are worth unwrapping — a quoted scalar is just text. */
function isContainer(v: unknown): v is Record<string, unknown> | unknown[] {
  return typeof v === 'object' && v !== null;
}

/**
 * Replaces every string that is itself a JSON object/array with the parsed
 * value, so nested payloads show as structure rather than `\"` escapes.
 */
function unwrapNested(v: unknown, depth = 0): unknown {
  if (depth > 6) return v;
  if (typeof v === 'string') {
    const inner = tryParse(v);
    return isContainer(inner) ? unwrapNested(inner, depth + 1) : v;
  }
  if (Array.isArray(v)) return v.map((e) => unwrapNested(e, depth + 1));
  if (isContainer(v)) {
    const out: Record<string, unknown> = {};
    for (const [k, val] of Object.entries(v)) out[k] = unwrapNested(val, depth + 1);
    return out;
  }
  return v;
}

/**
 * Formats a payload into the blocks to render. An unparseable payload comes
 * back as a single text block; `{}`/`[]`/blank yield no blocks at all.
 */
export function formatPayload(raw: string): PayloadBlock[] {
  const trimmed = raw.trim();
  if (trimmed === '') return [];
  const parsed = tryParse(trimmed);
  if (parsed === undefined) return [{ text: raw, lang: 'text' }];

  // A payload that is a JSON string was encoded twice: unwrap it, and keep
  // unwrapping while the result is still a JSON document.
  let value = unwrapNested(parsed);
  if (typeof value === 'string') return [{ text: value, lang: 'text' }];
  if (!isContainer(value)) return [{ text: String(value), lang: 'text' }];

  // Lift multi-line top-level strings (YAML definitions, exported config) so
  // they render as text instead of one escaped line.
  const extras: PayloadBlock[] = [];
  if (!Array.isArray(value)) {
    const obj: Record<string, unknown> = { ...(value as Record<string, unknown>) };
    for (const [k, val] of Object.entries(obj)) {
      if (typeof val === 'string' && val.includes('\n')) {
        extras.push({ label: k, text: val, lang: 'text' });
        obj[k] = LIFTED;
      }
    }
    value = obj;
  }

  return [{ text: JSON.stringify(value, null, 2), lang: 'json' }, ...extras];
}
