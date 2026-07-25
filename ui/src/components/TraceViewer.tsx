/**
 * Step-through viewer for a policy-execution trace.
 *
 * Shared by both Debug panel tabs: a live traced request and a sandbox run
 * return the identical trace shape, so this component is written once and
 * serves both.
 *
 * @module components/TraceViewer
 */
import { useState } from 'react';
import { AlertTriangle, ChevronDown, ChevronRight } from 'lucide-react';
import type { Change, EdgeKind, NodeStep, TraceDetail } from '../types';
import { getPluginMeta } from '../pluginMeta';
import { formatDuration } from '../format';

/** Props for {@link TraceViewer}. */
interface TraceViewerProps {
  /** The trace to render, from GET /api/debug/traces/{id} or a sandbox run. */
  trace: TraceDetail;
}

/** Human wording for each edge the engine can follow after a node. */
const EDGE_LABEL: Record<EdgeKind, string> = {
  success: 'success →',
  error: 'error →',
  catch_all: 'catch-all →',
  terminal: 'terminal (response sent)',
  end_of_chain: 'end of chain',
  unhandled: 'unhandled error → generic 500',
  node_not_found: 'node not found',
};

/** Edges that mean something went wrong, so they can be coloured as such. */
const EDGE_IS_PROBLEM: Partial<Record<EdgeKind, boolean>> = {
  unhandled: true,
  node_not_found: true,
};

/** Colour token for a change kind. */
function changeColor(kind: Change['kind']): string {
  if (kind === 'added') return 'var(--success, #22c55e)';
  if (kind === 'removed') return 'var(--error)';
  return 'var(--accent-hover)';
}

/** One `path / before / after` row in the Changes pane. */
function ChangeRow({ change }: { change: Change }) {
  return (
    <div
      style={{
        display: 'grid',
        gridTemplateColumns: '1fr auto',
        gap: 8,
        padding: '6px 8px',
        borderRadius: 'var(--radius-sm)',
        background: 'var(--surface-sunken)',
        border: '1px solid var(--border-subtle)',
      }}
    >
      <div className="min-w-0">
        <div
          className="truncate"
          style={{
            fontFamily: 'var(--font-mono)',
            fontSize: 'var(--text-xs)',
            color: 'var(--text-primary)',
          }}
        >
          {change.path}
        </div>
        <div
          style={{
            fontFamily: 'var(--font-mono)',
            fontSize: 'var(--text-2xs)',
            color: 'var(--text-secondary)',
            marginTop: 2,
            overflowWrap: 'anywhere',
          }}
        >
          {change.before !== undefined && (
            <span style={{ textDecoration: 'line-through', opacity: 0.75 }}>{change.before}</span>
          )}
          {change.before !== undefined && change.after !== undefined && <span> → </span>}
          {change.after !== undefined && <span>{change.after}</span>}
        </div>
      </div>
      <span
        style={{
          alignSelf: 'start',
          fontSize: 'var(--text-2xs)',
          fontWeight: 500,
          color: changeColor(change.kind),
          whiteSpace: 'nowrap',
        }}
      >
        {change.kind}
      </span>
    </div>
  );
}

/**
 * Left step rail + right detail pane.
 *
 * The Changes pane leads because "what did this plugin do" is the question a
 * policy author actually has; the full context snapshot is one collapse away
 * for when the answer is not enough.
 */
export function TraceViewer({ trace }: TraceViewerProps) {
  // Callers key this component by trace id, so a different trace remounts it
  // and both start fresh — the rail can never point past the end.
  const [selected, setSelected] = useState(0);
  const [showSnapshot, setShowSnapshot] = useState(false);

  const step: NodeStep | undefined = trace.steps[selected];

  return (
    <div style={{ display: 'flex', gap: 12, minHeight: 320 }}>
      {/* Step rail */}
      <div
        style={{
          width: 240,
          flexShrink: 0,
          maxHeight: '54vh',
          overflowY: 'auto',
          display: 'flex',
          flexDirection: 'column',
          gap: 4,
        }}
        onKeyDown={(e) => {
          if (e.key === 'ArrowDown' || e.key === 'ArrowRight') {
            setSelected((i) => Math.min(i + 1, trace.steps.length - 1));
          } else if (e.key === 'ArrowUp' || e.key === 'ArrowLeft') {
            setSelected((i) => Math.max(i - 1, 0));
          }
        }}
        tabIndex={0}
        role="listbox"
        aria-label="Trace steps"
      >
        {trace.steps.length === 0 && (
          <p style={{ fontSize: 'var(--text-xs)', color: 'var(--text-muted)', padding: 8 }}>
            No nodes executed.
          </p>
        )}
        {trace.steps.map((s, i) => {
          const meta = getPluginMeta(s.node_type);
          const Icon = meta.icon;
          const isSelected = i === selected;
          const failed = s.outcome.kind === 'error';
          return (
            <button
              key={`${s.index}-${s.node_id}`}
              role="option"
              aria-selected={isSelected}
              onClick={() => setSelected(i)}
              className="w-full text-left flex items-center transition-colors"
              style={{
                gap: 8,
                padding: '7px 9px',
                borderRadius: 'var(--radius-sm)',
                background: isSelected ? 'var(--surface-active)' : 'var(--surface-raised)',
                boxShadow: isSelected ? 'inset 0 0 0 1px var(--accent-ring)' : 'none',
                border: '1px solid var(--border-subtle)',
                color: 'var(--text-primary)',
              }}
            >
              <span
                className="flex items-center justify-center shrink-0"
                style={{
                  width: 22,
                  height: 22,
                  borderRadius: 'var(--radius-sm)',
                  background: `color-mix(in srgb, ${meta.color} 18%, transparent)`,
                  color: meta.color,
                }}
              >
                <Icon size={12} strokeWidth={1.75} />
              </span>
              <span className="flex flex-col min-w-0" style={{ flex: 1 }}>
                <span
                  className="truncate"
                  style={{
                    fontFamily: 'var(--font-mono)',
                    fontSize: 'var(--text-xs)',
                    fontWeight: 500,
                  }}
                >
                  {s.node_id}
                </span>
                <span
                  className="truncate"
                  style={{ fontSize: 'var(--text-2xs)', color: 'var(--text-muted)' }}
                >
                  {s.node_type} · {formatDuration(s.duration_us)}
                </span>
              </span>
              {failed && <AlertTriangle size={12} style={{ color: 'var(--error)' }} />}
            </button>
          );
        })}
      </div>

      {/* Detail pane */}
      <div style={{ flex: 1, minWidth: 0, maxHeight: '54vh', overflowY: 'auto' }}>
        {!step && (
          <p style={{ fontSize: 'var(--text-sm)', color: 'var(--text-muted)' }}>
            Select a step to see what it changed.
          </p>
        )}
        {step && (
          <>
            <div style={{ marginBottom: 10 }}>
              <div
                style={{
                  fontFamily: 'var(--font-mono)',
                  fontSize: 'var(--text-sm)',
                  color: 'var(--text-primary)',
                }}
              >
                {step.node_id}{' '}
                <span style={{ color: 'var(--text-muted)' }}>({step.node_type})</span>
              </div>
              <div
                style={{
                  fontSize: 'var(--text-2xs)',
                  color: EDGE_IS_PROBLEM[step.edge] ? 'var(--error)' : 'var(--text-secondary)',
                  marginTop: 2,
                }}
              >
                {EDGE_LABEL[step.edge]}
                {step.next_node_id ? ` ${step.next_node_id}` : ''}
                {step.outcome.kind === 'error' && (
                  <>
                    {' · '}
                    <span style={{ color: 'var(--error)' }}>
                      {step.outcome.code}: {step.outcome.message}
                    </span>
                  </>
                )}
              </div>
            </div>

            <div className="eyebrow" style={{ marginBottom: 6 }}>
              Changes
            </div>
            {step.changes.length === 0 ? (
              <p
                style={{
                  fontSize: 'var(--text-xs)',
                  color: 'var(--text-muted)',
                  margin: '0 0 12px',
                }}
              >
                This node did not modify the context.
              </p>
            ) : (
              <div className="space-y-1.5" style={{ marginBottom: 12 }}>
                {step.changes.map((c, i) => (
                  <ChangeRow key={`${c.path}-${i}`} change={c} />
                ))}
              </div>
            )}

            <button
              onClick={() => setShowSnapshot((v) => !v)}
              className="flex items-center gap-1"
              style={{
                background: 'transparent',
                color: 'var(--text-secondary)',
                fontSize: 'var(--text-xs)',
                padding: 0,
              }}
              aria-expanded={showSnapshot}
            >
              {showSnapshot ? <ChevronDown size={13} /> : <ChevronRight size={13} />}
              Context after this node
            </button>
            {showSnapshot && (
              <pre
                style={{
                  margin: '8px 0 0',
                  padding: 10,
                  maxHeight: 260,
                  overflow: 'auto',
                  borderRadius: 'var(--radius-sm)',
                  background: 'var(--surface-input)',
                  border: '1px solid var(--border)',
                  fontFamily: 'var(--font-mono)',
                  fontSize: 'var(--text-2xs)',
                  lineHeight: 1.5,
                  color: 'var(--text-primary)',
                }}
              >
                {JSON.stringify(step.after, null, 2)}
              </pre>
            )}
          </>
        )}
      </div>
    </div>
  );
}

/** Compact header describing a trace, shown above the viewer. */
export function TraceHeader({
  trace,
  onCopyToSandbox,
}: {
  trace: TraceDetail;
  /** When set, shows a button that replays this trace's request in the sandbox. */
  onCopyToSandbox?: () => void;
}) {
  return (
    <div style={{ marginBottom: 10 }}>
      <div className="flex items-center justify-between" style={{ gap: 8 }}>
        <div
          style={{
            fontFamily: 'var(--font-mono)',
            fontSize: 'var(--text-sm)',
            color: 'var(--text-primary)',
          }}
        >
          {trace.method} {trace.path} · {trace.status}
        </div>
        {onCopyToSandbox && (
          <button
            onClick={onCopyToSandbox}
            title="Load this request's context into the Sandbox tab to replay or tweak it"
            style={{
              flexShrink: 0,
              padding: '3px 10px',
              borderRadius: 'var(--radius-sm)',
              fontSize: 'var(--text-2xs)',
              fontWeight: 500,
              background: 'var(--surface-input)',
              color: 'var(--text-primary)',
              border: '1px solid var(--border)',
            }}
            onMouseEnter={(e) => (e.currentTarget.style.filter = 'brightness(1.08)')}
            onMouseLeave={(e) => (e.currentTarget.style.filter = 'none')}
          >
            Copy to sandbox
          </button>
        )}
      </div>
      <div style={{ fontSize: 'var(--text-2xs)', color: 'var(--text-muted)', marginTop: 2 }}>
        policy {trace.policy}
        {trace.route ? ` · route ${trace.route}` : ''} · {trace.steps.length} nodes ·{' '}
        {formatDuration(trace.duration_us)}
        {!trace.captured_bodies && ' · bodies not captured'}
      </div>
      {trace.notes.length > 0 && (
        <div style={{ fontSize: 'var(--text-2xs)', color: 'var(--error)', marginTop: 4 }}>
          {trace.notes.join(' · ')}
        </div>
      )}
    </div>
  );
}
