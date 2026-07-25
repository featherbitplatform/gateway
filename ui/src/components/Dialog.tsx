/**
 * Styled modal dialog and its building blocks (DialogButton, DialogField),
 * used by the admin UI in place of the browser's native prompt/confirm/alert
 * for flows like creating or deleting a route.
 *
 * @module components/Dialog
 */
import type { ReactNode } from 'react';

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
 * Renders a dimmed, blurred backdrop with a centered panel (header, body,
 * footer). Clicking the backdrop calls `onClose`; there is no Escape-key
 * handling or focus trap.
 *
 * @remarks Compose the body from DialogField and the footer from DialogButton
 * in this module.
 */
export function Dialog({ open, title, children, footer, onClose, width = 380 }: DialogProps) {
  if (!open) return null;

  return (
    <div
      className="fixed inset-0 flex items-center justify-center"
      style={{
        zIndex: 80,
        background: 'rgba(5, 8, 16, 0.62)',
        backdropFilter: 'blur(3px)',
        WebkitBackdropFilter: 'blur(3px)',
      }}
      onClick={onClose}
    >
      <div
        role="dialog"
        aria-modal="true"
        aria-label={title}
        onClick={(e) => e.stopPropagation()}
        style={{
          width,
          maxWidth: 'calc(100vw - 32px)',
          background: 'var(--surface)',
          border: '1px solid var(--border)',
          borderRadius: 'var(--radius-md)',
          boxShadow: 'var(--shadow-lg)',
        }}
      >
        <div
          style={{
            padding: '14px 16px',
            borderBottom: '1px solid var(--border)',
            fontSize: 'var(--text-md)',
            fontWeight: 600,
            color: 'var(--text-primary)',
          }}
        >
          {title}
        </div>
        <div style={{ padding: 16 }}>{children}</div>
        <div
          className="flex justify-end gap-2"
          style={{ padding: '12px 16px', borderTop: '1px solid var(--border)' }}
        >
          {footer}
        </div>
      </div>
    </div>
  );
}

/**
 * Footer action button for Dialog.
 *
 * Variants: `primary` (accent background, for the confirming action),
 * `danger` (error background, for destructive confirmations such as deleting
 * a route), and `ghost` (bordered/transparent, for cancel).
 *
 * @param variant - Visual style of the button; defaults to `primary`.
 */
export function DialogButton({
  variant = 'primary',
  onClick,
  children,
}: {
  variant?: 'primary' | 'ghost' | 'danger';
  onClick: () => void;
  children: ReactNode;
}) {
  const bg =
    variant === 'primary' ? 'var(--accent)' : variant === 'danger' ? 'var(--error)' : 'transparent';
  return (
    <button
      onClick={onClick}
      style={{
        padding: '6px 14px',
        borderRadius: 'var(--radius-sm)',
        fontSize: 'var(--text-sm)',
        fontWeight: 500,
        background: bg,
        color: variant === 'ghost' ? 'var(--text-secondary)' : 'var(--text-on-accent)',
        border: variant === 'ghost' ? '1px solid var(--border)' : '1px solid transparent',
        transition: 'filter var(--dur-fast) var(--ease-out)',
      }}
      onMouseEnter={(e) => (e.currentTarget.style.filter = 'brightness(1.08)')}
      onMouseLeave={(e) => (e.currentTarget.style.filter = 'none')}
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
        className="w-full"
        style={{
          padding: '7px 10px',
          borderRadius: 'var(--radius-sm)',
          fontFamily: mono ? 'var(--font-mono)' : 'var(--font-sans)',
          fontSize: 'var(--text-sm)',
          background: 'var(--surface-input)',
          color: 'var(--text-primary)',
          border: '1px solid var(--border)',
        }}
      />
    </div>
  );
}
