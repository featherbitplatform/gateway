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

/** One row offered by the autocomplete popover / var legend. */
export interface Suggestion {
  /** Bare variable name, e.g. `uri`, `http_user_agent`. */
  name: string;
  /** Text to insert at the cursor, e.g. `$uri` or `${msg_consumer.name}`. */
  insert: string;
  /** Section this row belongs to: `request` | `response` | `message`. */
  group: string;
  /** One-line human-readable explanation, from the catalog entry. */
  description?: string;
  /** Live preview value from the most recent trace, when available. */
  value?: string;
  /** Explains why `value` is absent (e.g. redacted, not captured, empty). */
  note?: string;
}

const REQUEST = 'request';
const RESPONSE = 'response';
const MESSAGE = 'message';

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

/** Rebuilds `k=v&k=v...` sorted by key, matching `query_string()` in src/vars/mod.rs. */
function rebuildQueryString(params: Record<string, string[]>): string {
  const pairs: string[] = [];
  for (const k of Object.keys(params).sort()) {
    for (const v of params[k]) pairs.push(`${k}=${v}`);
  }
  return pairs.join('&');
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

function bodySuggestion(
  base: Suggestion,
  text: string | null | undefined,
  capturedBodies: boolean,
): Suggestion {
  if (text !== null && text !== undefined) return { ...base, value: previewValue(text) };
  return { ...base, note: capturedBodies ? 'no body captured yet' : 'enable debug.capture_bodies' };
}

function messageValueSuggestion(
  base: Suggestion,
  snapshot: ContextSnapshot,
  key: string,
): Suggestion {
  const v = snapshot.message[key];
  if (v === undefined) return base;
  return { ...base, value: previewValue(stringifyMessageValue(v)) };
}

function staticSuggestion(
  entry: VarEntry,
  snapshot: ContextSnapshot | null,
  capturedBodies: boolean,
  requestBodyText: string | null | undefined,
  responseBodyText: string | null | undefined,
): Suggestion {
  const base: Suggestion = {
    name: entry.name,
    insert: insertionText(entry.name),
    group: STATIC_GROUPS[entry.name] ?? REQUEST,
    description: entry.description,
  };
  if (!snapshot) return base;

  switch (entry.name) {
    case 'uri':
      return { ...base, value: previewValue(snapshot.request.path) };
    case 'request_uri': {
      const qs = rebuildQueryString(snapshot.request.query_params);
      const value = qs ? `${snapshot.request.path}?${qs}` : snapshot.request.path;
      return { ...base, value: previewValue(value) };
    }
    case 'method':
    case 'request_method':
      return { ...base, value: previewValue(snapshot.request.method) };
    case 'host':
      return { ...base, value: previewValue(snapshot.request.host) };
    case 'scheme':
      return { ...base, value: previewValue(snapshot.request.scheme) };
    case 'status':
      return { ...base, value: previewValue(String(snapshot.response.status_code)) };
    case 'query_string': {
      const qs = rebuildQueryString(snapshot.request.query_params);
      return qs ? { ...base, value: previewValue(qs) } : { ...base, note: 'empty' };
    }
    case 'consumer_name':
      return messageValueSuggestion(base, snapshot, 'consumer.name');
    case 'consumer_group_id':
      return messageValueSuggestion(base, snapshot, 'consumer.group');
    case 'resp_body':
      return bodySuggestion(base, responseBodyText, capturedBodies);
    case 'request_body':
      return bodySuggestion(base, requestBodyText, capturedBodies);
    case 'protocol':
    case 'remote_addr':
    case 'remote_port':
      return { ...base, note: 'not captured in traces' };
    default:
      return base;
  }
}

function familyRow(name: string, group: string, description: string, note?: string): Suggestion {
  return { name, insert: insertionText(name), group, description, ...(note ? { note } : {}) };
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
    const value = headers![h][0];
    return {
      name: varName,
      insert: insertionText(varName),
      group,
      description,
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
    return { name: varName, insert: insertionText(varName), group, description, value: previewValue(v) };
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
  for (const entry of catalog) {
    if (entry.kind === 'static') {
      out.push(staticSuggestion(entry, snapshot, capturedBodies, requestBodyText, responseBodyText));
    } else {
      out.push(...familySuggestions(entry, snapshot, requestBodyText));
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

/**
 * Resolves the live `$var` suggestions for one node in the editor.
 *
 * See the module doc comment for the full data flow and the
 * `predecessorId` undefined-vs-null encoding. Re-fetches whenever any input
 * changes (in particular `nodeId`, when the selection moves).
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

    function namesOnly(entries: VarEntry[]): Suggestion[] {
      return buildSuggestions(entries, null, false);
    }

    async function run() {
      const entries = await loadCatalog().catch(() => [] as VarEntry[]);
      if (cancelled) return;
      setCatalog(entries);

      if (kind === 'supernode') {
        setAvailability('supernode-definition');
        setSuggestions(namesOnly(entries));
        return;
      }
      if (!debugEnabled) {
        setAvailability('debug-off');
        setSuggestions(namesOnly(entries));
        return;
      }
      if (predecessorId === undefined) {
        setAvailability('no-incoming-edge');
        setSuggestions(namesOnly(entries));
        return;
      }

      try {
        const traces = await api.listTraces({ policy: policyName ?? undefined, limit: 1 });
        if (cancelled) return;
        if (traces.length === 0) {
          setAvailability('no-trace');
          setSuggestions(namesOnly(entries));
          return;
        }

        const trace = await api.getTrace(traces[0].id);
        if (cancelled) return;
        const snapshot = predecessorSnapshot(trace, predecessorId);
        if (!snapshot) {
          setAvailability('no-trace');
          setSuggestions(namesOnly(entries));
          return;
        }

        const stepIndex =
          predecessorId === null ? -1 : trace.steps.findIndex((s) => s.node_id === predecessorId);
        const requestBodyText = bodyText(trace, stepIndex, 'request');
        const responseBodyText = bodyText(trace, stepIndex, 'response');
        setAvailability('ok');
        setSuggestions(
          buildSuggestions(entries, snapshot, captureBodies, requestBodyText, responseBodyText),
        );
      } catch {
        // Network errors / 404s (stale trace id, gateway restarted, ...)
        // degrade to "no trace" silently rather than surfacing an error UI
        // for what is a best-effort preview feature.
        if (!cancelled) {
          setAvailability('no-trace');
          setSuggestions(namesOnly(entries));
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
