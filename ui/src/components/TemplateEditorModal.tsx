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
 * @module components/TemplateEditorModal
 */
import { useEffect, useRef, useState } from 'react';
import { Dialog, DialogButton } from './Dialog';
import { ValueCell } from './VarInput';
import { useTemplateToken } from '../templateToken';
import { AVAILABILITY_MESSAGE } from '../varSuggestions';
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
    if (open) setDraft(value);
    // Deliberately keyed on `open` alone: this re-initializes the draft the
    // moment the modal opens (including a re-open after a prior Cancel/Esc
    // discard), not on every subsequent `value` prop change while it stays
    // open.
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
  // suggestion row). Skipped while a token is active (`tokenOpen`, captured
  // from the render current when this listener was last (re)installed) so
  // Escape closes only the in-progress-token suggestion state first — the
  // textarea's own `onKeyDown` below already delegates that Escape to the
  // hook — and a second Escape then closes the modal.
  useEffect(() => {
    if (!open) return;
    function handleWindowKeyDown(e: KeyboardEvent) {
      if (e.key === 'Escape' && !tokenOpen) {
        onClose();
      }
    }
    window.addEventListener('keydown', handleWindowKeyDown);
    return () => window.removeEventListener('keydown', handleWindowKeyDown);
  }, [open, tokenOpen, onClose]);

  // Browse-mode rows: every suggestion, restricted only by `templateMode`
  // (mirrors the hook's own mode filter) — not by `trigger`, since there is
  // no active token to decide which trigger family the user means. Shown
  // whenever no token is active, so the panel is never empty.
  const browseSuggestions = suggestions.filter(
    (s) => templateMode !== 'env-only' || s.group === 'env',
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
            {availability !== 'ok' ? AVAILABILITY_MESSAGE[availability] : ''}
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
