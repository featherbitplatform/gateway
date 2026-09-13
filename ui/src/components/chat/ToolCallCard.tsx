import { useState } from 'react';
import { ChevronDown, ChevronRight, Loader2 } from 'lucide-react';
import type { ToolGroup } from '../../chat/attempts';
import { isWriteTool, needsConfirmation } from '../../chat/loop';
import { formatPayload, isEmptyPayload, type PayloadBlock } from '../../chat/payload';

interface ToolCallCardProps {
  /** All attempts of one tool (retries folded); the last one is current. */
  group: ToolGroup;
  /** True while the current attempt awaits Run/Skip. */
  awaitingConfirm: boolean;
  onRun: () => void;
  onSkip: () => void;
}

/** First line of an error payload, for the folded attempt list. */
function errorSummary(content: string): string {
  try {
    const v = JSON.parse(content) as { message?: string; code?: string };
    if (v && typeof v === 'object' && (v.message || v.code)) return [v.code, v.message].filter(Boolean).join(': ');
  } catch {
    // plain text
  }
  const line = content.split('\n')[0];
  return line.length > 160 ? `${line.slice(0, 160)}…` : line;
}

/** The formatted blocks of one payload, each a fenced-style code block. */
function Payload({ blocks, maxHeight }: { blocks: PayloadBlock[]; maxHeight?: number }) {
  return (
    <>
      {blocks.map((b, i) => (
        <div key={i} style={{ display: 'flex', flexDirection: 'column', gap: 2 }}>
          {b.label && <span style={{ color: 'var(--text-muted)' }}>{b.label}</span>}
          <pre
            style={{
              margin: 0,
              padding: 8,
              borderRadius: 'var(--radius-sm)',
              background: 'var(--surface)',
              border: '1px solid var(--border)',
              overflow: 'auto',
              maxHeight,
              fontFamily: 'var(--font-mono)',
              fontSize: 'var(--text-2xs)',
              whiteSpace: 'pre',
              color: 'var(--text-primary)',
            }}
          >
            <code className={`language-${b.lang}`}>{b.text}</code>
          </pre>
        </div>
      ))}
    </>
  );
}

const statusColor: Record<'done' | 'declined' | 'error', string> = {
  done: 'var(--success)',
  declined: 'var(--warning)',
  error: 'var(--error)',
};

const toggleStyle: React.CSSProperties = {
  background: 'transparent',
  border: 'none',
  color: 'var(--text-muted)',
  padding: 0,
  alignSelf: 'flex-start',
};

export function ToolCallCard({ group, awaitingConfirm, onRun, onSkip }: ToolCallCardProps) {
  const [showArgs, setShowArgs] = useState(false);
  const [showResult, setShowResult] = useState(false);
  const [showPrevious, setShowPrevious] = useState(false);

  const attempts = group.attempts;
  const current = attempts[attempts.length - 1];
  const previous = attempts.slice(0, -1);
  const { call, result } = current;
  const write = isWriteTool(group.name);
  // `run_sandbox` reads nothing back into the config but executes nodes for
  // real, so it is gated like a write without being labelled as one.
  const confirms = !write && needsConfirmation(group.name);
  const running = !result && !awaitingConfirm;
  const hasArgs = !isEmptyPayload(call.arguments);
  const args = formatPayload(call.arguments);
  const resultBlocks = result ? formatPayload(result.content) : [];
  // Arguments are worth a glance while the call is pending (that is what the
  // Run/Skip decision is about); afterwards they fold away behind a toggle.
  const argsOpen = hasArgs && (awaitingConfirm || running || showArgs);

  return (
    <div
      data-testid={`tool-call-${group.name}`}
      style={{
        border: `1px solid ${running || awaitingConfirm ? 'var(--accent)' : 'var(--border)'}`,
        borderRadius: 'var(--radius-sm)',
        background: 'var(--surface-input)',
        padding: '6px 8px',
        fontSize: 'var(--text-2xs)',
        display: 'flex',
        flexDirection: 'column',
        gap: 4,
      }}
    >
      <div className="flex items-center justify-between" style={{ gap: 8 }}>
        <span className="flex items-center gap-2" style={{ fontFamily: 'var(--font-mono)', color: 'var(--text-primary)' }}>
          {running && <Loader2 size={12} className="animate-spin" style={{ color: 'var(--accent)' }} aria-label="running" />}
          <span>
            {write ? 'write · ' : confirms ? 'confirm · ' : ''}
            {group.name}
          </span>
          {attempts.length > 1 && (
            <span style={{ color: 'var(--text-muted)', fontFamily: 'inherit' }} data-testid="tool-attempt">
              attempt {attempts.length}
            </span>
          )}
        </span>
        {result ? (
          <span style={{ color: statusColor[result.status] }}>{result.status}</span>
        ) : awaitingConfirm ? (
          <span style={{ color: 'var(--warning)' }}>awaiting confirmation</span>
        ) : (
          <span style={{ color: 'var(--accent)' }}>running…</span>
        )}
      </div>

      {previous.length > 0 && (
        <>
          <button onClick={() => setShowPrevious((o) => !o)} className="flex items-center gap-1" style={toggleStyle}>
            {showPrevious ? <ChevronDown size={11} /> : <ChevronRight size={11} />}
            {previous.length} failed {previous.length === 1 ? 'attempt' : 'attempts'}
          </button>
          {showPrevious && (
            <ol style={{ margin: 0, paddingLeft: 18, color: 'var(--text-muted)', display: 'flex', flexDirection: 'column', gap: 2 }}>
              {previous.map((a, i) => (
                <li key={a.call.id}>
                  <span style={{ color: 'var(--error)' }}>#{i + 1}</span> {a.result ? errorSummary(a.result.content) : 'no result'}
                </li>
              ))}
            </ol>
          )}
        </>
      )}

      {hasArgs && result && (
        <button onClick={() => setShowArgs((o) => !o)} className="flex items-center gap-1" style={toggleStyle}>
          {showArgs ? <ChevronDown size={11} /> : <ChevronRight size={11} />} arguments
        </button>
      )}
      {argsOpen && (
        <>
          {!result && <span style={{ color: 'var(--text-muted)' }}>arguments</span>}
          <Payload blocks={args} maxHeight={200} />
        </>
      )}

      {awaitingConfirm && (
        <div className="flex gap-2">
          <button onClick={onRun} style={{ padding: '3px 10px', borderRadius: 'var(--radius-sm)', background: 'var(--accent)', color: 'var(--text-on-accent)', border: 'none' }}>
            Run
          </button>
          <button onClick={onSkip} style={{ padding: '3px 10px', borderRadius: 'var(--radius-sm)', background: 'transparent', color: 'var(--text-primary)', border: '1px solid var(--border)' }}>
            Skip
          </button>
        </div>
      )}

      {result && resultBlocks.length > 0 && (
        <>
          <button onClick={() => setShowResult((o) => !o)} className="flex items-center gap-1" style={toggleStyle}>
            {showResult ? <ChevronDown size={11} /> : <ChevronRight size={11} />} result
          </button>
          {showResult && <Payload blocks={resultBlocks} maxHeight={280} />}
          {result.status === 'declined' && !showResult && <span style={{ color: 'var(--text-muted)' }}>{result.content}</span>}
        </>
      )}
    </div>
  );
}
