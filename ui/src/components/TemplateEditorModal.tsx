/**
 * Expanded editor for a single `VarInput` field — a wide modal that pairs a
 * multi-line draft textarea with an always-populated suggestion panel above
 * it, for authoring longer or more intricate `{{path}}` templates than the
 * inline popover comfortably supports.
 *
 * Shares `useTemplateToken` (`../templateToken.ts`, Task 1) with `VarInput`
 * rather than re-implementing token detection/filtering/insertion: this
 * component only adds the modal chrome (Dialog, draft state, the panel's
 * browse-vs-filtered switch) around the same hook. See `VarInput.tsx` for
 * the popover this modal is the "expand" counterpart of, and its "Expand
 * editor" button / Ctrl+Space shortcut that open this modal.
 *
 * ## Browse mode
 *
 * The suggestion panel is always populated: `filtered` (trigger + mode +
 * substring filtered against the in-progress token) while a token is active,
 * every suggestion (mode-filtered only) otherwise, so the user can browse the
 * full catalog before typing a token at all. Clicking a row in browse mode
 * must insert at the bare caret rather than reuse `tokenStart`, which could
 * be stale (left over from a token that's no longer active) or simply 0 (a
 * modal that just opened never had one) — `useTemplateToken.insert` was
 * extended with an explicit `insertAtCaretWhenNoToken` option for exactly
 * this (see its doc comment in `templateToken.ts`) rather than duplicating
 * the splice/caret-restore logic here.
 *
 * ## Mounting contract
 *
 * `VarInput` mounts this component conditionally (`{modalOpen &&
 * <TemplateEditorModal .../>}`) rather than always rendering it with `open`
 * toggling — so there's never an idle `useTemplateToken` instance (and a
 * full suggestion-list filter pass) running per closed field. A consequence
 * this component relies on: every mount already has `open` true from the
 * very first render, so the `[open]`-keyed effect below (draft
 * (re)initialization + focus) always finds its own `<textarea>` already in
 * the DOM the first time it runs — refs are attached during the commit
 * phase, which completes before any effect fires, regardless of whether
 * that commit is this component's *first* one (conditional mount) or a
 * later one (a hypothetical caller that keeps it mounted and toggles `open`
 * instead — the same effect covers that case too, unchanged).
 *
 * @module components/TemplateEditorModal
 */
import { useEffect, useRef, useState } from 'react';
import { Dialog, DialogButton } from './Dialog';
import { ValueCell } from './ValueCell';
import { useTemplateToken } from '../templateToken';
import { AVAILABILITY_MESSAGE, ENV_ONLY_MESSAGE } from '../varSuggestions';
import type { Availability, Suggestion } from '../varSuggestions';

/** Props for TemplateEditorModal. */
export interface TemplateEditorModalProps {
  /** Whether the modal is shown. */
  open: boolean;
  /** Esc, backdrop click, or Cancel — discards the draft, no `onApply`. */
  onClose: () => void;
  /** Field's current value; the draft re-initializes from this every time `open` flips to true. */
  value: string;
  /** Apply button — called with the draft; caller is expected to also close the modal. */
  onApply: (v: string) => void;
  /** Candidate rows; same list `VarInput` was given for this field. */
  suggestions: Suggestion[];
  /** Why live `value`s may be missing from `suggestions`; drives the footer message. */
  availability: Availability;
  /** Same meaning as `VarInput`'s prop of the same name. */
  templateMode: 'full' | 'env-only';
  /** Same meaning as `VarInput`'s prop of the same name. */
  legacyDollar: boolean;
  /** Opens the full context-vars legend/reference dialog. */
  onOpenLegend: () => void;
}

/** Row layout shared by both group-header and plain rows in the panel. */
const ROW_STYLE: React.CSSProperties = {
  display: 'flex',
  flexWrap: 'wrap',
  alignItems: 'baseline',
  justifyContent: 'space-between',
  gap: 8,
  padding: '5px 10px',
  cursor: 'pointer',
};

/**
 * Value-cell style override for the panel: unlike the popover's single-line
 * ellipsis truncation, the modal never truncates — a long value wraps
 * (`overflowWrap: 'anywhere'`) inside its own column instead, reading as a
 * dimmed second line under the row when it doesn't fit on the first.
 */
const WRAPPING_VALUE_STYLE: React.CSSProperties = {
  maxWidth: '60%',
  overflow: 'visible',
  textOverflow: 'clip',
  whiteSpace: 'normal',
  overflowWrap: 'anywhere',
  textAlign: 'right',
  flexShrink: 1,
};

export function TemplateEditorModal({
  open,
  onClose,
  value,
  onApply,
  suggestions,
  availability,
  templateMode,
  legacyDollar,
  onOpenLegend,
}: TemplateEditorModalProps) {
  const elRef = useRef<HTMLTextAreaElement | null>(null);
  // Local scratch copy the user edits; only committed to the field via
  // `onApply` on the Apply click. Re-initialized from `props.value` every
  // time `open` flips to true (not on every `value` change while already
  // open, which would clobber in-progress edits out from under the user —
  // see the effect below).
  const [draft, setDraft] = useState(value);

  useEffect(() => {
    if (!open) return;
    setDraft(value);
    // Move focus (and the caret) into the modal's own textarea the moment it
    // opens — without this, focus stays wherever it was on the field behind
    // the backdrop (the VarInput this modal was opened from). Two concrete
    // bugs that caused: (1) opening via Ctrl+Space left focus on the hidden
    // field, so the user's very next keystrokes edited that field instead of
    // the draft — invisible until Apply overwrote the field with the (still
    // stale) draft, silently discarding what was actually typed; (2) the
    // first click on a browse-mode row read `elRef.current.selectionStart`
    // off an unfocused textarea, which reports `0`, so `insert`'s
    // `insertAtCaretWhenNoToken` path spliced at position 0 instead of at the
    // end of the draft. Caret placed at `value.length` (the end of the
    // freshly-(re)initialized draft) rather than `0` so a browse-mode
    // insertion append to existing content instead of prepending to it.
    // `elRef.current` is guaranteed non-null here: this effect only runs
    // while `open` (the textarea is in the DOM — see this component's module
    // doc comment on conditional mounting), and refs are attached during the
    // commit phase, which always completes before effects run.
    const el = elRef.current;
    if (el) {
      el.focus();
      el.setSelectionRange(value.length, value.length);
    }
    // Deliberately keyed on `open` alone: this re-initializes the draft (and
    // refocuses) the moment the modal opens (including a re-open after a
    // prior Cancel/Esc discard), not on every subsequent `value` prop change
    // while it stays open.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [open]);

  const tokenState = useTemplateToken({
    value: draft,
    onChange: setDraft,
    elRef,
    suggestions,
    templateMode,
    legacyDollar,
  });
  const { open: tokenOpen, filtered, activeIndex } = tokenState;

  // Global Escape-to-close: Dialog itself has no Escape handling (verified —
  // see task-2-report.md), so this modal adds its own rather than forking
  // Dialog for every caller. Attached at `window` level (not just the
  // textarea's `onKeyDown`) so Escape closes the modal regardless of which
  // element currently has focus (textarea, Cancel/Apply button, a
  // suggestion row). Added/removed keyed on `open` alone, so no listener is
  // left attached to `window` once the modal closes.
  useEffect(() => {
    if (!open) return;
    function handleWindowKeyDown(e: KeyboardEvent) {
      // Explicit consume/bubble contract (not implicit bubble-order/stale-
      // closure timing): the token popover's own Escape handling
      // (`useTemplateToken`'s `onKeyDown`, reached via the textarea's
      // `onKeyDown` below) calls `e.preventDefault()` when it consumes an
      // Escape to close just the in-progress-token suggestion state — that
      // handler runs first (React's delegated listener is an ancestor of
      // `window` in the bubble chain, so it always fires before this native
      // `window` listener sees the same event). This listener closes the
      // modal only on an Escape nothing upstream has already consumed.
      if (e.defaultPrevented) return;
      if (e.key !== 'Escape') return;
      // The var-legend dialog (`VarLegend.tsx`, opened via this modal's own
      // `onOpenLegend` prop) is owned and rendered by an ancestor outside
      // this component's reach — `onOpenLegend` is a bare callback, not a
      // piece of state this modal can see, so there is no prop to plumb
      // "is the legend currently open" across that ownership boundary
      // without inventing a new one purely for this. Pragmatic, local fix:
      // check for the legend's own content marker
      // (`data-testid="var-legend"`, present in the DOM only while
      // `VarLegend`'s `open` is true) and, when present, treat this Escape as
      // belonging to the legend — don't also discard this modal's draft
      // underneath it. `Dialog` itself has no Escape handling (see this
      // file's other Escape comment), so this Escape does nothing at all in
      // that case; closing the legend remains a separate, unaddressed
      // concern (its own Cancel/backdrop-click still works), deliberately
      // out of scope here.
      if (document.querySelector('[data-testid="var-legend"]')) return;
      onClose();
    }
    window.addEventListener('keydown', handleWindowKeyDown);
    return () => window.removeEventListener('keydown', handleWindowKeyDown);
  }, [open, onClose]);

  // Browse-mode rows: every suggestion the field actually supports, mode-
  // and trigger-filtered exactly like the hook's own `filtered` would be if
  // a token were active — there just isn't one to pick a single
  // `activeTrigger` from, so both offered triggers pass. `suggestions`
  // always contains both `'dollar'` and `'brace'` rows regardless of what
  // this field accepts (see varSuggestions.ts); a `'dollar'` row (`$uri`,
  // `$http_*`, ...) only ever resolves on a `legacyDollar` field, so a
  // non-legacy field must only ever browse `'brace'` (and, when
  // `templateMode === 'full'`, `'env'`) rows — otherwise it would offer a
  // clickable row whose `insert` text is inert literal text once written
  // into a field that doesn't parse `$name`/`${name}` at all. Shown whenever
  // no token is active, so the panel is never empty.
  const browseSuggestions = suggestions.filter(
    (s) =>
      (templateMode !== 'env-only' || s.group === 'env') && (s.trigger === 'brace' || legacyDollar),
  );
  const rows = tokenOpen ? filtered : browseSuggestions;

  function handleChange(e: React.ChangeEvent<HTMLTextAreaElement>) {
    setDraft(e.target.value);
    tokenState.onValueEvent();
  }

  function handleSelect() {
    tokenState.onValueEvent();
  }

  function handleKeyDown(e: React.KeyboardEvent<HTMLTextAreaElement>) {
    tokenState.onKeyDown(e);
  }

  function handleRowClick(s: Suggestion) {
    // `insertAtCaretWhenNoToken: true` is a no-op while a token is active
    // (the hook only applies it when `!open`) — see `templateToken.ts` — so
    // this single call correctly serves both the filtered (token-active) and
    // browse (no token) cases without branching here.
    tokenState.insert(s, { insertAtCaretWhenNoToken: true });
  }

  let previousGroup: string | null = null;

  return (
    <Dialog
      open={open}
      title="Template editor"
      width={720}
      onClose={onClose}
      footer={
        <>
          <DialogButton variant="ghost" onClick={onClose}>
            Cancel
          </DialogButton>
          <DialogButton variant="primary" onClick={() => onApply(draft)}>
            Apply
          </DialogButton>
        </>
      }
    >
      <div data-testid="template-editor-modal">
        <div
          data-testid="template-editor-suggestions"
          style={{
            maxHeight: '40vh',
            overflowY: 'auto',
            border: '1px solid var(--border-subtle)',
            borderRadius: 'var(--radius-sm)',
            marginBottom: 10,
          }}
        >
          {rows.length === 0 ? (
            <div style={{ padding: '8px 10px', fontSize: 'var(--text-xs)', color: 'var(--text-muted)' }}>
              No matching variables
            </div>
          ) : (
            rows.map((s, i) => {
              const showGroupHeader = s.group !== previousGroup;
              previousGroup = s.group;
              const isActive = tokenOpen && i === activeIndex;
              return (
                <div key={`${s.group}:${s.name}`}>
                  {showGroupHeader && (
                    <div className="eyebrow" style={{ padding: '6px 10px 2px' }}>
                      {s.group}
                    </div>
                  )}
                  <div
                    onMouseDown={(e) => e.preventDefault()}
                    onClick={() => handleRowClick(s)}
                    style={{
                      ...ROW_STYLE,
                      background: isActive ? 'var(--surface-active)' : 'transparent',
                    }}
                  >
                    <span
                      style={{
                        fontFamily: 'var(--font-mono)',
                        fontSize: 'var(--text-xs)',
                        color: 'var(--text-primary)',
                        overflowWrap: 'anywhere',
                        flex: '1 1 auto',
                      }}
                    >
                      {s.name}
                    </span>
                    <ValueCell suggestion={s} style={WRAPPING_VALUE_STYLE} />
                  </div>
                </div>
              );
            })
          )}
        </div>
        <textarea
          ref={elRef}
          data-testid="template-editor-input"
          rows={4}
          value={draft}
          onChange={handleChange}
          onSelect={handleSelect}
          onKeyDown={handleKeyDown}
          className="w-full resize-y"
          style={{
            fontFamily: 'var(--font-mono)',
            fontSize: 'var(--text-sm)',
            padding: '8px 10px',
            borderRadius: 'var(--radius-sm)',
            background: 'var(--surface-input)',
            color: 'var(--text-primary)',
            border: '1px solid var(--border)',
          }}
        />
        <div
          style={{
            display: 'flex',
            alignItems: 'center',
            justifyContent: 'space-between',
            gap: 8,
            marginTop: 8,
          }}
        >
          <span style={{ fontSize: 'var(--text-2xs)', color: 'var(--text-muted)' }}>
            {templateMode === 'env-only'
              ? ENV_ONLY_MESSAGE
              : availability !== 'ok' ? AVAILABILITY_MESSAGE[availability] : ''}
          </span>
          <button
            type="button"
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
    </Dialog>
  );
}
