/**
 * Trace-derived `$var` suggestion engine.
 *
 * Feeds the node-editor's variable autocomplete popover and the var legend.
 * Data flow: the {@link VarEntry} catalog (`GET /api/vars`, static — one per
 * gateway process) describes every name/family `resolve()` accepts, but
 * carries no *live values*; those come from the most recent debug trace for
 * the node's policy. {@link useContextSuggestions} stitches the two
 * together: it loads the catalog once (module-level cache), looks up the
 * predecessor node's {@link ContextSnapshot} within the latest trace, and
 * calls {@link buildSuggestions} to produce concrete, previewable rows.
 *
 * Exported signatures (kept in sync with this comment — update both if
 * either drifts):
 *
 * ```ts
 * function insertionText(name: string): string;
 * function previewValue(raw: string): string;
 * function predecessorSnapshot(trace: TraceDetail, predecessorId: string | null): ContextSnapshot | null;
 * function bodyText(trace: TraceDetail, stepIndex: number, which: 'request' | 'response'): string | null;
 * function buildSuggestions(
 *   catalog: VarEntry[],
 *   snapshot: ContextSnapshot | null,
 *   capturedBodies: boolean,
 *   requestBodyText?: string | null,
 *   responseBodyText?: string | null,
 * ): Suggestion[];
 * function useContextSuggestions(args: {
 *   policyName: string | null;
 *   nodeId: string | null;
 *   predecessorId: string | null | undefined;
 *   kind: 'policy' | 'supernode';
 *   debugEnabled: boolean;
 *   captureBodies: boolean;
 * }): { suggestions: Suggestion[]; availability: Availability; catalog: VarEntry[] };
 * ```
 *
 * `buildSuggestions` was deliberately given optional `requestBodyText` /
 * `responseBodyText` parameters rather than resolving body text itself from
 * a step index — {@link useContextSuggestions} is the only caller that has
 * a trace + step index to walk, so it computes the texts via {@link bodyText}
 * and passes them in; `buildSuggestions` stays a pure function of a single
 * snapshot plus the two already-resolved strings.
 *
 * `predecessorId` uses an undefined-vs-null encoding chosen by the caller
 * (the node editor, in Task 5/6): `undefined` means "this node has no
 * incoming edge yet" (nothing to preview from), `null` means "the incoming
 * edge comes from the listener" (preview from `trace.initial`, there being
 * no prior node step), and a string is a real predecessor node id.
 *
 * `headerFamily` (backing both `http_*` and `sent_http_*`) skips-with-a-note
 * any captured header whose name contains `_`: the suggested var name is
 * built by lowercasing and mapping `-` to `_` (`X-Trace-Id` → `http_x_trace_id`),
 * but `resolve()` on the gateway side maps `_` back to `-` when it looks the
 * header up (`src/vars/mod.rs`), so a header that had a literal `_` in it
 * (e.g. `X_Trace_Id`) is not the header that `$http_x_trace_id` actually
 * resolves — it would look up `x-trace-id` instead and most likely miss. The
 * chosen fix is a per-row note ("header name contains '_' — not resolvable
 * via $http_*") rather than omitting the row outright, so the header is
 * still visible in the popover/legend but is never shown with a (misleading)
 * live value. That guard is specific to the legacy `$http_*` mangling and
 * does **not** apply to the `{{request.headers.<h>}}` universal-template
 * form below — dashes (and underscores) are legal characters in that
 * syntax, so a header name is used there exactly as captured, with no
 * mangling and therefore nothing to warn about.
 *
 * ## Two suggestion families, one list
 *
 * Since Task 8, every row built here is tagged {@link Suggestion.trigger}:
 * `'dollar'` for the legacy `$name`/`${name}` completions (unchanged from
 * before), or `'brace'` for the new `{{path}}` completions keyed off each
 * catalog entry's `path` (entries with an empty `path` — no direct template
 * equivalent, e.g. `protocol`, `post_arg_*` — simply produce no `'brace'`
 * row). Both families are built into the *same* flat list rather than two
 * separate hook calls or two separate lists, because {@link
 * useContextSuggestions} has exactly one source of truth (one catalog fetch,
 * one trace snapshot) and a single field only ever wants one trigger's rows
 * at a time — so the cheapest place to split them is at render time, in
 * {@link VarInput}, by filtering on `trigger` for whichever token (`$`/`${`
 * vs `{{`) the user is actually completing. This also means a `'brace'` and
 * a `'dollar'` row can legitimately share the same `name` string coincidence
 * without colliding (they never render side by side), and callers that
 * only ever want one family (like the var legend, keyed by the catalog's
 * legacy `name`) are unaffected by the other family's presence in the list.
 * `'brace'` rows for two different static catalog entries that happen to
 * share a `path` (e.g. `uri`/`request_uri` both — for now — mapping to
 * `request.path`) are de-duplicated by `insert` text in {@link
 * buildSuggestions}, since two rows that insert byte-identical text would
 * otherwise be indistinguishable clutter in the popover.
 *
 * A new `'env'` group (fed by `GET /api/env-vars` via {@link
 * useContextSuggestions}, module-cached the same way as the catalog) is
 * `'brace'`-only: `{{env.NAME}}` has no `$`-syntax equivalent (the `{{` token
 * is still what opens the popover on an `'env-only'` field), so every
 * `env.*` row is tagged `'brace'` and carries no `value` (env-var values are
 * never fetched or shown — only names). It does, however, carry a second
 * *insertion* form, `insertEnvOnly` (`${NAME}`), that `VarInput` swaps in for
 * `'env-only'` fields — see `Suggestion.insertEnvOnly`'s doc comment for why
 * `{{env.NAME}}` itself would never actually resolve there.
 *
 * `VarInput`'s new `templateMode` prop (`'full' | 'env-only' | 'none'`,
 * default `'full'`) is a second, orthogonal filter applied on top of
 * `trigger`: Task 9 decides which fields pass a non-default mode, but the
 * filtering itself lives in `VarInput` now so one hook instance/suggestion
 * list can serve every field regardless of its mode.
 *
 * @module varSuggestions
 */
import { useEffect, useState } from 'react';
import { api } from './api/client';
import type { BodyCapture, ContextSnapshot, TraceDetail, VarEntry } from './types';

/** Why live suggestions may currently be unavailable, for UI messaging. */
export type Availability =
  | 'ok'
  | 'debug-off'
  | 'no-incoming-edge'
  | 'no-trace'
  | 'supernode-definition';

/**
 * Exact copy for each non-`ok` {@link Availability}, shown in both
 * `VarInput`'s popover footer and `TemplateEditorModal`'s footer (Task 2) —
 * lives here rather than in either component file so the two share one copy
 * without either importing a non-component value from the other (which
 * would trip `react-refresh/only-export-components` on whichever component
 * file it lived in).
 */
export const AVAILABILITY_MESSAGE: Record<Exclude<Availability, 'ok'>, string> = {
  'debug-off': 'Debug is off — enable debug.enabled for live values',
  'no-incoming-edge': 'No incoming edge — connect this node to preview values',
  'no-trace': 'No trace yet — send a request through this route',
  'supernode-definition': 'Live values unavailable while editing a supernode definition',
};

/**
 * Footer copy for fields that never render context templates
 * ({@link FieldSchema.template} `'env-only'`) — shown in place of
 * {@link AVAILABILITY_MESSAGE} in both `VarInput`'s popover footer and
 * `TemplateEditorModal`'s footer, since availability (debug on/off, trace
 * present) describes context-suggestion readiness, which is irrelevant when
 * context suggestions are never offered at all.
 */
export const ENV_ONLY_MESSAGE =
  "Context data isn't available here — this value is fixed when configuration loads. ${ENV} references still apply.";

/** One row offered by the autocomplete popover / var legend. */
export interface Suggestion {
  /**
   * Display/filter name: the bare `$var` name (e.g. `uri`,
   * `http_user_agent`) for `trigger: 'dollar'` rows, or the full namespace
   * path (e.g. `request.method`, `request.headers.x-trace-id`, `env.HOME`)
   * for `trigger: 'brace'` rows.
   */
  name: string;
  /**
   * Text to insert at the cursor: `$uri` / `${msg_consumer.name}` for
   * `'dollar'` rows, `{{request.method}}` / `{{env.HOME}}` for `'brace'`
   * rows.
   */
  insert: string;
  /**
   * Alternate insertion text used only for `group: 'env'` rows when the
   * field's `templateMode` is `'env-only'` — `${NAME}`, the legacy
   * `${VAR}`/`${VAR:-default}` form `interpolate_env`/`interpolate_env_json`
   * (`src/config/loader.rs`) substitutes at config-compile time for *every*
   * string leaf of *every* node's config, templated or not. `insert`
   * (`{{env.NAME}}`) only ever resolves in fields the plugin actually parses
   * into a `Template` (`template: 'full'` fields) — `Template::parse` is what
   * substitutes `env.*` references, and an `'env-only'` field's raw string
   * never reaches `Template::parse` at all, so `{{env.NAME}}` would sit
   * there as inert, unresolved literal text forever. `VarInput` picks
   * between the two at insertion time based on its `templateMode` prop; see
   * that component and Templates → Load-time vs request-time resolution /
   * Exclusions on the docs site for the full explanation. `undefined` for
   * every non-`env` row (they have no env-only field they'd ever appear in —
   * the `env` group is the only one offered on `'env-only'` fields).
   */
  insertEnvOnly?: string;
  /** Section this row belongs to: `request` | `response` | `message` | `env`. */
  group: string;
  /** One-line human-readable explanation, from the catalog entry. */
  description?: string;
  /** Live preview value from the most recent trace, when available. */
  value?: string;
  /** Explains why `value` is absent (e.g. redacted, not captured, empty). */
  note?: string;
  /**
   * Which token syntax this row completes — see the module doc comment
   * ("Two suggestion families, one list"). `VarInput` filters the list by
   * whichever trigger (`$`/`${` vs `{{`) opened the popover, so a `'dollar'`
   * row is never offered while completing a `{{` token and vice versa.
   */
  trigger: 'dollar' | 'brace';
}

const REQUEST = 'request';
const RESPONSE = 'response';
const MESSAGE = 'message';
const ENV = 'env';

/** Fixed section for each static var name (independent of any snapshot). */
const STATIC_GROUPS: Record<string, string> = {
  uri: REQUEST,
  request_uri: REQUEST,
  method: REQUEST,
  request_method: REQUEST,
  host: REQUEST,
  scheme: REQUEST,
  protocol: REQUEST,
  remote_addr: REQUEST,
  remote_port: REQUEST,
  query_string: REQUEST,
  status: RESPONSE,
  resp_body: RESPONSE,
  request_body: REQUEST,
  consumer_name: MESSAGE,
  consumer_group_id: MESSAGE,
};

/** Fixed section for each family, keyed by its catalog `name` (`foo_*`). */
const FAMILY_GROUPS: Record<string, string> = {
  'arg_*': REQUEST,
  'http_*': REQUEST,
  'cookie_*': REQUEST,
  'post_arg_*': REQUEST,
  'msg_*': MESSAGE,
  'sent_http_*': RESPONSE,
};

/**
 * Whether `$name` can be used bare or needs the `${name}` brace form —
 * mirrors the tokenizer's identifier rule (`interpolate` in src/vars/mod.rs
 * only greedily consumes `[A-Za-z0-9_]` after a bare `$`).
 */
export function insertionText(name: string): string {
  return /^[A-Za-z0-9_]+$/.test(name) ? `$${name}` : `\${${name}}`;
}

/**
 * Collapses a raw value to one line and truncates it to ~80 chars, so
 * previews stay compact in the popover and legend. Shared by both.
 */
export function previewValue(raw: string): string {
  const singleLine = raw.replace(/\s+/g, ' ').trim();
  return singleLine.length > 80 ? `${singleLine.slice(0, 80)}…` : singleLine;
}

/**
 * Resolves the {@link ContextSnapshot} a node should preview variables
 * from: the snapshot captured *after* its predecessor ran.
 *
 * @param predecessorId - `null` for a predecessor that is the listener
 * (preview from the trace's initial snapshot, before any node ran); a node
 * id for a real predecessor step. (`undefined` is handled the same as
 * `null` here for completeness, though callers are expected to short-circuit
 * on `undefined` before reaching this function — see the module doc comment.)
 * @returns `null` when `predecessorId` names a step not present in the
 * trace (e.g. the trace is stale relative to the current graph).
 */
export function predecessorSnapshot(
  trace: TraceDetail,
  predecessorId: string | null | undefined,
): ContextSnapshot | null {
  if (predecessorId === null || predecessorId === undefined) return trace.initial;
  return trace.steps.find((s) => s.node_id === predecessorId)?.after ?? null;
}

function bodyCapture(snapshot: ContextSnapshot, which: 'request' | 'response'): BodyCapture {
  return which === 'request' ? snapshot.request.body : snapshot.response.body;
}

/**
 * Finds the most recent captured body text at or before `stepIndex`.
 *
 * Bodies are only captured (as `text`) when they change; unchanged bodies
 * are marked `unchanged: true` with no `text`, so this walks backwards
 * through step snapshots while a body stays `unchanged` with no `text`,
 * stopping at the first `text` found, at `trace.initial`, or at a step
 * whose body changed but wasn't captured as text (binary/too large) — in
 * which case there is nothing valid to backfill from and the result is
 * `null`.
 *
 * @param stepIndex - Index into `trace.steps` to start from; `-1` means
 * "no step yet" (the listener predecessor case), starting the search at
 * `trace.initial` directly.
 */
export function bodyText(
  trace: TraceDetail,
  stepIndex: number,
  which: 'request' | 'response',
): string | null {
  for (let i = stepIndex; i >= 0; i -= 1) {
    const step = trace.steps[i];
    if (!step) continue;
    const body = bodyCapture(step.after, which);
    if (body.text !== undefined) return body.text;
    if (!body.unchanged) return null;
  }
  const initial = bodyCapture(trace.initial, which);
  return initial.text ?? null;
}

function stringifyMessageValue(v: unknown): string {
  return typeof v === 'string' ? v : JSON.stringify(v);
}

/** Case-insensitive header lookup (captured header keys may retain original case). */
function firstHeaderValue(headers: Record<string, string[]>, name: string): string | undefined {
  const lower = name.toLowerCase();
  for (const [k, values] of Object.entries(headers)) {
    if (k.toLowerCase() === lower) return values[0];
  }
  return undefined;
}

/**
 * Rebuilds `k=v&k=v...`, matching `query_string()` in src/vars/mod.rs
 * exactly: every `k=v` pair is collected as a string first, and the pair
 * *strings* are sorted (not the keys) — so repeated keys get their values
 * ordered too, with `=` participating in the comparison. Do not "optimize"
 * this to sort keys first and emit values in array order; that produces a
 * different (wrong) order whenever a key repeats, e.g. `{a: ["z","b"],
 * a2: ["3"]}` must rebuild to `a2=3&a=b&a=z`, not `a=z&a=b&a2=3`.
 */
function rebuildQueryString(params: Record<string, string[]>): string {
  const pairs: string[] = [];
  for (const [k, vs] of Object.entries(params)) {
    for (const v of vs) pairs.push(`${k}=${v}`);
  }
  return pairs.sort().join('&');
}

/** Mirrors `urldecode()` in src/vars/mod.rs closely enough for a preview value. */
function urldecodeFormValue(s: string): string {
  try {
    return decodeURIComponent(s.replace(/\+/g, ' '));
  } catch {
    return s;
  }
}

/** Parses `k=v&k=v...`, decoding values only (keys stay literal, as `post_arg_<field>` expects). */
function parseFormBody(text: string): Array<[string, string]> {
  const out: Array<[string, string]> = [];
  for (const pair of text.split('&')) {
    if (!pair) continue;
    const idx = pair.indexOf('=');
    if (idx === -1) continue;
    out.push([pair.slice(0, idx), urldecodeFormValue(pair.slice(idx + 1))]);
  }
  return out;
}

/** Shared by both the `resp_body`/`request_body` static entries in {@link staticExtra}. */
function bodyExtra(text: string | null | undefined, capturedBodies: boolean): Partial<Suggestion> {
  if (text !== null && text !== undefined) return { value: previewValue(text) };
  return { note: capturedBodies ? 'no body captured yet' : 'enable debug.capture_bodies' };
}

/** Shared by both the `consumer_name`/`consumer_group_id` static entries in {@link staticExtra}. */
function messageValueExtra(snapshot: ContextSnapshot, key: string): Partial<Suggestion> {
  const v = snapshot.message[key];
  return v === undefined ? {} : { value: previewValue(stringifyMessageValue(v)) };
}

/**
 * Computes the value/note extras for a static catalog entry against a
 * snapshot — factored out of {@link staticSuggestion} so the identical
 * per-entry logic can be shared with {@link staticPathSuggestion} without
 * duplicating the switch, since both the `$name` row and the `{{path}}` row
 * for the same entry preview the exact same underlying context field.
 */
function staticExtra(
  entry: VarEntry,
  snapshot: ContextSnapshot | null,
  capturedBodies: boolean,
  requestBodyText: string | null | undefined,
  responseBodyText: string | null | undefined,
): Partial<Suggestion> {
  if (!snapshot) return {};

  switch (entry.name) {
    case 'uri':
      return { value: previewValue(snapshot.request.path) };
    case 'request_uri': {
      const qs = rebuildQueryString(snapshot.request.query_params);
      const value = qs ? `${snapshot.request.path}?${qs}` : snapshot.request.path;
      return { value: previewValue(value) };
    }
    case 'method':
    case 'request_method':
      return { value: previewValue(snapshot.request.method) };
    case 'host':
      return { value: previewValue(snapshot.request.host) };
    case 'scheme':
      return { value: previewValue(snapshot.request.scheme) };
    case 'status':
      return { value: previewValue(String(snapshot.response.status_code)) };
    case 'query_string': {
      const qs = rebuildQueryString(snapshot.request.query_params);
      return qs ? { value: previewValue(qs) } : { note: 'empty' };
    }
    case 'consumer_name':
      return messageValueExtra(snapshot, 'consumer.name');
    case 'consumer_group_id':
      return messageValueExtra(snapshot, 'consumer.group');
    case 'resp_body':
      return bodyExtra(responseBodyText, capturedBodies);
    case 'request_body':
      return bodyExtra(requestBodyText, capturedBodies);
    case 'protocol':
    case 'remote_addr':
    case 'remote_port':
      return { note: 'not captured in traces' };
    default:
      return {};
  }
}

function staticSuggestion(
  entry: VarEntry,
  snapshot: ContextSnapshot | null,
  capturedBodies: boolean,
  requestBodyText: string | null | undefined,
  responseBodyText: string | null | undefined,
): Suggestion {
  return {
    name: entry.name,
    insert: insertionText(entry.name),
    group: STATIC_GROUPS[entry.name] ?? REQUEST,
    description: entry.description,
    trigger: 'dollar',
    ...staticExtra(entry, snapshot, capturedBodies, requestBodyText, responseBodyText),
  };
}

/**
 * `{{path}}` counterpart to {@link staticSuggestion} — `null` when the entry
 * has no template equivalent (`entry.path === ''`), per the module doc
 * comment.
 */
function staticPathSuggestion(
  entry: VarEntry,
  snapshot: ContextSnapshot | null,
  capturedBodies: boolean,
  requestBodyText: string | null | undefined,
  responseBodyText: string | null | undefined,
): Suggestion | null {
  if (!entry.path) return null;
  return {
    name: entry.path,
    insert: `{{${entry.path}}}`,
    group: STATIC_GROUPS[entry.name] ?? REQUEST,
    description: entry.description,
    trigger: 'brace',
    ...staticExtra(entry, snapshot, capturedBodies, requestBodyText, responseBodyText),
  };
}

function familyRow(name: string, group: string, description: string, note?: string): Suggestion {
  return {
    name,
    insert: insertionText(name),
    group,
    description,
    trigger: 'dollar',
    ...(note ? { note } : {}),
  };
}

function headerFamily(
  headers: Record<string, string[]> | undefined,
  prefix: string,
  group: string,
  description: string,
): Suggestion[] {
  const names = headers ? Object.keys(headers).sort() : [];
  if (names.length === 0) return [familyRow(`${prefix}*`, group, description)];
  return names.map((h) => {
    const varName = `${prefix}${h.toLowerCase().replace(/-/g, '_')}`;
    // A header name that itself contains '_' can't round-trip through
    // resolve()'s '_' -> '-' mapping (see module doc): the suggested
    // `$http_*`/`$sent_http_*` var would resolve a *different* header (with
    // '-' where this one has '_') and most likely come back empty. Surface
    // the name so it's still discoverable, but never attach a value/insert
    // that would look resolvable.
    if (h.includes('_')) {
      return familyRow(
        varName,
        group,
        description,
        "header name contains '_' — not resolvable via $http_*",
      );
    }
    const value = headers![h][0];
    return {
      name: varName,
      insert: insertionText(varName),
      group,
      description,
      trigger: 'dollar' as const,
      ...(value !== undefined ? { value: previewValue(value) } : {}),
    };
  });
}

/**
 * `{{<pathPrefix><h>}}` counterpart to the header half of {@link
 * headerFamily} — one row per captured header, keyed off the raw header
 * name exactly as captured (no lowercasing, no `-` → `_` mapping): the
 * universal-template grammar allows dashes verbatim and resolves headers
 * case-insensitively (`src/vars/template.rs`), so unlike `$http_*` there is
 * no mangling to guard against and no fallback/no-note case is needed. No
 * members means no rows at all (no unexpanded `request.headers.*` fallback
 * row) — there is nothing meaningful to insert for a bare family path.
 */
function headerFamilyPaths(
  headers: Record<string, string[]> | undefined,
  pathPrefix: string,
  group: string,
  description: string,
): Suggestion[] {
  const names = headers ? Object.keys(headers).sort() : [];
  return names.map((h) => {
    const path = `${pathPrefix}${h}`;
    const value = headers![h][0];
    return {
      name: path,
      insert: `{{${path}}}`,
      group,
      description,
      trigger: 'brace' as const,
      ...(value !== undefined ? { value: previewValue(value) } : {}),
    };
  });
}

function queryParamFamily(
  params: Record<string, string[]> | undefined,
  group: string,
  description: string,
): Suggestion[] {
  const keys = params ? Object.keys(params).sort() : [];
  if (keys.length === 0) return [familyRow('arg_*', group, description)];
  return keys.map((k) => {
    const varName = `arg_${k}`;
    return {
      name: varName,
      insert: insertionText(varName),
      group,
      description,
      trigger: 'dollar' as const,
      value: previewValue(params![k][0]),
    };
  });
}

/** `{{request.query.<k>}}` counterpart to {@link queryParamFamily}. */
function queryParamFamilyPaths(
  params: Record<string, string[]> | undefined,
  group: string,
  description: string,
): Suggestion[] {
  const keys = params ? Object.keys(params).sort() : [];
  return keys.map((k) => {
    const path = `request.query.${k}`;
    return {
      name: path,
      insert: `{{${path}}}`,
      group,
      description,
      trigger: 'brace' as const,
      value: previewValue(params![k][0]),
    };
  });
}

function messageFamily(
  message: Record<string, unknown> | undefined,
  group: string,
  description: string,
): Suggestion[] {
  const keys = message ? Object.keys(message).sort() : [];
  if (keys.length === 0) return [familyRow('msg_*', group, description)];
  return keys.map((k) => {
    const varName = `msg_${k}`;
    return {
      name: varName,
      insert: insertionText(varName),
      group,
      description,
      trigger: 'dollar' as const,
      value: previewValue(stringifyMessageValue(message![k])),
    };
  });
}

/** `{{message.<k>}}` counterpart to {@link messageFamily}. */
function messageFamilyPaths(
  message: Record<string, unknown> | undefined,
  group: string,
  description: string,
): Suggestion[] {
  const keys = message ? Object.keys(message).sort() : [];
  return keys.map((k) => {
    const path = `message.${k}`;
    return {
      name: path,
      insert: `{{${path}}}`,
      group,
      description,
      trigger: 'brace' as const,
      value: previewValue(stringifyMessageValue(message![k])),
    };
  });
}

function postArgFamily(
  snapshot: ContextSnapshot | null,
  requestBodyText: string | null | undefined,
  group: string,
  description: string,
): Suggestion[] {
  const fallback = familyRow('post_arg_*', group, description, 'form-urlencoded bodies only');
  if (!snapshot || !requestBodyText) return [fallback];
  const contentType = firstHeaderValue(snapshot.request.headers, 'content-type');
  if (!contentType || !contentType.toLowerCase().startsWith('application/x-www-form-urlencoded')) {
    return [fallback];
  }
  const members = parseFormBody(requestBodyText);
  if (members.length === 0) return [fallback];
  return members.map(([k, v]) => {
    const varName = `post_arg_${k}`;
    return {
      name: varName,
      insert: insertionText(varName),
      group,
      description,
      trigger: 'dollar' as const,
      value: previewValue(v),
    };
  });
}

function familySuggestions(
  entry: VarEntry,
  snapshot: ContextSnapshot | null,
  requestBodyText: string | null | undefined,
): Suggestion[] {
  const group = FAMILY_GROUPS[entry.name] ?? REQUEST;
  const description = entry.description;
  switch (entry.name) {
    case 'http_*':
      return headerFamily(snapshot?.request.headers, 'http_', group, description);
    case 'sent_http_*':
      return headerFamily(snapshot?.response.headers, 'sent_http_', group, description);
    case 'arg_*':
      return queryParamFamily(snapshot?.request.query_params, group, description);
    case 'msg_*':
      return messageFamily(snapshot?.message, group, description);
    case 'cookie_*':
      return [
        familyRow(entry.name, group, description, 'cookies are redacted in traces — values never previewable'),
      ];
    case 'post_arg_*':
      return postArgFamily(snapshot, requestBodyText, group, description);
    default:
      return [familyRow(entry.name, group, description)];
  }
}

/**
 * `{{path}}` counterpart to {@link familySuggestions} — only produces rows
 * for entries with a non-empty `path` and only for families whose live
 * members are actually discoverable from a snapshot (`http_*`,
 * `sent_http_*`, `arg_*`, `msg_*`); `cookie_*` has a path
 * (`request.cookies.*`) but no discoverable member list (cookie values are
 * always redacted, so there's nothing to expand), and `post_arg_*` has an
 * empty path (skipped by the `!entry.path` guard before this is even
 * reached from {@link buildSuggestions}). No members discovered means no
 * rows — see {@link headerFamilyPaths}'s doc comment.
 */
function familyPathSuggestions(entry: VarEntry, snapshot: ContextSnapshot | null): Suggestion[] {
  if (!entry.path) return [];
  const group = FAMILY_GROUPS[entry.name] ?? REQUEST;
  const description = entry.description;
  switch (entry.name) {
    case 'http_*':
      return headerFamilyPaths(snapshot?.request.headers, 'request.headers.', group, description);
    case 'sent_http_*':
      return headerFamilyPaths(snapshot?.response.headers, 'response.headers.', group, description);
    case 'arg_*':
      return queryParamFamilyPaths(snapshot?.request.query_params, group, description);
    case 'msg_*':
      return messageFamilyPaths(snapshot?.message, group, description);
    default:
      return [];
  }
}

/**
 * Builds the full suggestion list for one predecessor snapshot.
 *
 * @param capturedBodies - Whether `debug.capture_bodies` is enabled
 * (`DebugConfig.capture_bodies`); shapes the note shown for `resp_body` /
 * `request_body` when no body text is available.
 * @param requestBodyText - Request body text resolved via {@link bodyText}
 * for the predecessor's step (backwalking unchanged bodies); `undefined`/`null`
 * when none is available.
 * @param responseBodyText - Same, for the response body.
 */
export function buildSuggestions(
  catalog: VarEntry[],
  snapshot: ContextSnapshot | null,
  capturedBodies: boolean,
  requestBodyText?: string | null,
  responseBodyText?: string | null,
): Suggestion[] {
  const out: Suggestion[] = [];
  // De-dupes `'brace'` rows by their `insert` text: two static catalog
  // entries can map to the same `path` (e.g. `uri`/`request_uri` both
  // currently → `request.path`), which would otherwise surface two rows
  // that insert byte-identical `{{...}}` text — see the module doc comment.
  // The `'dollar'` rows never need this: distinct catalog `name`s never
  // collide with each other.
  const seenPathInserts = new Set<string>();
  for (const entry of catalog) {
    if (entry.kind === 'static') {
      out.push(staticSuggestion(entry, snapshot, capturedBodies, requestBodyText, responseBodyText));
      const pathSuggestion = staticPathSuggestion(
        entry,
        snapshot,
        capturedBodies,
        requestBodyText,
        responseBodyText,
      );
      if (pathSuggestion && !seenPathInserts.has(pathSuggestion.insert)) {
        seenPathInserts.add(pathSuggestion.insert);
        out.push(pathSuggestion);
      }
    } else {
      out.push(...familySuggestions(entry, snapshot, requestBodyText));
      out.push(...familyPathSuggestions(entry, snapshot));
    }
  }
  return out;
}

/** Module-level cache: the catalog never changes within a running gateway process. */
let catalogPromise: Promise<VarEntry[]> | null = null;

function loadCatalog(): Promise<VarEntry[]> {
  if (!catalogPromise) {
    catalogPromise = api.listVars().catch((e) => {
      catalogPromise = null;
      throw e;
    });
  }
  return catalogPromise;
}

/** Module-level cache: the process environment doesn't change while the gateway runs. */
let envNamesPromise: Promise<string[]> | null = null;

function loadEnvNames(): Promise<string[]> {
  if (!envNamesPromise) {
    envNamesPromise = api.listEnvVars().catch((e) => {
      envNamesPromise = null;
      throw e;
    });
  }
  return envNamesPromise;
}

/**
 * Builds the `'env'` group: one `'brace'`-only row per environment variable
 * *name* (never a value — `GET /api/env-vars` never returns values, and
 * this function has none to show even if it did).
 *
 * Carries both insertion forms: `insert` (`{{env.NAME}}`, resolved by
 * `Template::parse` — only reachable in `template: 'full'` fields) and
 * `insertEnvOnly` (`${NAME}`, resolved by the universal `interpolate_env`/
 * `interpolate_env_json` pass that runs over every string leaf of every
 * node's config regardless of templating — the only form that actually
 * resolves in an `'env-only'` field). See the `insertEnvOnly` doc comment on
 * {@link Suggestion} for why the two aren't interchangeable.
 */
function envSuggestions(names: string[]): Suggestion[] {
  return names.map((name) => ({
    name: `env.${name}`,
    insert: `{{env.${name}}}`,
    insertEnvOnly: `\${${name}}`,
    group: ENV,
    trigger: 'brace',
  }));
}

/**
 * Resolves the live `$var`/`{{path}}` suggestions for one node in the
 * editor — every group (context statics/families *and* `env`), both
 * triggers (`'dollar'` *and* `'brace'`); see the module doc comment.
 * Per-field filtering (by trigger, and by `VarInput`'s `templateMode`) is
 * the caller's job, not this hook's — one hook instance serves every field
 * on the node regardless of that field's mode.
 *
 * See the module doc comment for the full data flow and the
 * `predecessorId` undefined-vs-null encoding. Re-fetches whenever any input
 * changes (in particular `nodeId`, when the selection moves).
 *
 * `nodeId: null` is the caller's signal that this selection needs no live
 * preview at all — the node editor passes it for fixed/supernode/boundary
 * nodes (listener, client, supernode instances, and the input/output/error
 * boundary pseudo-nodes), which never render a `VarInput` popover to show a
 * value in, anyway. `run()` below checks this *last*, immediately before the
 * `/api/debug/traces` round trip, so the cheaper synchronous branches above
 * it (supernode-definition canvas, debug-off, no-incoming-edge, no policy
 * name) still take priority and keep their existing `availability` — this
 * check only ever prevents a network call that would otherwise have fired.
 * It reuses `'no-trace'` for `availability`: no other value describes "this
 * node was never eligible for a trace lookup" any better, and since no
 * popover renders for these nodes the value is never actually shown to a
 * user — it only flows through as the (unused) `availability` return for
 * this hook's caller in that case.
 */
export function useContextSuggestions(args: {
  policyName: string | null;
  nodeId: string | null;
  predecessorId: string | null | undefined;
  kind: 'policy' | 'supernode';
  debugEnabled: boolean;
  captureBodies: boolean;
}): { suggestions: Suggestion[]; availability: Availability; catalog: VarEntry[] } {
  const { policyName, nodeId, predecessorId, kind, debugEnabled, captureBodies } = args;
  const [catalog, setCatalog] = useState<VarEntry[]>([]);
  const [suggestions, setSuggestions] = useState<Suggestion[]>([]);
  const [availability, setAvailability] = useState<Availability>('no-trace');

  useEffect(() => {
    let cancelled = false;

    async function run() {
      // The catalog and the env-var name list are independent, cacheable
      // fetches — loaded in parallel so neither one blocks the other.
      const [entries, envNames] = await Promise.all([
        loadCatalog().catch(() => [] as VarEntry[]),
        loadEnvNames().catch(() => [] as string[]),
      ]);
      if (cancelled) return;
      setCatalog(entries);
      const env = envSuggestions(envNames);

      // `env` is appended to every branch below (including the early
      // returns): env-var names never depend on a trace/snapshot, so
      // they're always available regardless of `availability`.
      function namesOnly(): Suggestion[] {
        return [...buildSuggestions(entries, null, false), ...env];
      }

      if (kind === 'supernode') {
        setAvailability('supernode-definition');
        setSuggestions(namesOnly());
        return;
      }
      if (!debugEnabled) {
        setAvailability('debug-off');
        setSuggestions(namesOnly());
        return;
      }
      if (predecessorId === undefined) {
        setAvailability('no-incoming-edge');
        setSuggestions(namesOnly());
        return;
      }
      // Without a policy name there is nothing to scope the trace lookup
      // to: api.listTraces drops a falsy `policy` filter entirely and would
      // happily return the newest trace from *any* policy in the buffer,
      // which would then get previewed as if it belonged here. Bail out
      // before fetching anything.
      if (!policyName) {
        setAvailability('no-trace');
        setSuggestions(namesOnly());
        return;
      }
      // See this function's doc comment: `nodeId: null` means the caller
      // already decided this selection gets no live preview, so skip the
      // trace fetch entirely — this is the only thing standing between a
      // fixed/supernode/boundary-node selection and two unnecessary
      // /api/debug/traces requests.
      if (nodeId === null) {
        setAvailability('no-trace');
        setSuggestions(namesOnly());
        return;
      }

      try {
        // `source: 'request'` excludes sandbox-run traces from the preview:
        // the docs promise "the latest handled request", and a sandbox trace
        // is a synthetic, user-triggered probe rather than real traffic.
        const traces = await api.listTraces({ policy: policyName, limit: 1, source: 'request' });
        if (cancelled) return;
        if (traces.length === 0) {
          setAvailability('no-trace');
          setSuggestions(namesOnly());
          return;
        }

        const trace = await api.getTrace(traces[0].id);
        if (cancelled) return;
        // Defensive re-check: the server-side filter should guarantee this,
        // but never preview a foreign policy's trace if it somehow doesn't.
        if (trace.policy !== policyName) {
          setAvailability('no-trace');
          setSuggestions(namesOnly());
          return;
        }
        const snapshot = predecessorSnapshot(trace, predecessorId);
        if (!snapshot) {
          setAvailability('no-trace');
          setSuggestions(namesOnly());
          return;
        }

        const stepIndex =
          predecessorId === null ? -1 : trace.steps.findIndex((s) => s.node_id === predecessorId);
        const requestBodyText = bodyText(trace, stepIndex, 'request');
        const responseBodyText = bodyText(trace, stepIndex, 'response');
        setAvailability('ok');
        setSuggestions([
          ...buildSuggestions(entries, snapshot, captureBodies, requestBodyText, responseBodyText),
          ...env,
        ]);
      } catch {
        // Network errors / 404s (stale trace id, gateway restarted, ...)
        // degrade to "no trace" silently rather than surfacing an error UI
        // for what is a best-effort preview feature.
        if (!cancelled) {
          setAvailability('no-trace');
          setSuggestions(namesOnly());
        }
      }
    }

    run();
    return () => {
      cancelled = true;
    };
  }, [policyName, nodeId, predecessorId, kind, debugEnabled, captureBodies]);

  return { suggestions, availability, catalog };
}
