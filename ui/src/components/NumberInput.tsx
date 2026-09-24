/**
 * Input for numeric config fields that also accepts an environment
 * placeholder, e.g. an upstream `port` of `${BACKEND_PORT}` or
 * `${BACKEND_PORT:-3000}`. The rules live in ../numberField.ts.
 *
 * A native `type="number"` input cannot hold a placeholder at all, and it
 * shows a stored placeholder string (loaded from gateway.yaml) as empty. So
 * this is a text input: while the text is a number or a whole placeholder it
 * is stored; anything else (a half-typed `${BACK`) stays on screen, marked
 * invalid, and the config keeps its last valid value.
 *
 * @module components/NumberInput
 */
import { useState, type CSSProperties } from 'react';
import { numberFieldText, parseNumberField } from '../numberField';

interface NumberInputProps {
  value: unknown;
  /** Receives a number, a placeholder string, or `undefined` for an empty field. */
  onChange: (value: number | string | undefined) => void;
  placeholder?: string;
  style?: CSSProperties;
  'aria-label'?: string;
}

export function NumberInput({ value, onChange, placeholder, style, 'aria-label': ariaLabel }: NumberInputProps) {
  // The text being typed, kept while focused so "1." or "${PO" is not
  // rewritten under the cursor; null shows the stored value.
  const [draft, setDraft] = useState<string | null>(null);
  const text = draft ?? numberFieldText(value);
  const invalid = draft !== null && !parseNumberField(draft).ok;

  return (
    <input
      type="text"
      inputMode="decimal"
      value={text}
      placeholder={placeholder}
      aria-label={ariaLabel}
      aria-invalid={invalid || undefined}
      title={invalid ? 'Enter a number, or one environment placeholder such as ${PORT} or ${PORT:-3000}' : undefined}
      onChange={(e) => {
        setDraft(e.target.value);
        const parsed = parseNumberField(e.target.value);
        if (parsed.ok) onChange(parsed.value);
      }}
      onBlur={() => {
        if (draft !== null && parseNumberField(draft).ok) setDraft(null);
      }}
      style={{
        ...style,
        fontFamily: typeof value === 'string' || (draft ?? '').includes('${') ? 'var(--font-mono)' : style?.fontFamily,
        ...(invalid ? { borderColor: 'var(--error)', boxShadow: '0 0 0 1px var(--error)' } : {}),
      }}
    />
  );
}
