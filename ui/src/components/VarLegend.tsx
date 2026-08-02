/**
 * Full reference dialog for `$var` interpolation — every name/family the
 * resolver accepts (`src/vars/mod.rs`, mirrored by `GET /api/vars`), plus
 * live preview values when a trace is available for the selected node.
 *
 * Opened from the NodeInspector header's "Context vars" button and from the
 * `VarInput` popover footer's "Context vars reference" link (both wire the
 * same `onOpenLegend` callback back to this dialog's `open` state).
 *
 * @module components/VarLegend
 */
import { Dialog, DialogButton } from './Dialog';
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

/** Exact copy for each non-`ok` {@link Availability}, matching VarInput's popover footer. */
const AVAILABILITY_MESSAGE: Record<Exclude<Availability, 'ok'>, string> = {
  'debug-off': 'Debug is off — enable debug.enabled for live values.',
  'no-incoming-edge': 'No incoming edge — connect this node to preview values.',
  'no-trace': 'No trace yet — send a request through this route.',
  'supernode-definition': 'Live values unavailable while editing a supernode definition.',
};

/** Static catalog names shown under the Response table (everything else static defaults to Request). */
const RESPONSE_STATIC = new Set(['status', 'resp_body']);
/** Static catalog names shown under the Message & consumer table. */
const MESSAGE_STATIC = new Set(['consumer_name', 'consumer_group_id']);

function staticGroupLabel(name: string): 'Request' | 'Response' | 'Message & consumer' {
  if (RESPONSE_STATIC.has(name)) return 'Response';
  if (MESSAGE_STATIC.has(name)) return 'Message & consumer';
  return 'Request';
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

const exampleCellStyle: React.CSSProperties = {
  ...tdStyle,
  fontFamily: 'var(--font-mono)',
  color: 'var(--text-secondary)',
  whiteSpace: 'nowrap',
};

/**
 * Renders the live-value cell for one catalog row, mirroring VarInput's
 * `ValueCell` treatment: a literal `<redacted>` value reads as dimmed
 * italic (not as if it were the real value), an empty string reads as
 * `(empty)`, a `note` (no value at all) is muted italic, and anything else
 * is a plain dimmed preview.
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

/** One catalog-driven table: name / description / example, plus the value column when live. */
function VarTable({
  entries,
  suggestionByName,
  showValue,
}: {
  entries: VarEntry[];
  suggestionByName: Map<string, Suggestion>;
  showValue: boolean;
}) {
  if (entries.length === 0) return null;
  return (
    <table style={tableStyle}>
      <thead>
        <tr>
          <th style={thStyle}>Name</th>
          <th style={thStyle}>Description</th>
          <th style={thStyle}>Example</th>
          {showValue && <th style={thStyle}>Live value</th>}
        </tr>
      </thead>
      <tbody>
        {entries.map((entry) => (
          <tr key={entry.name}>
            <td style={nameCellStyle}>{entry.name}</td>
            <td style={tdStyle}>{entry.description}</td>
            <td style={exampleCellStyle}>{entry.example}</td>
            {showValue && <ValueCell suggestion={suggestionByName.get(entry.name)} />}
          </tr>
        ))}
      </tbody>
    </table>
  );
}

/**
 * Var-legend reference dialog.
 *
 * All table content is derived from `catalog` (never hardcoded), grouped
 * into Request / Response / Message & consumer (statics) and Families
 * (every `kind: 'family'` entry, e.g. `http_*`, `arg_*`, `msg_*`). When
 * `availability` is `ok`, an extra "Live value" column is added, populated
 * by looking up each row's name in `suggestions` — family rows that
 * expanded into concrete members (e.g. `http_user_agent`) during
 * suggestion-building won't match their catalog `prefix_*` name and so show
 * no value here; their live values surface in the field popover itself
 * instead. When `catalog` is empty (the `GET /api/vars` fetch failed, or
 * hasn't resolved), the dialog shows a one-line "unavailable" notice
 * instead of empty tables.
 */
export function VarLegend({ open, onClose, catalog, suggestions, availability }: VarLegendProps) {
  if (!open) return null;

  const suggestionByName = new Map(suggestions.map((s) => [s.name, s]));
  const showValue = availability === 'ok';

  const staticEntries = catalog.filter((e) => e.kind === 'static');
  const familyEntries = catalog.filter((e) => e.kind === 'family');
  const requestEntries = staticEntries.filter((e) => staticGroupLabel(e.name) === 'Request');
  const responseEntries = staticEntries.filter((e) => staticGroupLabel(e.name) === 'Response');
  const messageEntries = staticEntries.filter((e) => staticGroupLabel(e.name) === 'Message & consumer');

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
          Reference a value with <code>$name</code> (e.g. <code>$uri</code>); names containing a dot
          — such as message keys like <code>$msg_consumer.name</code> — need the brace form{' '}
          <code>{'${name}'}</code> instead (e.g. <code>{'${msg_consumer.name}'}</code>). An unknown
          name resolves to an empty string.
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

            <h4 style={sectionTitleStyle}>Request</h4>
            <VarTable entries={requestEntries} suggestionByName={suggestionByName} showValue={showValue} />

            <h4 style={sectionTitleStyle}>Response</h4>
            <VarTable entries={responseEntries} suggestionByName={suggestionByName} showValue={showValue} />

            <h4 style={sectionTitleStyle}>Message &amp; consumer</h4>
            <VarTable entries={messageEntries} suggestionByName={suggestionByName} showValue={showValue} />

            <h4 style={sectionTitleStyle}>Families</h4>
            <VarTable entries={familyEntries} suggestionByName={suggestionByName} showValue={showValue} />
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
            <li>Auth/cookie/token headers (e.g. Authorization, Cookie, Set-Cookie, X-Api-Key) show
              {' '}<code>&lt;redacted&gt;</code> instead of their real value.</li>
            <li><code>$cookie_*</code> values are redacted at capture time and are never previewable.</li>
            <li><code>$request_body</code> / <code>$resp_body</code> only preview when
              {' '}<code>debug.capture_bodies</code> is enabled.</li>
            <li>Values shown here are previews (whitespace collapsed, truncated to ~80 chars) from
              the most recent request handled by this policy — not the current one.</li>
            <li>The autocomplete popover only attaches to schema-form fields. Some plugins accept
              interpolated config as raw JSON/YAML (e.g. a logger's <code>log_format</code>) where
              these vars still resolve at request time, but no popover is available while editing them.</li>
          </ul>
        </div>
      </div>
    </Dialog>
  );
}
