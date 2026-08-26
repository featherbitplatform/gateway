/**
 * Certificates panel: read-only status of ACME-managed certificates with a
 * per-row "Renew now". Mirrors SessionsPanel's dialog layout; unlike it, this
 * one polls (every 30 s while open) because issuance state changes on its own.
 *
 * @module components/CertificatesPanel
 */
import { useCallback, useEffect, useState } from 'react';
import type { CSSProperties } from 'react';
import { Dialog, DialogButton } from './Dialog';
import { formatUnixTime } from '../format';
import { expiryTone, formatExpiresIn } from '../certs';
import { api } from '../api/client';
import { parseApiError } from '../apiError';
import type { AcmeCert, AcmeCertsResponse } from '../types';

interface CertificatesPanelProps {
  open: boolean;
  onClose: () => void;
  onError: (title: string, message: string) => void;
}

const POLL_MS = 30_000;

const badgeColor: Record<AcmeCert['state'], string> = {
  placeholder: 'var(--text-muted)',
  issued: 'var(--success)',
  renewing: 'var(--accent)',
  failed: 'var(--error)',
};

const toneColor = {
  none: 'var(--text-muted)',
  ok: 'var(--text-primary)',
  warn: 'var(--warning)',
  danger: 'var(--error)',
} as const;

const cell: CSSProperties = {
  padding: '6px 8px',
  fontSize: 'var(--text-xs)',
  verticalAlign: 'top',
  borderBottom: '1px solid var(--border)',
};

function NotConfigured() {
  return (
    <p data-testid="acme-not-configured" style={{ fontSize: 'var(--text-sm)', color: 'var(--text-secondary)', margin: 0 }}>
      ACME is not configured — add an <code>acme:</code> block to <code>system.yaml</code> and mark a
      certificate slot <code>acme: {'{ domains: [...] }'}</code>, then restart.
    </p>
  );
}

export function CertificatesPanel({ open, onClose, onError }: CertificatesPanelProps) {
  const [data, setData] = useState<AcmeCertsResponse | null>(null);
  const [now, setNow] = useState(() => Math.floor(Date.now() / 1000));
  // Per-row two-step confirm: 'arm' after the first click, 'force' when the API said not_due.
  const [pending, setPending] = useState<{ id: string; stage: 'arm' | 'force' } | null>(null);
  const [expanded, setExpanded] = useState<string | null>(null);

  const load = useCallback(async () => {
    try {
      setData(await api.listAcmeCerts());
      setNow(Math.floor(Date.now() / 1000));
    } catch (e) {
      onError('Certificates', parseApiError(e).error);
    }
  }, [onError]);

  useEffect(() => {
    if (!open) return;
    // The setState calls inside `load` all land after an await, not
    // synchronously in this effect — mirrors SessionsPanel's identical
    // load-on-open effect and its documented rule false-positive.
    // eslint-disable-next-line react-hooks/set-state-in-effect
    void load();
    const t = setInterval(() => void load(), POLL_MS);
    return () => clearInterval(t);
  }, [open, load]);

  const renew = async (id: string, force: boolean) => {
    try {
      const res = await api.renewAcmeCert(id, force);
      if (!res.scheduled && res.reason === 'not_due') {
        setPending({ id, stage: 'force' });
        return;
      }
      setPending(null);
      await load();
    } catch (e) {
      setPending(null);
      onError('Renew certificate', parseApiError(e).error);
    }
  };

  return (
    <Dialog
      open={open}
      title="Certificates"
      width={760}
      onClose={onClose}
      footer={
        <>
          <DialogButton variant="ghost" onClick={() => void load()}>Refresh</DialogButton>
          <DialogButton onClick={onClose}>Close</DialogButton>
        </>
      }
    >
      {data === null ? (
        <p style={{ fontSize: 'var(--text-sm)', color: 'var(--text-secondary)' }}>Loading…</p>
      ) : !data.enabled ? (
        <NotConfigured />
      ) : (
        <>
          <p style={{ fontSize: 'var(--text-xs)', color: 'var(--text-secondary)', margin: '0 0 8px' }}>
            Storage: <code>{data.storage}</code> · {data.certs.length} managed certificate
            {data.certs.length === 1 ? '' : 's'}
          </p>
          <div style={{ overflowX: 'auto' }}>
            <table style={{ width: '100%', borderCollapse: 'collapse' }}>
              <thead>
                <tr style={{ color: 'var(--text-secondary)', textAlign: 'left' }}>
                  <th style={cell}>Domains</th>
                  <th style={cell}>State</th>
                  <th style={cell}>Expires</th>
                  <th style={cell}>Issuer</th>
                  <th style={cell}>Next renewal</th>
                  <th style={cell}></th>
                </tr>
              </thead>
              <tbody>
                {data.certs.map((c) => {
                  const tone = expiryTone(c.not_after, now);
                  const isPending = pending?.id === c.id;
                  return (
                    <tr key={c.id} data-testid="acme-cert-row" data-cert-id={c.id} data-state={c.state}>
                      <td style={{ ...cell, fontFamily: 'var(--font-mono)' }}>{c.domains.join(', ')}</td>
                      <td style={cell}>
                        <span
                          data-testid="acme-cert-state"
                          style={{
                            padding: '1px 6px',
                            borderRadius: 'var(--radius-sm)',
                            border: `1px solid ${badgeColor[c.state]}`,
                            color: badgeColor[c.state],
                            textTransform: 'capitalize',
                          }}
                        >
                          {c.state}
                        </span>
                        {c.last_error && (
                          <button
                            onClick={() => setExpanded(expanded === c.id ? null : c.id)}
                            style={{ marginLeft: 6, fontSize: 'var(--text-xs)', color: 'var(--error)', background: 'none', border: 0, cursor: 'pointer' }}
                          >
                            {expanded === c.id ? 'hide error' : 'last error'}
                          </button>
                        )}
                        {expanded === c.id && c.last_error && (
                          <pre
                            data-testid="acme-cert-error"
                            style={{ margin: '6px 0 0', whiteSpace: 'pre-wrap', fontSize: 'var(--text-xs)', color: 'var(--text-secondary)' }}
                          >
                            {c.last_error}
                          </pre>
                        )}
                      </td>
                      <td style={{ ...cell, color: toneColor[tone] }} title={c.not_after ? formatUnixTime(c.not_after) : ''}>
                        {formatExpiresIn(c.not_after, now)}
                      </td>
                      <td style={cell}>{c.issuer || '—'}</td>
                      <td style={cell}>{c.next_renewal_at ? formatUnixTime(c.next_renewal_at) : '—'}</td>
                      <td style={{ ...cell, whiteSpace: 'nowrap' }}>
                        {!isPending && (
                          <DialogButton variant="ghost" onClick={() => setPending({ id: c.id, stage: 'arm' })}>
                            Renew now
                          </DialogButton>
                        )}
                        {isPending && pending.stage === 'arm' && (
                          <>
                            <DialogButton variant="primary" onClick={() => void renew(c.id, false)}>Confirm</DialogButton>
                            <DialogButton variant="ghost" onClick={() => setPending(null)}>Cancel</DialogButton>
                          </>
                        )}
                        {isPending && pending.stage === 'force' && (
                          <>
                            <span style={{ fontSize: 'var(--text-xs)', color: 'var(--text-secondary)', marginRight: 6 }}>
                              Not due yet.
                            </span>
                            <DialogButton variant="danger" onClick={() => void renew(c.id, true)}>Force renew</DialogButton>
                            <DialogButton variant="ghost" onClick={() => setPending(null)}>Cancel</DialogButton>
                          </>
                        )}
                      </td>
                    </tr>
                  );
                })}
              </tbody>
            </table>
          </div>
        </>
      )}
    </Dialog>
  );
}
