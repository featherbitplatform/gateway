/**
 * Form editor for one named store: connection fields plus a live ping check
 * against the store's last-saved configuration. Saving PUTs the whole
 * definition; `topology`/`urls`/`tls` are not editable in v1 (reserved for
 * HA/TLS topologies not yet exposed here) but are preserved via spread so a
 * hand-authored YAML's settings for them survive a UI save.
 *
 * @remarks
 * Dumb panel: all local state is seeded from `def` once and the panel is
 * remounted (via `key={def.name}`) whenever the selected store changes — no
 * prop-sync effects, mirrors `PluginConfigPanel`.
 *
 * @module components/StoresPanel
 */
import { useState } from 'react';
import type { CSSProperties } from 'react';
import { Database } from 'lucide-react';
import type { StoreConfig, StorePing } from '../types';
import { api } from '../api/client';
import { parseApiError } from '../apiError';

interface StoresPanelProps {
  /** The store being edited (a copy; edits are local until Save). */
  def: StoreConfig;
  /** Fires with the edited definition when "Save Store" is clicked. */
  onSave: (store: StoreConfig) => Promise<void>;
  /** Fires with a title/message for errors this panel can't show inline (e.g. invalid input). */
  onError: (title: string, message: string) => void;
}

const labelStyle: CSSProperties = {
  display: 'block',
  fontSize: 'var(--text-xs)',
  fontWeight: 500,
  color: 'var(--text-secondary)',
  marginBottom: 4,
};

const inputStyle = (mono = false): CSSProperties => ({
  padding: '7px 10px',
  borderRadius: 'var(--radius-sm)',
  fontFamily: mono ? 'var(--font-mono)' : 'var(--font-sans)',
  fontSize: 'var(--text-sm)',
  background: 'var(--surface-input)',
  color: 'var(--text-primary)',
  border: '1px solid var(--border)',
});

const hintStyle: CSSProperties = {
  fontSize: 'var(--text-2xs)',
  color: 'var(--text-muted)',
  margin: '4px 0 16px',
};

type PingState =
  | { state: 'idle' }
  | { state: 'busy' }
  | { state: 'ok'; result: StorePing }
  | { state: 'fail'; message: string };

export function StoresPanel({ def, onSave, onError }: StoresPanelProps) {
  const [type, setType] = useState(def.type);
  const [description, setDescription] = useState(def.description ?? '');
  const [url, setUrl] = useState(def.url);
  const [password, setPassword] = useState(def.password ?? '');
  const [keyPrefix, setKeyPrefix] = useState(def.key_prefix);
  const [connectTimeoutMs, setConnectTimeoutMs] = useState(def.connect_timeout_ms);
  const [ping, setPing] = useState<PingState>({ state: 'idle' });

  const handlePing = async () => {
    setPing({ state: 'busy' });
    try {
      setPing({ state: 'ok', result: await api.pingStore(def.name) });
    } catch (e) {
      const parsed = parseApiError(e);
      setPing({
        state: 'fail',
        message:
          parsed.status === 501
            ? 'This gateway build has no redis-store support.'
            : parsed.status === 504
              ? `Timed out: ${parsed.error}`
              : parsed.error,
      });
    }
  };

  const handleSave = async () => {
    if (!Number.isFinite(connectTimeoutMs) || connectTimeoutMs <= 0) {
      onError('Invalid connect timeout', 'Connect timeout must be a positive number of milliseconds.');
      return;
    }
    await onSave({
      ...def,
      type,
      description: description || undefined,
      url,
      password: password || undefined,
      key_prefix: keyPrefix,
      connect_timeout_ms: connectTimeoutMs,
    });
  };

  return (
    <div className="flex-1 overflow-y-auto" style={{ background: 'var(--bg-canvas)' }}>
      <div style={{ maxWidth: 560, margin: '32px auto', padding: '0 16px' }}>
        <div className="flex items-center" style={{ gap: 10, marginBottom: 4 }}>
          <span
            className="flex items-center justify-center"
            style={{
              width: 30,
              height: 30,
              borderRadius: 'var(--radius-sm)',
              background: '#8b5cf6',
              color: '#fff',
            }}
          >
            <Database size={15} />
          </span>
          <div>
            <h2
              style={{
                fontFamily: 'var(--font-mono)',
                fontSize: 'var(--text-md)',
                fontWeight: 600,
                color: 'var(--text-primary)',
                margin: 0,
              }}
            >
              {def.name}
            </h2>
            <p style={{ fontSize: 'var(--text-xs)', color: 'var(--text-muted)', margin: 0 }}>
              named store &middot; <code>{type}</code>
            </p>
          </div>
        </div>
        <p style={{ fontSize: 'var(--text-xs)', color: 'var(--text-muted)', margin: '8px 0 16px' }}>
          Plugin config references this store by name (<code>store</code> / <code>session_store</code>).
          Saving applies immediately; clients rebuild at the next config apply.
        </p>

        <label style={labelStyle}>Type</label>
        <select
          value={type}
          onChange={(e) => setType(e.target.value)}
          className="w-full"
          style={{ ...inputStyle(), marginBottom: 16 }}
        >
          <option value="redis">redis</option>
          <option value="valkey">valkey</option>
        </select>

        <label style={labelStyle}>Description</label>
        <input
          type="text"
          value={description}
          onChange={(e) => setDescription(e.target.value)}
          className="w-full"
          style={{ ...inputStyle(), marginBottom: 16 }}
        />

        <label style={labelStyle}>URL</label>
        <input
          type="text"
          value={url}
          onChange={(e) => setUrl(e.target.value)}
          placeholder="redis://127.0.0.1:6379"
          className="w-full"
          style={inputStyle(true)}
        />
        <p style={hintStyle}>${'{ENV}'} placeholders stay raw &mdash; resolved only when the client is built</p>

        <label style={labelStyle}>Password</label>
        <input
          type="password"
          value={password}
          onChange={(e) => setPassword(e.target.value)}
          className="w-full"
          style={inputStyle(true)}
        />
        <p style={hintStyle}>optional; ${'{ENV}'} recommended</p>

        <label style={labelStyle}>Key prefix</label>
        <input
          type="text"
          value={keyPrefix}
          onChange={(e) => setKeyPrefix(e.target.value)}
          className="w-full"
          style={{ ...inputStyle(), marginBottom: 16 }}
        />

        <label style={labelStyle}>Connect timeout (ms)</label>
        <input
          type="number"
          value={connectTimeoutMs}
          onChange={(e) => setConnectTimeoutMs(Number(e.target.value))}
          className="w-full"
          style={{ ...inputStyle(), marginBottom: 16 }}
        />

        <div
          className="flex items-center"
          style={{
            gap: 10,
            padding: '10px 12px',
            marginBottom: 4,
            borderRadius: 'var(--radius-sm)',
            border: '1px solid var(--border)',
            background: 'var(--surface-sunken)',
          }}
        >
          <button
            onClick={handlePing}
            disabled={ping.state === 'busy'}
            aria-label="Ping store"
            style={{
              padding: '6px 14px',
              borderRadius: 'var(--radius-sm)',
              fontSize: 'var(--text-sm)',
              fontWeight: 500,
              background: 'transparent',
              color: 'var(--text-secondary)',
              border: '1px solid var(--border)',
              opacity: ping.state === 'busy' ? 0.5 : 1,
              cursor: ping.state === 'busy' ? 'not-allowed' : 'pointer',
            }}
          >
            {ping.state === 'busy' ? 'Pinging…' : 'Ping'}
          </button>
          {ping.state === 'ok' && (
            <span
              style={{
                fontFamily: 'var(--font-mono)',
                fontSize: 'var(--text-sm)',
                color: 'var(--success, var(--accent))',
              }}
            >
              ✓ {ping.result.latency_ms} ms &middot; v{ping.result.version}
            </span>
          )}
          {ping.state === 'fail' && (
            <span style={{ fontSize: 'var(--text-sm)', color: 'var(--error)' }}>{ping.message}</span>
          )}
        </div>
        <p style={hintStyle}>pings the last saved configuration</p>

        <button
          onClick={handleSave}
          className="w-full transition-colors"
          style={{
            marginTop: 12,
            padding: '8px 0',
            borderRadius: 'var(--radius-sm)',
            fontSize: 'var(--text-sm)',
            fontWeight: 500,
            background: 'var(--accent)',
            color: 'var(--text-on-accent)',
          }}
        >
          Save Store
        </button>
      </div>
    </div>
  );
}
