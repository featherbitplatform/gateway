/**
 * Styled modal dialog and its building blocks (DialogButton, DialogField),
 * used by the admin UI in place of the browser's native prompt/confirm/alert
 * for flows like creating or deleting a route.
 *
 * @module components/Dialog
 */
import type { CSSProperties, ReactNode } from 'react';

/** Props for Dialog. */
interface DialogProps {
  /** Whether the dialog is shown; when false the component renders nothing. */
  open: boolean;
  /** Header text, also used as the dialog's accessible label. */
  title: string;
  /** Body content, typically one or more DialogField inputs or a confirmation message. */
  children: ReactNode;
  /** Right-aligned action row, typically DialogButton elements (cancel/confirm). */
  footer: ReactNode;
  /** Invoked when the backdrop is clicked; clicks inside the panel do not close. */
  onClose: () => void;
  /** Panel width in px; defaults to 380. Widen for code/YAML views. */
  width?: number;
}

/**
 * Modal dialog — the styled replacement for native prompt/confirm/alert.
 * Renders a scrim with a centered panel (header, body, footer) that fades
 * and scales in from 0.96 (rg-scrim / rg-dialog). Clicking the backdrop calls `onClose`; there is no Escape-key
 * handling or focus trap.
 *
 * @remarks Compose the body from DialogField and the footer from DialogButton
 * in this module.
 */
export function Dialog({ open, title, children, footer, onClose, width = 380 }: DialogProps) {
  if (!open) return null;

  return (
    <div
      className="rg-scrim fixed inset-0 flex items-center justify-center"
      style={{
        zIndex: 80,
        background: 'var(--scrim)',
        backdropFilter: 'blur(2px)',
        WebkitBackdropFilter: 'blur(2px)',
      }}
      onClick={onClose}
    >
      <div
        role="dialog"
        aria-modal="true"
        aria-label={title}
        onClick={(e) => e.stopPropagation()}
        className="rg-dialog"
        style={{
          width,
          maxWidth: 'calc(100vw - 32px)',
          background: 'var(--surface)',
          border: '1px solid var(--border)',
          borderRadius: 'var(--radius-lg)',
          boxShadow: 'var(--shadow-xl), var(--shadow-inset)',
          overflow: 'hidden',
        }}
      >
        <div
          style={{
            padding: '16px 20px 4px',
            fontSize: 'var(--text-md)',
            fontWeight: 600,
            letterSpacing: 'var(--tracking-tight)',
            color: 'var(--text-primary)',
          }}
        >
          {title}
        </div>
        <div style={{ padding: '12px 20px 16px' }}>{children}</div>
        <div
          className="flex justify-end gap-2"
          style={{
            padding: '12px 16px',
            borderTop: '1px solid var(--border-subtle)',
            background: 'var(--surface-sunken)',
          }}
        >
          {footer}
        </div>
      </div>
    </div>
  );
}

/** Visual variants of {@link DialogButton}. */
type DialogButtonVariant = 'primary' | 'ghost' | 'danger' | 'danger-quiet';

/** Resting and hover colors per DialogButton variant (rg-hover vars). */
const DIALOG_BUTTON_VARS: Record<DialogButtonVariant, Record<string, string>> = {
  primary: {
    '--rg-bg': 'var(--accent)',
    '--rg-fg': 'var(--text-on-accent)',
    '--rg-bd': 'transparent',
    '--rg-hover-bg': 'var(--accent-hover)',
  },
  danger: {
    '--rg-bg': 'var(--error-solid)',
    '--rg-fg': 'var(--text-on-accent)',
    '--rg-bd': 'transparent',
    '--rg-hover-bg': 'color-mix(in oklab, var(--error-solid) 88%, black)',
  },
  'danger-quiet': {
    '--rg-bg': 'transparent',
    '--rg-fg': 'var(--error)',
    '--rg-bd': 'color-mix(in oklab, var(--error) 40%, transparent)',
    '--rg-hover-bg': 'var(--error-soft)',
    '--rg-hover-bd': 'var(--error)',
  },
  ghost: {
    '--rg-bg': 'transparent',
    '--rg-fg': 'var(--text-secondary)',
    '--rg-bd': 'var(--border)',
    '--rg-hover-bg': 'var(--surface-hover)',
    '--rg-hover-fg': 'var(--text-primary)',
  },
};

/**
 * Footer action button for Dialog.
 *
 * Variants: `primary` (accent fill, for the confirming action), `danger`
 * (solid error fill: inside a dialog it is always the final, deliberate
 * confirmation of a destructive action such as deleting a route),
 * `danger-quiet` (red text and tinted border: a destructive action that is
 * not itself the final confirmation), and `ghost` (quiet, for cancel). Press feedback and hover come from the
 * rg-press / rg-hover classes in index.css.
 *
 * @param variant - Visual style of the button; defaults to `primary`.
 * @param disabled - When true, dims the button and blocks `onClick` (e.g. a
 * confirming action whose required fields are not yet filled in).
 */
export function DialogButton({
  variant = 'primary',
  onClick,
  children,
  disabled = false,
}: {
  variant?: DialogButtonVariant;
  onClick: () => void;
  children: ReactNode;
  disabled?: boolean;
}) {
  const vars = DIALOG_BUTTON_VARS[variant];
  return (
    <button
      onClick={onClick}
      disabled={disabled}
      className="rg-press rg-hover"
      style={{
        height: 32,
        padding: '0 14px',
        borderRadius: 'var(--radius-sm)',
        fontSize: 'var(--text-sm)',
        fontWeight: 500,
        borderWidth: 1,
        borderStyle: 'solid',
        ...(vars as CSSProperties),
        opacity: disabled ? 0.5 : 1,
        cursor: disabled ? 'not-allowed' : 'pointer',
      }}
    >
      {children}
    </button>
  );
}

/**
 * Labelled single-line text input for use inside a Dialog body — the
 * replacement for the value half of a native prompt(). Controlled: renders
 * `value` and reports edits through `onChange`.
 *
 * `label` is the caption rendered above the input. `mono` (default false)
 * renders the input in the monospace font, for code-like values such as
 * route paths or upstream URLs. `autoFocus` (default false) focuses the
 * input when it mounts, for the dialog's primary field.
 */
export function DialogField({
  label,
  value,
  onChange,
  placeholder,
  mono = false,
  autoFocus = false,
}: {
  label: string;
  value: string;
  onChange: (v: string) => void;
  placeholder?: string;
  mono?: boolean;
  autoFocus?: boolean;
}) {
  return (
    <div style={{ marginBottom: 12 }}>
      <label
        style={{
          display: 'block',
          fontSize: 'var(--text-xs)',
          fontWeight: 500,
          color: 'var(--text-secondary)',
          marginBottom: 4,
        }}
      >
        {label}
      </label>
      <input
        type="text"
        value={value}
        autoFocus={autoFocus}
        placeholder={placeholder}
        onChange={(e) => onChange(e.target.value)}
        className="rg-field w-full"
        style={{
          height: 32,
          padding: '0 10px',
          borderRadius: 'var(--radius-sm)',
          fontFamily: mono ? 'var(--font-mono)' : 'var(--font-sans)',
          fontSize: 'var(--text-sm)',
          color: 'var(--text-primary)',
          outline: 'none',
        }}
      />
    </div>
  );
}
