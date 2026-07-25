/**
 * Debug panel: browse policy-execution traces and run the plugin sandbox.
 *
 * Rendered inside the shared {@link Dialog}, with two tabs. Both render their
 * result through the same {@link TraceViewer}, because a live trace and a
 * sandbox run return the identical shape.
 *
 * @module components/DebugPanel
 */
import { useCallback, useEffect, useState } from 'react';
import { Dialog, DialogButton } from './Dialog';
import { TraceHeader, TraceViewer } from './TraceViewer';
import { formatDuration } from '../format';
import { api } from '../api/client';
import type { DebugConfig, Policy, TraceDetail, TraceSummary } from '../types';

/** Props for {@link DebugPanel}. */
interface DebugPanelProps {
  /** Whether the dialog is shown. */
  open: boolean;
  /** Closes the dialog. */
  onClose: () => void;
  /** Effective debug settings, or null while still loading. */
  config: DebugConfig | null;
  /** Policies available to replay in the sandbox. */
  policies: Policy[];
  /** Policy pre-selected in the sandbox tab (the route open on the canvas). */
  selectedPolicy: string | null;
  /** Surfaces errors through the app's toast. */
  onError: (title: string, message: string) => void;
}

type Tab = 'traces' | 'sandbox';
type SandboxMode = 'policy' | 'nodes';

const DEFAULT_CONTEXT = `{
  "method": "GET",
  "path": "/api/hello",
  "headers": {}
}`;

const tabButtonStyle = (active: boolean): React.CSSProperties => ({
  padding: '5px 12px',
  borderRadius: 'var(--radius-sm)',
  fontSize: 'var(--text-sm)',
  fontWeight: 500,
  background: active ? 'var(--surface-active)' : 'transparent',
  color: active ? 'var(--text-primary)' : 'var(--text-secondary)',
  boxShadow: active ? 'inset 0 0 0 1px var(--accent-ring)' : 'none',
});

const inputStyle: React.CSSProperties = {
  width: '100%',
  padding: '6px 10px',
  borderRadius: 'var(--radius-sm)',
  fontFamily: 'var(--font-mono)',
  fontSize: 'var(--text-xs)',
  background: 'var(--surface-input)',
  color: 'var(--text-primary)',
  border: '1px solid var(--border)',
};

/** Panel shown instead of the tabs when debug mode is off. */
function DisabledNotice({ config }: { config: DebugConfig | null }) {
  return (
    <div style={{ padding: '8px 0' }}>
      <p style={{ fontSize: 'var(--text-sm)', color: 'var(--text-secondary)', marginTop: 0 }}>
        Debug mode is off. Enable it in <code>system.yaml</code> and restart the gateway —
        it is deliberately not switchable at runtime, so a compromised Admin API credential
        cannot start capturing request contexts.
      </p>
      <pre
        style={{
          margin: '10px 0 0',
          padding: 12,
          borderRadius: 'var(--radius-sm)',
          background: 'var(--surface-input)',
          border: '1px solid var(--border)',
          fontFamily: 'var(--font-mono)',
          fontSize: 'var(--text-xs)',
          color: 'var(--text-primary)',
        }}
      >
        {`debug:\n  enabled: \${FEATHERBIT_DEBUG:-false}\n  capture_bodies: \${FEATHERBIT_DEBUG_BODIES:-false}`}
      </pre>
      {config && (
        <p style={{ fontSize: 'var(--text-2xs)', color: 'var(--text-muted)', marginBottom: 0 }}>
          Then send <code>{config.trigger_header}: 1</code> on a request to trace it.
        </p>
      )}
    </div>
  );
}

/**
 * Two-tab debug surface.
 *
 * Traces polls on open rather than streaming — a developer reproducing a
 * request refreshes deliberately, and it keeps the transport a plain fetch.
 */
export function DebugPanel({
  open,
  onClose,
  config,
  policies,
  selectedPolicy,
  onError,
}: DebugPanelProps) {
  const [tab, setTab] = useState<Tab>('traces');

  // Traces tab
  const [traces, setTraces] = useState<TraceSummary[]>([]);
  const [detail, setDetail] = useState<TraceDetail | null>(null);
  // Filter the recent-requests buffer down to one policy. '' = all.
  const [filterPolicy, setFilterPolicy] = useState('');

  // Sandbox tab
  const [mode, setMode] = useState<SandboxMode>('policy');
  const [policyName, setPolicyName] = useState('');
  const [nodesJson, setNodesJson] = useState('[]');
  const [contextJson, setContextJson] = useState(DEFAULT_CONTEXT);
  const [onErrorMode, setOnErrorMode] = useState<'stop' | 'client'>('stop');
  const [result, setResult] = useState<TraceDetail | null>(null);
  const [running, setRunning] = useState(false);

  const enabled = config?.enabled ?? false;

  const refresh = useCallback(async () => {
    try {
      setTraces(await api.listTraces());
    } catch (e) {
      onError('Failed to load traces', `${e}`);
    }
  }, [onError]);

  // Load on open, then poll while the Traces tab is showing so requests that
  // arrive after the panel is already open appear without a manual refresh —
  // this is what makes it usable as a live "recent requests" view.
  useEffect(() => {
    if (!open || !enabled) return;
    refresh();
    setDetail(null);
  }, [open, enabled, refresh]);

  useEffect(() => {
    if (!open || !enabled || tab !== 'traces') return;
    const timer = setInterval(refresh, 2500);
    return () => clearInterval(timer);
  }, [open, enabled, tab, refresh]);

  // Seed the sandbox from whatever the user has open on the canvas: the common
  // case is "test the policy I am editing", which should be one click.
  useEffect(() => {
    if (!open) return;
    const initial = selectedPolicy ?? policies[0]?.name ?? '';
    setPolicyName(initial);
    const p = policies.find((x) => x.name === initial);
    if (p) {
      const userNodes = p.nodes.filter((n) => n.type !== 'listener' && n.type !== 'client');
      setNodesJson(JSON.stringify(userNodes, null, 2));
    }
  }, [open, selectedPolicy, policies]);

  // Distinct policies present in the buffer, with counts, so the filter offers
  // only policies that actually have recent requests.
  const policyOptions = (() => {
    const counts = new Map<string, number>();
    for (const t of traces) counts.set(t.policy, (counts.get(t.policy) ?? 0) + 1);
    return [...counts.entries()]
      .map(([name, count]) => ({ name, count }))
      .sort((a, b) => a.name.localeCompare(b.name));
  })();

  const visibleTraces = filterPolicy
    ? traces.filter((t) => t.policy === filterPolicy)
    : traces;

  const openTrace = async (id: string) => {
    try {
      setDetail(await api.getTrace(id));
    } catch (e) {
      onError('Failed to load trace', `${e}`);
    }
  };

  // Replay a traced request in the sandbox: drop its (redacted) initial context
  // into the Sandbox tab. The backend accepts the snapshot shape directly, so
  // no reshaping is needed here. If the trace ran a real policy, pre-select it.
  const copyTraceToSandbox = (trace: TraceDetail) => {
    setContextJson(JSON.stringify(trace.initial, null, 2));
    if (policies.some((p) => p.name === trace.policy)) {
      setMode('policy');
      setPolicyName(trace.policy);
    }
    setTab('sandbox');
  };

  const clear = async () => {
    try {
      await api.clearTraces();
      setDetail(null);
      await refresh();
    } catch (e) {
      onError('Failed to clear traces', `${e}`);
    }
  };

  const run = async () => {
    setRunning(true);
    setResult(null);
    try {
      const context = contextJson.trim() ? JSON.parse(contextJson) : {};
      const body =
        mode === 'policy'
          ? { policy: policyName, context }
          : { nodes: JSON.parse(nodesJson), on_error: onErrorMode, context };
      const res = await api.runSandbox(body);
      setResult(res.trace);
    } catch (e) {
      onError('Sandbox run failed', `${e}`);
    } finally {
      setRunning(false);
    }
  };

  return (
    <Dialog
      open={open}
      title="Debug"
      width={1040}
      onClose={onClose}
      footer={
        <>
          {enabled && tab === 'traces' && (
            <>
              <DialogButton variant="ghost" onClick={clear}>
                Clear
              </DialogButton>
              <DialogButton variant="ghost" onClick={refresh}>
                Refresh
              </DialogButton>
            </>
          )}
          {enabled && tab === 'sandbox' && (
            <DialogButton onClick={run}>{running ? 'Running…' : 'Run'}</DialogButton>
          )}
          <DialogButton variant="ghost" onClick={onClose}>
            Close
          </DialogButton>
        </>
      }
    >
      {!enabled ? (
        <DisabledNotice config={config} />
      ) : (
        <>
          <div className="flex" style={{ gap: 4, marginBottom: 12 }}>
            <button onClick={() => setTab('traces')} style={tabButtonStyle(tab === 'traces')}>
              Traces
            </button>
            <button onClick={() => setTab('sandbox')} style={tabButtonStyle(tab === 'sandbox')}>
              Sandbox
            </button>
          </div>

          {tab === 'traces' && (
            <div style={{ display: 'flex', gap: 12 }}>
              <div
                style={{
                  width: 300,
                  flexShrink: 0,
                  maxHeight: '58vh',
                  overflowY: 'auto',
                  display: 'flex',
                  flexDirection: 'column',
                  gap: 4,
                }}
              >
                {/* Filter the recent-requests buffer to one policy. Distinct
                    values come from the buffer itself, so only policies that
                    actually have traces are offered. */}
                {policyOptions.length > 1 && (
                  <select
                    value={filterPolicy}
                    onChange={(e) => setFilterPolicy(e.target.value)}
                    style={{ ...inputStyle, appearance: 'auto', marginBottom: 4 }}
                    aria-label="Filter by policy"
                  >
                    <option value="">All policies ({traces.length})</option>
                    {policyOptions.map((p) => (
                      <option key={p.name} value={p.name}>
                        {p.name} ({p.count})
                      </option>
                    ))}
                  </select>
                )}
                {traces.length === 0 && (
                  <p style={{ fontSize: 'var(--text-xs)', color: 'var(--text-muted)' }}>
                    No traces yet. Send a request with{' '}
                    <code>{config?.trigger_header}: 1</code>, or set{' '}
                    <code>trace_all</code> to capture every request.
                  </p>
                )}
                {traces.length > 0 && visibleTraces.length === 0 && (
                  <p style={{ fontSize: 'var(--text-xs)', color: 'var(--text-muted)' }}>
                    No recent requests on this policy.
                  </p>
                )}
                {visibleTraces.map((t) => (
                  <button
                    key={t.id}
                    onClick={() => openTrace(t.id)}
                    className="w-full text-left transition-colors"
                    style={{
                      padding: '7px 9px',
                      borderRadius: 'var(--radius-sm)',
                      background:
                        detail?.id === t.id ? 'var(--surface-active)' : 'var(--surface-raised)',
                      boxShadow:
                        detail?.id === t.id ? 'inset 0 0 0 1px var(--accent-ring)' : 'none',
                      border: '1px solid var(--border-subtle)',
                      color: 'var(--text-primary)',
                    }}
                  >
                    <div
                      className="truncate"
                      style={{ fontFamily: 'var(--font-mono)', fontSize: 'var(--text-xs)' }}
                    >
                      {t.method} {t.path}
                    </div>
                    <div
                      style={{ fontSize: 'var(--text-2xs)', color: 'var(--text-muted)', marginTop: 2 }}
                    >
                      {t.status} · {t.step_count} nodes · {formatDuration(t.duration_us)}
                      {t.source === 'sandbox' && ' · sandbox'}
                      {t.error_count > 0 && ` · ${t.error_count} err`}
                    </div>
                  </button>
                ))}
              </div>
              <div style={{ flex: 1, minWidth: 0 }}>
                {detail ? (
                  <>
                    <TraceHeader trace={detail} onCopyToSandbox={() => copyTraceToSandbox(detail)} />
                    {/* Keyed by id: a different trace remounts the viewer with fresh step state. */}
                    <TraceViewer key={detail.id} trace={detail} />
                  </>
                ) : (
                  <p style={{ fontSize: 'var(--text-sm)', color: 'var(--text-muted)' }}>
                    Select a trace to step through its nodes.
                  </p>
                )}
              </div>
            </div>
          )}

          {tab === 'sandbox' && (
            <div>
              <p
                style={{
                  fontSize: 'var(--text-2xs)',
                  color: 'var(--error)',
                  margin: '0 0 10px',
                }}
              >
                Plugins run for real: outbound calls are made and shared rate-limit and
                circuit-breaker state is mutated.
              </p>

              <div className="flex" style={{ gap: 4, marginBottom: 10 }}>
                <button onClick={() => setMode('policy')} style={tabButtonStyle(mode === 'policy')}>
                  Policy
                </button>
                <button onClick={() => setMode('nodes')} style={tabButtonStyle(mode === 'nodes')}>
                  Nodes
                </button>
              </div>

              <div style={{ display: 'flex', gap: 12 }}>
                <div style={{ width: 380, flexShrink: 0 }}>
                  {mode === 'policy' ? (
                    <label style={{ display: 'block', marginBottom: 10 }}>
                      <span className="eyebrow">Policy</span>
                      <select
                        value={policyName}
                        onChange={(e) => setPolicyName(e.target.value)}
                        style={{ ...inputStyle, appearance: 'auto', marginTop: 4 }}
                      >
                        {policies.map((p) => (
                          <option key={p.name} value={p.name}>
                            {p.name}
                          </option>
                        ))}
                      </select>
                    </label>
                  ) : (
                    <>
                      <label style={{ display: 'block', marginBottom: 10 }}>
                        <span className="eyebrow">Nodes (JSON)</span>
                        <textarea
                          value={nodesJson}
                          onChange={(e) => setNodesJson(e.target.value)}
                          rows={10}
                          style={{ ...inputStyle, marginTop: 4 }}
                        />
                      </label>
                      <label style={{ display: 'block', marginBottom: 10 }}>
                        <span className="eyebrow">On error</span>
                        <select
                          value={onErrorMode}
                          onChange={(e) => setOnErrorMode(e.target.value as 'stop' | 'client')}
                          style={{ ...inputStyle, appearance: 'auto', marginTop: 4 }}
                        >
                          <option value="stop">stop — leave error ports unwired</option>
                          <option value="client">client — wire errors to client</option>
                        </select>
                      </label>
                    </>
                  )}
                  <label style={{ display: 'block' }}>
                    <span className="eyebrow">Context (JSON)</span>
                    <textarea
                      value={contextJson}
                      onChange={(e) => setContextJson(e.target.value)}
                      rows={8}
                      style={{ ...inputStyle, marginTop: 4 }}
                    />
                  </label>
                  <p
                    style={{
                      fontSize: 'var(--text-2xs)',
                      color: 'var(--text-muted)',
                      margin: '6px 0 0',
                    }}
                  >
                    Testing a response-phase plugin (e.g. <code>proxy-rewrite</code> with{' '}
                    <code>phase: response</code>)? The synthetic response starts empty, so seed
                    one for the plugin to act on:{' '}
                    <button
                      type="button"
                      onClick={() =>
                        setContextJson(
                          JSON.stringify(
                            {
                              ...(() => {
                                try {
                                  return JSON.parse(contextJson || '{}');
                                } catch {
                                  return {};
                                }
                              })(),
                              response: {
                                status_code: 200,
                                headers: { 'x-powered-by': 'demo' },
                              },
                            },
                            null,
                            2,
                          ),
                        )
                      }
                      style={{
                        background: 'transparent',
                        color: 'var(--accent-hover)',
                        padding: 0,
                        fontSize: 'var(--text-2xs)',
                        textDecoration: 'underline',
                      }}
                    >
                      add a response
                    </button>
                    .
                  </p>
                </div>
                <div style={{ flex: 1, minWidth: 0 }}>
                  {result ? (
                    <>
                      <TraceHeader trace={result} />
                      {/* Keyed by id: each sandbox run remounts the viewer with fresh step state. */}
                      <TraceViewer key={result.id} trace={result} />
                    </>
                  ) : (
                    <p style={{ fontSize: 'var(--text-sm)', color: 'var(--text-muted)' }}>
                      Run to see each node's effect on the context.
                    </p>
                  )}
                </div>
              </div>
            </div>
          )}
        </>
      )}
    </Dialog>
  );
}
