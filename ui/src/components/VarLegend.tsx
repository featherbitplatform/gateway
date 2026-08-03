/**
 * Full reference dialog for `{{path}}` / legacy `$var` interpolation — every
 * namespace path and family the template engine and the legacy resolver
 * accept (`src/vars/catalog.rs`, mirrored by `GET /api/vars`), plus live
 * preview values when a trace is available for the selected node.
 *
 * Opened from the NodeInspector header's "Context vars" button and from the
 * `VarInput` popover footer's "Context vars reference" link (both wire the
 * same `onOpenLegend` callback back to this dialog's `open` state).
 *
 * @module components/VarLegend
 */
import { Fragment } from 'react';
import { Dialog, DialogButton } from './Dialog';
import { AVAILABILITY_MESSAGE } from '../varSuggestions';
import type { Availability, Suggestion } from '../varSuggestions';
import type { VarEntry } from '../types';

/** Props for {@link VarLegend}. */
interface VarLegendProps {
  /** Whether the dialog is shown. */
  open: boolean;
  /** Invoked on backdrop click or the Close button. */
  onClose: () => void;
  /** Full var catalog from `GET /api/vars` (empty when it failed to load). */
  catalog: VarEntry[];
  /** Live suggestions for the currently selected node, keyed by name for the value column. */
  suggestions: Suggestion[];
  /** Why live values may be unavailable; only `ok` renders the value column. */
  availability: Availability;
}

/**
 * Namespace bucket for a template path (`request.method` -> `'request'`),
 * matching `src/vars/template.rs`'s four namespaces. `null` for anything
 * else (shouldn't happen for a non-empty catalog `path`, but keeps this
 * total rather than throwing on drift).
 */
type Namespace = 'request' | 'response' | 'message' | 'client';

function pathNamespace(path: string): Namespace | null {
  const ns = path.split('.', 1)[0];
  if (ns === 'request' || ns === 'response' || ns === 'message' || ns === 'client') return ns;
  return null;
}

const sectionTitleStyle: React.CSSProperties = {
  fontSize: 'var(--text-xs)',
  fontWeight: 600,
  color: 'var(--text-primary)',
  margin: '16px 0 6px',
};

const tableStyle: React.CSSProperties = {
  width: '100%',
  borderCollapse: 'collapse',
  fontSize: 'var(--text-2xs)',
};

const thStyle: React.CSSProperties = {
  textAlign: 'left',
  padding: '4px 6px',
  color: 'var(--text-muted)',
  fontWeight: 500,
  borderBottom: '1px solid var(--border-subtle)',
};

const tdStyle: React.CSSProperties = {
  padding: '4px 6px',
  borderBottom: '1px solid var(--border-subtle)',
  verticalAlign: 'top',
};

const nameCellStyle: React.CSSProperties = {
  ...tdStyle,
  fontFamily: 'var(--font-mono)',
  color: 'var(--text-primary)',
  whiteSpace: 'nowrap',
};

/** Compact, indented mono row for a family's discovered member (e.g. `request.headers.x-trace-id` under `request.headers.*`). */
const memberNameCellStyle: React.CSSProperties = {
  ...nameCellStyle,
  paddingLeft: 20,
  color: 'var(--text-muted)',
};

const exampleCellStyle: React.CSSProperties = {
  ...tdStyle,
  fontFamily: 'var(--font-mono)',
  color: 'var(--text-secondary)',
  whiteSpace: 'nowrap',
};

const legacyOnlyCellStyle: React.CSSProperties = {
  ...tdStyle,
  color: 'var(--text-muted)',
  fontStyle: 'italic',
  whiteSpace: 'nowrap',
};

/**
 * Renders the live-value cell for one row, mirroring VarInput's `ValueCell`
 * treatment: a literal `<redacted>` value reads as dimmed italic (not as if
 * it were the real value), an empty string reads as `(empty)`, a `note` (no
 * value at all) is muted italic, and anything else is a plain dimmed
 * preview.
 */
function ValueCell({ suggestion }: { suggestion: Suggestion | undefined }) {
  if (!suggestion) return <td style={tdStyle} />;
  if (suggestion.value === '<redacted>') {
    return (
      <td style={{ ...tdStyle, color: 'var(--text-muted)', fontStyle: 'italic' }}>redacted</td>
    );
  }
  if (suggestion.value === '') {
    return (
      <td style={{ ...tdStyle, color: 'var(--text-muted)', fontStyle: 'italic' }}>(empty)</td>
    );
  }
  if (suggestion.value !== undefined) {
    return <td style={{ ...tdStyle, color: 'var(--text-muted)' }}>{suggestion.value}</td>;
  }
  if (suggestion.note) {
    return (
      <td style={{ ...tdStyle, color: 'var(--text-muted)', fontStyle: 'italic' }}>
        {suggestion.note}
      </td>
    );
  }
  return <td style={tdStyle} />;
}

/**
 * First-wins de-dupe by `path` — two legacy catalog entries can map to the
 * same template path (e.g. `uri`/`request_uri` both -> `request.path`),
 * which would otherwise surface as two identical rows in a path-keyed
 * table. The legacy mapping table at the bottom still lists both names
 * individually; only the new path-keyed sections collapse them.
 */
function dedupeByPath(entries: VarEntry[]): VarEntry[] {
  const seen = new Set<string>();
  const out: VarEntry[] = [];
  for (const entry of entries) {
    if (seen.has(entry.path)) continue;
    seen.add(entry.path);
    out.push(entry);
  }
  return out;
}

/** One row per fixed template path: path / description / `{{path}}` example, plus live value when available. */
function PathStaticTable({
  entries,
  suggestionByPath,
  showValue,
}: {
  entries: VarEntry[];
  suggestionByPath: Map<string, Suggestion>;
  showValue: boolean;
}) {
  if (entries.length === 0) return null;
  return (
    <table style={tableStyle}>
      <thead>
        <tr>
          <th style={thStyle}>Path</th>
          <th style={thStyle}>Description</th>
          <th style={thStyle}>Example</th>
          {showValue && <th style={thStyle}>Live value</th>}
        </tr>
      </thead>
      <tbody>
        {entries.map((entry) => (
          <tr key={entry.path}>
            <td style={nameCellStyle}>{entry.path}</td>
            <td style={tdStyle}>{entry.description}</td>
            <td style={exampleCellStyle}>{`{{${entry.path}}}`}</td>
            {showValue && <ValueCell suggestion={suggestionByPath.get(entry.path)} />}
          </tr>
        ))}
      </tbody>
    </table>
  );
}

/** Strips the trailing `*` off a family path (`request.headers.*` -> `request.headers.`). */
function pathFamilyPrefix(path: string): string {
  return path.endsWith('*') ? path.slice(0, -1) : path;
}

/**
 * Suggestions discovered for one family row's path, e.g. the concrete
 * `{{request.headers.x-trace-id}}` rows `buildSuggestions` expanded
 * `request.headers.*` into once a trace snapshot was available. Matches by
 * path prefix against `familyEntry`, excluding: the family's own unexpanded
 * path (never actually emitted as a row today, but excluded defensively),
 * and any suggestion better claimed by a *longer* family prefix also
 * present in the catalog (longest-prefix wins) — mirrors the legacy-name
 * version of this guard that used to live here, now applied to paths;
 * today's four namespaces never actually collide this way since each
 * family lives under a distinct dotted prefix, but the guard costs nothing.
 */
function pathFamilyMembers(
  familyEntry: VarEntry,
  allFamilies: VarEntry[],
  suggestions: Suggestion[],
): Suggestion[] {
  const prefix = pathFamilyPrefix(familyEntry.path);
  const longerPrefixes = allFamilies
    .map((f) => pathFamilyPrefix(f.path))
    .filter((p) => p.length > prefix.length);
  return suggestions.filter((s) => {
    if (s.trigger !== 'brace') return false;
    if (s.name === familyEntry.path) return false;
    if (!s.name.startsWith(prefix)) return false;
    return !longerPrefixes.some((p) => s.name.startsWith(p));
  });
}

/**
 * Families table, path-keyed: one row per catalog family entry's `path`,
 * plus — when a snapshot is available (`showValue`) — indented sub-rows for
 * each concrete member `buildSuggestions` discovered for it as a `{{...}}`
 * path (e.g. `request.headers.x-trace-id` under `request.headers.*`),
 * mirroring what the field popover itself would show for a `{{` token.
 */
function PathFamilyTable({
  entries,
  allFamilies,
  suggestionByPath,
  suggestions,
  showValue,
}: {
  entries: VarEntry[];
  allFamilies: VarEntry[];
  suggestionByPath: Map<string, Suggestion>;
  suggestions: Suggestion[];
  showValue: boolean;
}) {
  if (entries.length === 0) return null;
  return (
    <table style={tableStyle}>
      <thead>
        <tr>
          <th style={thStyle}>Path</th>
          <th style={thStyle}>Description</th>
          <th style={thStyle}>Example</th>
          {showValue && <th style={thStyle}>Live value</th>}
        </tr>
      </thead>
      <tbody>
        {entries.map((entry) => {
          const members = showValue ? pathFamilyMembers(entry, allFamilies, suggestions) : [];
          return (
            <Fragment key={entry.path}>
              <tr>
                <td style={nameCellStyle}>{entry.path}</td>
                <td style={tdStyle}>{entry.description}</td>
                <td style={exampleCellStyle}>{`{{${pathFamilyPrefix(entry.path)}<name>}}`}</td>
                {showValue && <ValueCell suggestion={suggestionByPath.get(entry.path)} />}
              </tr>
              {members.map((m) => (
                <tr key={`${entry.path}:${m.name}`}>
                  <td style={memberNameCellStyle}>{m.name}</td>
                  <td style={tdStyle} />
                  <td style={exampleCellStyle} />
                  <ValueCell suggestion={m} />
                </tr>
              ))}
            </Fragment>
          );
        })}
      </tbody>
    </table>
  );
}

/**
 * Environment section: one chip per `{{env.NAME}}` name discovered via
 * `GET /api/env-vars` (fed into `suggestions` — filtered here by `group ===
 * 'env'` — by `useContextSuggestions`, module-cached the same way as the
 * catalog). Names only, never values: env-var values are never fetched or
 * shown, by design (see varSuggestions.ts).
 */
function EnvList({ entries }: { entries: Suggestion[] }) {
  if (entries.length === 0) {
    return (
      <p style={{ fontSize: 'var(--text-2xs)', color: 'var(--text-muted)', fontStyle: 'italic', margin: 0 }}>
        No environment variables detected in the gateway process.
      </p>
    );
  }
  return (
    <div style={{ display: 'flex', flexWrap: 'wrap', gap: 6 }}>
      {entries.map((entry) => (
        <code
          key={entry.name}
          style={{
            fontSize: 'var(--text-2xs)',
            fontFamily: 'var(--font-mono)',
            padding: '2px 6px',
            borderRadius: 'var(--radius-sm)',
            background: 'var(--surface-sunken)',
            border: '1px solid var(--border-subtle)',
            color: 'var(--text-primary)',
          }}
        >
          {entry.name}
        </code>
      ))}
    </div>
  );
}

/**
 * Legacy `$var` mapping table: every catalog entry's legacy name, its
 * `$`/`${}` example, and the new template path it maps to — or "legacy
 * only" (italic, muted) for the handful of names with no direct `{{...}}`
 * equivalent (`protocol`, `query_string`, `post_arg_*`). No live-value
 * column here; live values are shown once, in the path-keyed sections
 * above, keyed by the new paths.
 */
function LegacyTable({ catalog }: { catalog: VarEntry[] }) {
  if (catalog.length === 0) return null;
  return (
    <table style={tableStyle}>
      <thead>
        <tr>
          <th style={thStyle}>Legacy name</th>
          <th style={thStyle}>Example</th>
          <th style={thStyle}>Template path</th>
          <th style={thStyle}>Description</th>
        </tr>
      </thead>
      <tbody>
        {catalog.map((entry) => (
          <tr key={entry.name}>
            <td style={nameCellStyle}>{entry.name}</td>
            <td style={exampleCellStyle}>{entry.example}</td>
            <td style={entry.path ? exampleCellStyle : legacyOnlyCellStyle}>
              {entry.path || 'legacy only'}
            </td>
            <td style={tdStyle}>{entry.description}</td>
          </tr>
        ))}
      </tbody>
    </table>
  );
}

/**
 * Renders one namespace section (title + statics table + families table).
 * A plain helper function rather than a component defined inside
 * {@link VarLegend} — hoisting it out (and calling it, not tagging it as
 * JSX) sidesteps `react-hooks/static-components`, which flags components
 * declared during render since they'd reset state on every re-render; this
 * has no state of its own, but the lint rule can't tell that from a nested
 * function declaration alone.
 */
function renderNamespaceSection(
  title: string,
  ns: Namespace,
  staticPathEntries: VarEntry[],
  familyPathEntries: VarEntry[],
  suggestionByPath: Map<string, Suggestion>,
  suggestions: Suggestion[],
  showValue: boolean,
) {
  return (
    <Fragment key={ns}>
      <h4 style={sectionTitleStyle}>{title}</h4>
      <PathStaticTable
        entries={staticPathEntries.filter((e) => pathNamespace(e.path) === ns)}
        suggestionByPath={suggestionByPath}
        showValue={showValue}
      />
      <PathFamilyTable
        entries={familyPathEntries.filter((e) => pathNamespace(e.path) === ns)}
        allFamilies={familyPathEntries}
        suggestionByPath={suggestionByPath}
        suggestions={suggestions}
        showValue={showValue}
      />
    </Fragment>
  );
}

/**
 * Var-legend reference dialog.
 *
 * Sections: Request / Response / Message & consumer / Client — each built
 * from `catalog` entries with a non-empty `path`, bucketed by the path's
 * leading namespace segment ({@link pathNamespace}), with family entries
 * (`http_*`, `arg_*`, ...) expanding into indented `{{...}}` sub-rows for
 * every concrete member `suggestions` discovered once a trace snapshot is
 * available (`availability === 'ok'`) — see {@link PathFamilyTable} /
 * {@link pathFamilyMembers}. Then Environment (names only, from the `env`
 * suggestion group). Then a flat Legacy `$var` mapping table covering every
 * catalog entry by its old name, for looking up what an existing `$foo` in
 * a policy maps to. When `catalog` is empty (the `GET /api/vars` fetch
 * failed, or hasn't resolved), the dialog shows a one-line "unavailable"
 * notice instead of empty tables.
 */
export function VarLegend({ open, onClose, catalog, suggestions, availability }: VarLegendProps) {
  if (!open) return null;

  const showValue = availability === 'ok';

  const braceSuggestionByPath = new Map(
    suggestions.filter((s) => s.trigger === 'brace').map((s) => [s.name, s]),
  );

  const staticPathEntries = dedupeByPath(catalog.filter((e) => e.kind === 'static' && e.path !== ''));
  const familyPathEntries = catalog.filter((e) => e.kind === 'family' && e.path !== '');
  const envEntries = suggestions.filter((s) => s.group === 'env');

  return (
    <Dialog
      open={open}
      title="Context vars reference"
      onClose={onClose}
      width={640}
      footer={
        <DialogButton variant="ghost" onClick={onClose}>
          Close
        </DialogButton>
      }
    >
      <div data-testid="var-legend" style={{ maxHeight: '70vh', overflowY: 'auto' }}>
        <p style={{ fontSize: 'var(--text-xs)', color: 'var(--text-secondary)', margin: '0 0 8px' }}>
          Reference a field with <code>{'{{path}}'}</code> (e.g. <code>{'{{request.method}}'}</code>),
          or an environment variable with <code>{'{{env.NAME}}'}</code>. Older policies may still use
          the legacy <code>$name</code> (e.g. <code>$uri</code>) or, for names containing a dot such as
          message keys, <code>{'${name}'}</code> form — see the mapping table at the bottom for the
          template path each legacy name corresponds to.
        </p>

        {catalog.length === 0 ? (
          <p style={{ fontSize: 'var(--text-xs)', color: 'var(--text-muted)', fontStyle: 'italic' }}>
            Variable catalog unavailable — GET /api/vars did not return any entries (the gateway may
            be unreachable, or the request is still in flight).
          </p>
        ) : (
          <>
            {availability !== 'ok' && (
              <p style={{ fontSize: 'var(--text-2xs)', color: 'var(--text-muted)', margin: '0 0 8px' }}>
                {AVAILABILITY_MESSAGE[availability]}
              </p>
            )}

            {renderNamespaceSection(
              'Request',
              'request',
              staticPathEntries,
              familyPathEntries,
              braceSuggestionByPath,
              suggestions,
              showValue,
            )}
            {renderNamespaceSection(
              'Response',
              'response',
              staticPathEntries,
              familyPathEntries,
              braceSuggestionByPath,
              suggestions,
              showValue,
            )}
            {renderNamespaceSection(
              'Message & consumer',
              'message',
              staticPathEntries,
              familyPathEntries,
              braceSuggestionByPath,
              suggestions,
              showValue,
            )}
            {renderNamespaceSection(
              'Client',
              'client',
              staticPathEntries,
              familyPathEntries,
              braceSuggestionByPath,
              suggestions,
              showValue,
            )}

            <h4 style={sectionTitleStyle}>{`Environment (${envEntries.length})`}</h4>
            <EnvList entries={envEntries} />

            <h4 style={sectionTitleStyle}>Legacy $var mapping</h4>
            <LegacyTable catalog={catalog} />
          </>
        )}

        <div
          style={{
            marginTop: 16,
            padding: '8px 10px',
            borderRadius: 'var(--radius-sm)',
            background: 'var(--surface-sunken)',
            border: '1px solid var(--border-subtle)',
            fontSize: 'var(--text-2xs)',
            color: 'var(--text-muted)',
          }}
        >
          <p style={{ margin: '0 0 4px', fontWeight: 500, color: 'var(--text-secondary)' }}>Caveats</p>
          <ul style={{ margin: 0, paddingLeft: 16 }}>
            <li>An unrecognized <code>{'{{...}}'}</code> — unknown namespace, a typo, or an unclosed
              brace — passes through as a literal, unchanged; this differs from an unrecognized legacy
              <code> $name</code>, which resolves to an empty string instead.</li>
            <li>Malformed <code>{'{{...}}'}</code> references and unset <code>{'{{env.NAME}}'}</code>{' '}
              lookups are logged as warnings when the gateway loads its config — not shown live in this
              dialog, so check the gateway log after a reload if a template isn&apos;t resolving as
              expected.</li>
            <li><code>$name</code> / <code>{'${name}'}</code> completion is only offered on the
              handful of fields still flagged legacy; everywhere else in the node inspector only the
              <code> {'{{path}}'}</code> syntax is available.</li>
            <li>Text fields the engine doesn&apos;t render still open the popover on{' '}
              <code>{'{{'}</code>, but only offer the <code>env</code> group there, and picking one
              inserts <code>{'${NAME}'}</code> instead of <code>{'{{env.NAME}}'}</code> — the field is
              never parsed as a <code>Template</code>, so only the universal{' '}
              <code>{'${NAME}'}</code>/<code>{'${NAME:-default}'}</code> environment substitution
              (applied to every config field, templated or not) actually resolves there. Dropdowns,
              switches, and numeric fields offer no completion at all. Context suggestions
              (request/response/message/client) appear only in fields the engine actually templates.</li>
            <li>Values shown here are previews (whitespace collapsed, truncated to ~80 chars) from
              the most recent request handled by this policy — not the current one.</li>
            <li>Auth/cookie/token headers (e.g. Authorization, Cookie, Set-Cookie, X-Api-Key) show
              {' '}<code>&lt;redacted&gt;</code> instead of their real value.</li>
            <li><code>request.cookies.*</code> (legacy <code>$cookie_*</code>) values are redacted at
              capture time and are never previewable.</li>
            <li><code>request.body</code> / <code>response.body</code> (legacy <code>$request_body</code>
              {' '}/ <code>$resp_body</code>) only preview when <code>debug.capture_bodies</code> is
              enabled.</li>
            <li>The autocomplete popover only attaches to schema-form fields. Some plugins accept
              interpolated config as raw JSON/YAML (e.g. a logger's <code>log_format</code>) where
              these vars still resolve at request time, but no popover is available while editing them.</li>
          </ul>
        </div>
      </div>
    </Dialog>
  );
}
