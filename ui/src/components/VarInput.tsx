/**
 * Controlled `<input>`/`<textarea>` with a `$var` autocomplete popover.
 *
 * Detects an in-progress `$name` / `${name` token at the caret and offers
 * matching {@link Suggestion} rows (name + trace-derived live preview, from
 * {@link useContextSuggestions} in varSuggestions.ts) to complete it. This
 * component only owns token detection, filtering, keyboard navigation, and
 * insertion — the suggestion list, live values, and availability messaging
 * are all supplied by the caller (SchemaForm, via its `varContext` prop).
 *
 * @module components/VarInput
 */
import { useEffect, useRef, useState } from 'react';
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

/** Matches an in-progress `$name` or `${name` token ending exactly at the caret. */
const TOKEN_RE = /\$\{?([A-Za-z0-9_.]*)$/;

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
 * Controlled field + `$var` autocomplete popover.
 *
 * @remarks Token detection re-runs on every change and every selection
 * change (mouse click, arrow keys) against `value.slice(0, selectionStart)`,
 * per the behavior contract in task-5-brief.md. Insertion replaces the
 * matched `$...` span with `suggestion.insert`, restores focus, and places
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
  style,
}: VarInputProps) {
  const [open, setOpen] = useState(false);
  const [filter, setFilter] = useState('');
  const [tokenStart, setTokenStart] = useState(0);
  const [activeIndex, setActiveIndex] = useState(0);

  const elRef = useRef<FieldEl | null>(null);
  const blurTimer = useRef<ReturnType<typeof setTimeout> | null>(null);
  // Caret position to apply once the controlled `value` prop reflects an
  // insertion — the DOM's own value only updates after this component
  // re-renders with the new prop, so `setSelectionRange` has to wait for it.
  const pendingCaret = useRef<number | null>(null);
  // One-shot guard: `setSelectionRange` in the caret effect below fires a
  // native `select` event on the field, which would otherwise reach
  // `syncTokenFromCaret` and immediately reopen the popover on the
  // freshly-inserted `$name`. Set right before the insertion's `onChange`,
  // consumed (and cleared) by the very next `syncTokenFromCaret` call —
  // which is that synthetic reopen — so any *later*, real caret movement or
  // typing still opens the popover normally.
  const justInserted = useRef(false);

  const lowerFilter = filter.toLowerCase();
  const filtered = suggestions.filter((s) => s.name.toLowerCase().includes(lowerFilter));

  useEffect(() => {
    const el = elRef.current;
    if (pendingCaret.current !== null && el) {
      const pos = pendingCaret.current;
      el.setSelectionRange(pos, pos);
      pendingCaret.current = null;
    }
  }, [value]);

  useEffect(() => {
    return () => {
      if (blurTimer.current) clearTimeout(blurTimer.current);
    };
  }, []);

  function setRef(el: FieldEl | null) {
    elRef.current = el;
  }

  /** Re-derives the popover's open/filter/token state from the caret position. */
  function syncTokenFromCaret(el: FieldEl) {
    if (justInserted.current) {
      // Swallow exactly the synthetic `select` event fired by this
      // component's own post-insert `setSelectionRange` (see the ref's
      // comment) and nothing after it.
      justInserted.current = false;
      return;
    }
    const pos = el.selectionStart ?? el.value.length;
    const head = el.value.slice(0, pos);
    const match = head.match(TOKEN_RE);
    if (!match) {
      setOpen(false);
      return;
    }
    setTokenStart(pos - match[0].length);
    setFilter(match[1]);
    setActiveIndex(0);
    setOpen(true);
  }

  function insertSuggestion(suggestion: Suggestion | undefined) {
    if (!suggestion) return;
    const el = elRef.current;
    const caret = el?.selectionStart ?? tokenStart;
    // If the in-progress token began with the brace form (`${`) and the
    // character sitting right at the caret is that token's own closing
    // `}`, consume it too — otherwise the inserted text leaves a stray `}`
    // behind (`${foo` + completion + already-typed `}` -> double brace).
    // Gated on the *matched* token's opening delimiter (not on whether this
    // suggestion's own `insert` happens to use braces), so completing a
    // plain `$token` never eats an unrelated, legitimate `}` at the caret.
    const isBraceToken = value[tokenStart] === '$' && value[tokenStart + 1] === '{';
    const spliceEnd = isBraceToken && value[caret] === '}' ? caret + 1 : caret;
    const nextValue = `${value.slice(0, tokenStart)}${suggestion.insert}${value.slice(spliceEnd)}`;
    setOpen(false);
    if (nextValue === value) {
      // No-op insertion (the token was already fully typed out verbatim):
      // `onChange` won't fire, so the `[value]`-keyed caret effect never
      // runs either. Clear the pending caret here instead of leaving it to
      // be misapplied on some later, unrelated value change.
      pendingCaret.current = null;
      return;
    }
    pendingCaret.current = tokenStart + suggestion.insert.length;
    justInserted.current = true;
    onChange(nextValue);
    el?.focus();
  }

  function handleChange(e: React.ChangeEvent<FieldEl>) {
    onChange(e.target.value);
    syncTokenFromCaret(e.target);
  }

  function handleSelect(e: React.SyntheticEvent<FieldEl>) {
    syncTokenFromCaret(e.currentTarget);
  }

  function handleKeyDown(e: React.KeyboardEvent<FieldEl>) {
    if (!open) return;
    switch (e.key) {
      case 'ArrowDown':
        e.preventDefault();
        setActiveIndex((i) => (filtered.length ? (i + 1) % filtered.length : 0));
        return;
      case 'ArrowUp':
        e.preventDefault();
        setActiveIndex((i) => (filtered.length ? (i - 1 + filtered.length) % filtered.length : 0));
        return;
      case 'Enter':
      case 'Tab':
        if (filtered.length === 0) {
          // Nothing to insert — let Enter/Tab do its normal thing (newline,
          // focus move, form submit, ...) instead of swallowing it just
          // because the popover happens to be open with no matches.
          setOpen(false);
          return;
        }
        e.preventDefault();
        insertSuggestion(filtered[activeIndex]);
        return;
      case 'Escape':
        e.preventDefault();
        setOpen(false);
        return;
      default:
        return;
    }
  }

  function handleBlur() {
    blurTimer.current = setTimeout(() => setOpen(false), BLUR_CLOSE_DELAY_MS);
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
                    onClick={() => insertSuggestion(s)}
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
