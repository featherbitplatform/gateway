/**
 * Pure helpers for layering a node's local plugin config over the config
 * inherited from a shared plugin config (`config_ref`). Mirrors the
 * gateway's compile-time merge semantics: shallow, per top-level key, a
 * local key (including an explicit `null`) always wins.
 *
 * Used by NodeInspector/SchemaForm to show inherited values inline, to
 * auto-drop edits that land back on the inherited value, and to compute the
 * effective config captured by "Save as shared config".
 *
 * @module configInheritance
 */

/** Where a field's currently displayed value comes from. */
export type KeyOrigin = 'default' | 'inherited' | 'override' | 'added';

/** A key counts as present when it exists and is not `undefined` (JSON has no undefined; the serializer drops it). */
function has(obj: Record<string, unknown>, key: string): boolean {
  return key in obj && obj[key] !== undefined;
}

/** Structural deep equality over JSON-shaped values (objects, arrays, scalars, null). */
function deepEqual(a: unknown, b: unknown): boolean {
  if (Object.is(a, b)) return true;
  if (typeof a !== 'object' || typeof b !== 'object' || a === null || b === null) return false;
  if (Array.isArray(a) !== Array.isArray(b)) return false;
  if (Array.isArray(a) && Array.isArray(b)) {
    return a.length === b.length && a.every((v, i) => deepEqual(v, b[i]));
  }
  const ka = Object.keys(a as Record<string, unknown>);
  const kb = Object.keys(b as Record<string, unknown>);
  if (ka.length !== kb.length) return false;
  return ka.every(
    (k) =>
      k in (b as Record<string, unknown>) &&
      deepEqual((a as Record<string, unknown>)[k], (b as Record<string, unknown>)[k])
  );
}

/**
 * The value a form field should display for `key`: the local value when the
 * key is locally set (explicit `null` included), else the inherited value,
 * else `fieldDefault`.
 */
export function displayValue(
  local: Record<string, unknown>,
  inherited: Record<string, unknown>,
  key: string,
  fieldDefault?: unknown
): unknown {
  if (has(local, key)) return local[key];
  if (has(inherited, key)) return inherited[key];
  return fieldDefault;
}

/**
 * Classifies `key` by which layer(s) define it: `override` (local shadows
 * inherited), `added` (local only), `inherited` (shared config only), or
 * `default` (neither — the field shows its schema default).
 */
export function classifyKey(
  local: Record<string, unknown>,
  inherited: Record<string, unknown>,
  key: string
): KeyOrigin {
  const inLocal = has(local, key);
  const inInherited = has(inherited, key);
  if (inLocal) return inInherited ? 'override' : 'added';
  return inInherited ? 'inherited' : 'default';
}

/**
 * Returns the local config after editing `key` to `value`, without mutating
 * the inputs. An edit that lands exactly on the inherited value (deep
 * equality) — or clears the field (`undefined`) — drops the key from local
 * config so the field stays inherited instead of pinning a redundant copy.
 */
export function applyEdit(
  local: Record<string, unknown>,
  inherited: Record<string, unknown>,
  key: string,
  value: unknown
): Record<string, unknown> {
  const dropped = value === undefined || (has(inherited, key) && deepEqual(value, inherited[key]));
  if (dropped) {
    const rest = { ...local };
    delete rest[key];
    return rest;
  }
  return { ...local, [key]: value };
}

/**
 * The node's effective config: inherited keys overlaid by local keys
 * (shallow, local wins — same semantics as the gateway's compile-time
 * merge). This is what "Save as shared config" captures.
 */
export function mergeEffective(
  inherited: Record<string, unknown>,
  local: Record<string, unknown>
): Record<string, unknown> {
  return { ...inherited, ...local };
}
