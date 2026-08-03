/**
 * Shared `{{path}}` (and, on opted-in fields, legacy `$var`) token-detection,
 * filtering, keyboard navigation, and insertion engine behind {@link
 * VarInput}'s autocomplete popover.
 *
 * Extracted out of `VarInput.tsx` (behavior-preserving refactor — see
 * task-1-brief.md) so a second consumer (the template-editor modal, Task 2)
 * can share the exact same token logic instead of duplicating it. This
 * module owns only the *state machine*: open/filter/active-suggestion state,
 * the `pendingCaret`/`justInserted` refs, and the caret-restore effect.
 * Popover rendering, availability messaging, and the footer stay in
 * `VarInput` (and, from Task 2, whatever else renders a popover against this
 * hook) — see that component's module doc comment for the popover's
 * user-facing behavior contract.
 *
 * @module templateToken
 */
import { useEffect, useRef, useState } from 'react';
import type { Suggestion } from './varSuggestions';

/**
 * Matches an in-progress token ending exactly at the caret: either a `{{`
 * token (group 2; always active — this is the universal-template trigger)
 * or a legacy `$name` / `${name` token (group 3; only ever matched when the
 * caller opts the field into `legacyDollar` — see `onValueEvent` below, which
 * rejects a group-3 match when that prop is false rather than maintaining
 * two separate regexes).
 */
const TOKEN_RE = /(\{\{\s*([A-Za-z0-9_.-]*)|\$\{?([A-Za-z0-9_.]*))$/;

/** Args accepted by {@link useTemplateToken}. */
export interface TemplateTokenArgs {
  /** Current field value (controlled). */
  value: string;
  /** Called with the full replacement value on insert. */
  onChange: (v: string) => void;
  /** Ref to the underlying `<input>`/`<textarea>` — used to read/restore the caret. */
  elRef: React.RefObject<HTMLInputElement | HTMLTextAreaElement | null>;
  /** Candidate rows; filtered here by the in-progress token, `templateMode`, and trigger. */
  suggestions: Suggestion[];
  /** See `VarInput`'s `templateMode` prop doc comment. */
  templateMode: 'full' | 'env-only';
  /** Whether this field also accepts legacy `$name`/`${name}` completions. */
  legacyDollar: boolean;
}

/** State + handlers returned by {@link useTemplateToken}. */
export interface TemplateTokenState {
  open: boolean;
  /** Trigger- and mode-filtered rows, ready to render. */
  filtered: Suggestion[];
  activeIndex: number;
  setActiveIndex: (i: number) => void;
  /** Call from the field's `onKeyDown`; returns `true` when handled (caller stops). */
  onKeyDown: (e: React.KeyboardEvent) => boolean;
  /** Call from the field's `onChange`/`onSelect`/keyup paths to re-detect the token at the caret. */
  onValueEvent: () => void;
  insert: (s: Suggestion | undefined) => void;
  close: () => void;
}

/**
 * Detects an in-progress `{{path` (and, when `legacyDollar`, `$name`/`${name`)
 * token at the caret, filters `suggestions` against it (trigger + mode +
 * substring), and exposes keyboard navigation + insertion.
 *
 * @remarks Token detection re-runs on every change and every selection
 * change (mouse click, arrow keys) against `value.slice(0, selectionStart)`,
 * per the behavior contract in task-5-brief.md. Insertion replaces the
 * matched token span with the suggestion's insert text, restores focus, and
 * places the caret right after the inserted text.
 */
export function useTemplateToken(args: TemplateTokenArgs): TemplateTokenState {
  const { value, onChange, elRef, suggestions, templateMode, legacyDollar } = args;

  const [open, setOpen] = useState(false);
  const [filter, setFilter] = useState('');
  const [tokenStart, setTokenStart] = useState(0);
  const [activeIndex, setActiveIndex] = useState(0);
  // Which token syntax the currently-open popover is completing — decides
  // which half of `suggestions` (see `Suggestion.trigger` in
  // varSuggestions.ts) `filtered` below draws from. Only meaningful while
  // `open`; reset alongside every other token-derived piece of state in
  // `onValueEvent`.
  const [activeTrigger, setActiveTrigger] = useState<Suggestion['trigger']>('brace');

  // Caret position to apply once the controlled `value` prop reflects an
  // insertion — the DOM's own value only updates after the consuming
  // component re-renders with the new prop, so `setSelectionRange` has to
  // wait for it.
  const pendingCaret = useRef<number | null>(null);
  // One-shot guard: `setSelectionRange` in the caret effect below fires a
  // native `select` event on the field, which would otherwise reach
  // `onValueEvent` and immediately reopen the popover on the freshly-inserted
  // `$name`. Set right before the insertion's `onChange`, consumed (and
  // cleared) by the very next `onValueEvent` call — which is that synthetic
  // reopen — so any *later*, real caret movement or typing still opens the
  // popover normally.
  const justInserted = useRef(false);

  const lowerFilter = filter.toLowerCase();
  const filtered = suggestions.filter(
    (s) =>
      s.trigger === activeTrigger &&
      (templateMode !== 'env-only' || s.group === 'env') &&
      s.name.toLowerCase().includes(lowerFilter),
  );

  useEffect(() => {
    const el = elRef.current;
    if (pendingCaret.current !== null && el) {
      const pos = pendingCaret.current;
      el.setSelectionRange(pos, pos);
      pendingCaret.current = null;
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps -- caret restore is keyed on `value` only, matching the pre-extraction effect verbatim; `elRef` is a stable ref object.
  }, [value]);

  /** Re-derives the popover's open/filter/token state from the caret position. */
  function onValueEvent() {
    if (justInserted.current) {
      // Swallow exactly the synthetic `select` event fired by this hook's
      // own post-insert `setSelectionRange` (see the ref's comment) and
      // nothing after it.
      justInserted.current = false;
      return;
    }
    const el = elRef.current;
    if (!el) return;
    const pos = el.selectionStart ?? el.value.length;
    const head = el.value.slice(0, pos);
    const match = head.match(TOKEN_RE);
    if (!match) {
      setOpen(false);
      return;
    }
    // Group 2 only participates when the `{{` alternative matched; group 3
    // only when the `$`/`${` alternative matched (see TOKEN_RE's doc
    // comment) — exactly one of the two is ever defined.
    const isDollarToken = match[3] !== undefined;
    if (isDollarToken && !legacyDollar) {
      // This field never offers `$` completions — treat it as no match at
      // all rather than opening a popover for a token the field doesn't
      // support (and doesn't want its typing gated on).
      setOpen(false);
      return;
    }
    setTokenStart(pos - match[0].length);
    setFilter((isDollarToken ? match[3] : match[2]) ?? '');
    setActiveTrigger(isDollarToken ? 'dollar' : 'brace');
    setActiveIndex(0);
    setOpen(true);
  }

  function insert(suggestion: Suggestion | undefined) {
    if (!suggestion) return;
    // `env` rows carry two insertion forms (see `insertEnvOnly` on
    // `Suggestion`): `{{env.NAME}}` only ever resolves in a field the plugin
    // parses into a `Template` ('full'); an 'env-only' field never reaches
    // `Template::parse`; only the universal `${NAME}` env pass
    // (`interpolate_env_json`, applied to every field regardless of
    // templating) resolves there. Inserting the brace form into an
    // 'env-only' field would look identical to a working suggestion but
    // never actually substitute — the exact "dishonest suggestion" this
    // component otherwise goes out of its way to avoid. Every other
    // suggestion (non-`env` groups, or `env` in a `'full'` field) is
    // unaffected.
    const insertText =
      templateMode === 'env-only' && suggestion.insertEnvOnly !== undefined
        ? suggestion.insertEnvOnly
        : suggestion.insert;
    const el = elRef.current;
    const caret = el?.selectionStart ?? tokenStart;
    // If the in-progress token began with a brace form and the caret sits
    // right before that token's own closing brace(s), consume them too —
    // otherwise the inserted text leaves stray brace(s) behind (`${foo` +
    // completion + already-typed `}` -> double brace; same idea for `{{foo`
    // + completion + already-typed `}}`). Gated on the *matched* token's
    // opening delimiter (not on whether this suggestion's own `insert`
    // happens to use braces), so completing a plain `$token` never eats an
    // unrelated, legitimate `}` at the caret.
    const isDollarBraceToken = value[tokenStart] === '$' && value[tokenStart + 1] === '{';
    const isDoubleBraceToken = value[tokenStart] === '{' && value[tokenStart + 1] === '{';
    let spliceEnd = caret;
    if (isDollarBraceToken && value[caret] === '}') {
      spliceEnd = caret + 1;
    } else if (isDoubleBraceToken) {
      if (value.slice(caret, caret + 2) === '}}') {
        spliceEnd = caret + 2;
      } else if (value[caret] === '}') {
        spliceEnd = caret + 1;
      }
    }
    const nextValue = `${value.slice(0, tokenStart)}${insertText}${value.slice(spliceEnd)}`;
    setOpen(false);
    if (nextValue === value) {
      // No-op insertion (the token was already fully typed out verbatim):
      // `onChange` won't fire, so the `[value]`-keyed caret effect never
      // runs either. Clear the pending caret here instead of leaving it to
      // be misapplied on some later, unrelated value change.
      pendingCaret.current = null;
      return;
    }
    pendingCaret.current = tokenStart + insertText.length;
    justInserted.current = true;
    onChange(nextValue);
    el?.focus();
  }

  function onKeyDown(e: React.KeyboardEvent): boolean {
    if (!open) return false;
    switch (e.key) {
      case 'ArrowDown':
        e.preventDefault();
        setActiveIndex((i) => (filtered.length ? (i + 1) % filtered.length : 0));
        return true;
      case 'ArrowUp':
        e.preventDefault();
        setActiveIndex((i) => (filtered.length ? (i - 1 + filtered.length) % filtered.length : 0));
        return true;
      case 'Enter':
      case 'Tab':
        if (filtered.length === 0) {
          // Nothing to insert — let Enter/Tab do its normal thing (newline,
          // focus move, form submit, ...) instead of swallowing it just
          // because the popover happens to be open with no matches.
          setOpen(false);
          return true;
        }
        e.preventDefault();
        insert(filtered[activeIndex]);
        return true;
      case 'Escape':
        e.preventDefault();
        setOpen(false);
        return true;
      default:
        return false;
    }
  }

  function close() {
    setOpen(false);
  }

  return { open, filtered, activeIndex, setActiveIndex, onKeyDown, onValueEvent, insert, close };
}
