/**
 * Transient success/error notification for the admin UI, used to report the
 * outcome of admin API calls (route saved, config reloaded, request failed).
 * A single toast is shown at a time in the bottom-right corner.
 *
 * @module components/Toast
 */
import { useEffect } from 'react';
import { CircleCheck, CircleX, X } from 'lucide-react';

/** Payload describing one notification. */
export interface ToastData {
  /** Visual tone: `success` (green check) or `error` (red cross). */
  tone: 'success' | 'error';
  /** Bold headline, e.g. the action that succeeded or failed. */
  title: string;
  /** Optional monospace detail line, typically the admin API error message. */
  message?: string;
}

/** Props for Toast. */
interface ToastProps {
  /** The toast to display, or null to render nothing. */
  toast: ToastData | null;
  /** Clears the toast; called by the close button and the auto-dismiss timer. */
  onDismiss: () => void;
}

/**
 * Transient feedback card — replaces the gateway's old native alert().
 * Auto-dismisses after 5 seconds (timer resets whenever a new toast arrives)
 * and can be dismissed early via the close button.
 */
export function Toast({ toast, onDismiss }: ToastProps) {
  useEffect(() => {
    if (!toast) return;
    const t = setTimeout(onDismiss, 5000);
    return () => clearTimeout(t);
  }, [toast, onDismiss]);

  if (!toast) return null;

  const color = toast.tone === 'success' ? 'var(--success)' : 'var(--error)';
  const Icon = toast.tone === 'success' ? CircleCheck : CircleX;

  return (
    <div
      className="fixed bottom-4 right-4 flex items-start gap-2.5"
      style={{
        zIndex: 70,
        width: 320,
        padding: '12px 14px',
        background: 'var(--surface-raised)',
        border: '1px solid var(--border)',
        borderLeft: `2px solid ${color}`,
        borderRadius: 'var(--radius-md)',
        boxShadow: 'var(--shadow-lg)',
      }}
      role="status"
    >
      <Icon size={16} style={{ color, flexShrink: 0, marginTop: 1 }} />
      <div className="flex-1 min-w-0">
        <p
          style={{
            fontSize: 'var(--text-sm)',
            fontWeight: 600,
            color: 'var(--text-primary)',
            margin: 0,
          }}
        >
          {toast.title}
        </p>
        {toast.message && (
          <p
            style={{
              fontFamily: 'var(--font-mono)',
              fontSize: 'var(--text-xs)',
              color: 'var(--text-secondary)',
              margin: '2px 0 0',
              overflowWrap: 'anywhere',
            }}
          >
            {toast.message}
          </p>
        )}
      </div>
      <button
        onClick={onDismiss}
        className="flex items-center justify-center rounded shrink-0"
        style={{ width: 20, height: 20, color: 'var(--text-muted)' }}
        aria-label="Dismiss"
      >
        <X size={13} />
      </button>
    </div>
  );
}
