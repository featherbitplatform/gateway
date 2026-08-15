/**
 * Renders the value/note cell of one suggestion row — shared by `VarInput`'s
 * popover and `TemplateEditorModal`'s panel.
 *
 * Extracted to its own module (rather than living in, and being exported
 * from, either component file) so the two don't form a circular import:
 * `VarInput` renders `TemplateEditorModal` (the "Expand editor" button /
 * Ctrl+Space target) and `TemplateEditorModal` needs this same cell —
 * putting it in a third, dependency-free module lets both import it without
 * either importing the other.
 *
 * @module components/ValueCell
 */
import type { Suggestion } from '../varSuggestions';

/**
 * Three cases need distinct styling from a normal preview (dimmed, single
 * line): a literal `'<redacted>'` value must not read as if it were the
 * real resolved value (shown muted + italic, same treatment as a `note`);
 * an empty-string value renders as `(empty)` rather than a blank cell; a
 * `note` (no live value at all) is muted + italic. Everything else is a
 * plain dimmed preview — `previewValue` (varSuggestions.ts) already
 * collapsed whitespace and capped the length upstream.
 *
 * Takes an optional `style` override merged on top of the default
 * truncated-single-line layout, so `TemplateEditorModal` can swap in its own
 * no-truncation, wrap-to-second-line row layout instead of forking this
 * component.
 */
export function ValueCell({
  suggestion,
  style,
}: {
  suggestion: Suggestion;
  style?: React.CSSProperties;
}) {
  const cellStyle: React.CSSProperties = {
    fontSize: 'var(--text-2xs)',
    maxWidth: '55%',
    overflow: 'hidden',
    textOverflow: 'ellipsis',
    whiteSpace: 'nowrap',
    flexShrink: 0,
    ...style,
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
