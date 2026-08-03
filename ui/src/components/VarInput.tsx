/**
 * Controlled `<input>`/`<textarea>` with a `{{path}}` (and, on opted-in
 * fields, legacy `$var`) autocomplete popover.
 *
 * Detects an in-progress `{{path` token — or, when `legacyDollar` is true,
 * also a `$name` / `${name` token — at the caret and offers matching
 * {@link Suggestion} rows (name + trace-derived live preview, from
 * {@link useContextSuggestions} in varSuggestions.ts) to complete it. This
 * component only owns token detection, filtering, keyboard navigation, and
 * insertion — the suggestion list, live values, and availability messaging
 * are all supplied by the caller (SchemaForm, via its `varContext` prop).
 *
 * @module components/VarInput
 */
import { useEffect, useRef } from 'react';
import { useTemplateToken } from '../templateToken';
import type { Availability, Suggestion } from '../varSuggestions';

/** Props for VarInput. */
interface VarInputProps {
  /** Current field value (controlled). */
  value: string;
  /** Called with the full replacement value on every edit and on suggestion insert. */
  onChange: (v: string) => void;
  /** Placeholder text for the empty field. */
  placeholder?: string;
  /** Render a `<textarea>` instead of a single-line `<input>`. */
  multiline?: boolean;
  /** Visible rows when `multiline`; ignored otherwise (defaults to 4). */
  rows?: number;
  /** Candidate rows for the popover; filtered here by the in-progress token. */
  suggestions: Suggestion[];
  /** Why live `value`s may be missing from `suggestions`; drives the footer message. */
  availability: Availability;
  /** Opens the full context-vars legend/reference dialog. */
  onOpenLegend: () => void;
  /**
   * Whether this field also accepts legacy `$name`/`${name}` completions.
   * `false` (default) means only `{{path}}` tokens open the popover; a `$`
   * or `${` at the caret is left untouched (no popover, no gating of typing).
   * The 15 legacy-`$` fields (Task 2) pass `true` so their existing
   * `$`-completion behavior keeps working unchanged.
   */
  legacyDollar?: boolean;
  /**
   * Which suggestion groups this field offers, on top of the `trigger`
   * filter: `'full'` (default) offers every group; `'env-only'` restricts
   * the popover to the `env` group (for fields that may only reference
   * environment variables, not request/response/message context — see
   * Task 9, which decides which fields pass this). `'none'` fields are not
   * expected to render `VarInput` at all; it's accepted here only so a
   * caller can pass a field's mode through without a conditional, and is
   * treated the same as `'full'` if one ever does render with it.
   *
   * Also decides *which form* an `env`-group suggestion inserts (see
   * `useTemplateToken`'s `insert` and `Suggestion.insertEnvOnly` in
   * varSuggestions.ts):
   * `'env-only'` fields are never parsed into a `Template` by the Rust
   * plugin, so `{{env.NAME}}` — which only `Template::parse` resolves —
   * would sit there unresolved forever; only the universal `${NAME}` env
   * pass (`interpolate_env_json`, applied to every field's config
   * regardless of templating) actually works there. `'full'` fields insert
   * `{{env.NAME}}` as before.
   */
  templateMode?: 'full' | 'env-only' | 'none';
  /** Spread onto the input/textarea element (callers pass their shared field style). */
  style?: React.CSSProperties;
}

/** Exact copy for each non-`ok` {@link Availability}, shown in the popover footer. */
const AVAILABILITY_MESSAGE: Record<Exclude<Availability, 'ok'>, string> = {
  'debug-off': 'Debug is off — enable debug.enabled for live values',
  'no-incoming-edge': 'No incoming edge — connect this node to preview values',
  'no-trace': 'No trace yet — send a request through this route',
  'supernode-definition': 'Live values unavailable while editing a supernode definition',
};

/** Delay before a blur closes the popover, so a row's `click` lands first. */
const BLUR_CLOSE_DELAY_MS = 120;

type FieldEl = HTMLInputElement | HTMLTextAreaElement;

/**
 * Renders the value/note cell of one popover row.
 *
 * Three cases need distinct styling from a normal preview (dimmed, single
 * line): a literal `'<redacted>'` value must not read as if it were the
 * real resolved value (shown muted + italic, same treatment as a `note`);
 * an empty-string value renders as `(empty)` rather than a blank cell; a
 * `note` (no live value at all) is muted + italic. Everything else is a
 * plain dimmed, truncated preview — `previewValue` (varSuggestions.ts)
 * already collapsed whitespace and capped the length upstream.
 */
function ValueCell({ suggestion }: { suggestion: Suggestion }) {
  const cellStyle: React.CSSProperties = {
    fontSize: 'var(--text-2xs)',
    maxWidth: '55%',
    overflow: 'hidden',
    textOverflow: 'ellipsis',
    whiteSpace: 'nowrap',
    flexShrink: 0,
  };
  if (suggestion.value === '<redacted>') {
    return (
      <span style={{ ...cellStyle, color: 'var(--text-muted)', fontStyle: 'italic' }}>redacted</span>
    );
  }
  if (suggestion.value === '') {
    return (
      <span style={{ ...cellStyle, color: 'var(--text-muted)', fontStyle: 'italic' }}>(empty)</span>
    );
  }
  if (suggestion.value !== undefined) {
    return <span style={{ ...cellStyle, color: 'var(--text-muted)' }}>{suggestion.value}</span>;
  }
  if (suggestion.note) {
    return (
      <span style={{ ...cellStyle, color: 'var(--text-muted)', fontStyle: 'italic' }}>
        {suggestion.note}
      </span>
    );
  }
  return null;
}

/**
 * Controlled field + `{{path}}` (and, on opted-in fields, legacy `$var`)
 * autocomplete popover.
 *
 * @remarks Token detection re-runs on every change and every selection
 * change (mouse click, arrow keys) against `value.slice(0, selectionStart)`,
 * per the behavior contract in task-5-brief.md. Insertion replaces the
 * matched token span with `suggestion.insert`, restores focus, and places
 * the caret right after the inserted text.
 */
export function VarInput({
  value,
  onChange,
  placeholder,
  multiline,
  rows,
  suggestions,
  availability,
  onOpenLegend,
  legacyDollar = false,
  templateMode = 'full',
  style,
}: VarInputProps) {
  const elRef = useRef<FieldEl | null>(null);
  const blurTimer = useRef<ReturnType<typeof setTimeout> | null>(null);

  const tokenState = useTemplateToken({
    value,
    onChange,
    elRef,
    suggestions,
    // 'none' fields are never expected to render VarInput at all (see this
    // prop's doc comment above); treated the same as 'full' if one ever
    // does, same as the pre-extraction `templateMode !== 'env-only'` filter.
    templateMode: templateMode === 'env-only' ? 'env-only' : 'full',
    legacyDollar,
  });
  const { open, filtered, activeIndex } = tokenState;

  useEffect(() => {
    return () => {
      if (blurTimer.current) clearTimeout(blurTimer.current);
    };
  }, []);

  function setRef(el: FieldEl | null) {
    elRef.current = el;
  }

  function handleChange(e: React.ChangeEvent<FieldEl>) {
    onChange(e.target.value);
    tokenState.onValueEvent();
  }

  function handleSelect() {
    tokenState.onValueEvent();
  }

  function handleKeyDown(e: React.KeyboardEvent<FieldEl>) {
    tokenState.onKeyDown(e);
  }

  function handleBlur() {
    blurTimer.current = setTimeout(() => tokenState.close(), BLUR_CLOSE_DELAY_MS);
  }

  function handleFocus() {
    if (blurTimer.current) {
      clearTimeout(blurTimer.current);
      blurTimer.current = null;
    }
  }

  const fieldProps = {
    ref: setRef,
    value,
    placeholder,
    onChange: handleChange,
    onSelect: handleSelect,
    onKeyDown: handleKeyDown,
    onBlur: handleBlur,
    onFocus: handleFocus,
    style,
  };

  let previousGroup: string | null = null;

  return (
    <div style={{ position: 'relative' }}>
      {multiline ? (
        <textarea rows={rows ?? 4} className="resize-y" {...fieldProps} />
      ) : (
        <input type="text" {...fieldProps} />
      )}
      {open && (
        <div
          data-testid="var-popover"
          style={{
            position: 'absolute',
            top: '100%',
            left: 0,
            right: 0,
            zIndex: 50,
            maxHeight: 240,
            overflowY: 'auto',
            marginTop: 4,
            background: 'var(--surface-raised)',
            border: '1px solid var(--border)',
            borderRadius: 'var(--radius-sm)',
            boxShadow: 'var(--shadow-md)',
          }}
        >
          {filtered.length === 0 ? (
            <div
              style={{ padding: '8px 10px', fontSize: 'var(--text-xs)', color: 'var(--text-muted)' }}
            >
              No matching variables
            </div>
          ) : (
            filtered.map((s, i) => {
              const showGroupHeader = s.group !== previousGroup;
              previousGroup = s.group;
              return (
                <div key={`${s.group}:${s.name}`}>
                  {showGroupHeader && (
                    <div className="eyebrow" style={{ padding: '6px 10px 2px' }}>
                      {s.group}
                    </div>
                  )}
                  <div
                    onMouseDown={(e) => e.preventDefault()}
                    onClick={() => tokenState.insert(s)}
                    style={{
                      display: 'flex',
                      alignItems: 'center',
                      justifyContent: 'space-between',
                      gap: 8,
                      padding: '5px 10px',
                      cursor: 'pointer',
                      background: i === activeIndex ? 'var(--surface-active)' : 'transparent',
                    }}
                  >
                    <span
                      style={{
                        fontFamily: 'var(--font-mono)',
                        fontSize: 'var(--text-xs)',
                        color: 'var(--text-primary)',
                        whiteSpace: 'nowrap',
                        overflow: 'hidden',
                        textOverflow: 'ellipsis',
                      }}
                    >
                      {s.name}
                    </span>
                    <ValueCell suggestion={s} />
                  </div>
                </div>
              );
            })
          )}
          <div
            style={{
              display: 'flex',
              alignItems: 'center',
              justifyContent: 'space-between',
              gap: 8,
              padding: '6px 10px',
              borderTop: '1px solid var(--border-subtle)',
            }}
          >
            <span style={{ fontSize: 'var(--text-2xs)', color: 'var(--text-muted)' }}>
              {availability !== 'ok' ? AVAILABILITY_MESSAGE[availability] : ''}
            </span>
            <button
              type="button"
              onMouseDown={(e) => e.preventDefault()}
              onClick={onOpenLegend}
              style={{
                fontSize: 'var(--text-2xs)',
                color: 'var(--accent-hover)',
                background: 'transparent',
                border: 'none',
                padding: 0,
                cursor: 'pointer',
                whiteSpace: 'nowrap',
              }}
            >
              Context vars reference
            </button>
          </div>
        </div>
      )}
    </div>
  );
}
