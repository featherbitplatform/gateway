import { useState } from 'react';
import { ChevronDown, ChevronRight, Loader2 } from 'lucide-react';
import type { ChatMessage, ToolCall } from '../../chat/store';
import { isWriteTool, needsConfirmation } from '../../chat/loop';

type ToolMessage = Extract<ChatMessage, { role: 'tool' }>;

interface ToolCallCardProps {
  call: ToolCall;
  /** The stored result, once the call finished (done/declined/error). */
  result: ToolMessage | undefined;
  /** True while this exact call awaits Run/Skip. */
  awaitingConfirm: boolean;
  onRun: () => void;
  onSkip: () => void;
}

function prettyArgs(raw: string): string {
  try {
    return JSON.stringify(JSON.parse(raw), null, 2);
  } catch {
    return raw;
  }
}

const statusColor: Record<ToolMessage['status'], string> = {
  done: 'var(--success)',
  declined: 'var(--warning)',
  error: 'var(--error)',
};

export function ToolCallCard({ call, result, awaitingConfirm, onRun, onSkip }: ToolCallCardProps) {
  const [open, setOpen] = useState(false);
  const write = isWriteTool(call.name);
  // `run_sandbox` reads nothing back into the config but executes nodes for
  // real, so it is gated like a write without being labelled as one.
  const confirms = !write && needsConfirmation(call.name);
  return (
    <div
      data-testid={`tool-call-${call.name}`}
      style={{
        border: '1px solid var(--border)',
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
        <span style={{ fontFamily: 'var(--font-mono)', color: 'var(--text-primary)' }}>
          {write ? 'write · ' : confirms ? 'confirm · ' : ''}
          {call.name}
        </span>
        {result ? (
          <span style={{ color: statusColor[result.status] }}>{result.status}</span>
        ) : awaitingConfirm ? (
          <span style={{ color: 'var(--warning)' }}>awaiting confirmation</span>
        ) : (
          <span className="flex items-center gap-1" style={{ color: 'var(--text-muted)' }}>
            <Loader2 size={11} className="animate-spin" /> running
          </span>
        )}
      </div>
      <pre style={{ margin: 0, whiteSpace: 'pre-wrap', wordBreak: 'break-all', color: 'var(--text-secondary)', fontFamily: 'var(--font-mono)' }}>
        {prettyArgs(call.arguments)}
      </pre>
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
      {result && (
        <>
          <button
            onClick={() => setOpen((o) => !o)}
            className="flex items-center gap-1"
            style={{ background: 'transparent', border: 'none', color: 'var(--text-muted)', padding: 0, alignSelf: 'flex-start' }}
          >
            {open ? <ChevronDown size={11} /> : <ChevronRight size={11} />} result
          </button>
          {open && (
            <pre style={{ margin: 0, whiteSpace: 'pre-wrap', wordBreak: 'break-all', maxHeight: 240, overflowY: 'auto', fontFamily: 'var(--font-mono)' }}>
              {result.content}
            </pre>
          )}
          {result.status === 'declined' && !open && <span style={{ color: 'var(--text-muted)' }}>{result.content}</span>}
        </>
      )}
    </div>
  );
}
