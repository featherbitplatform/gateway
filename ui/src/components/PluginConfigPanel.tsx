/**
 * Form editor for one shared plugin config: description plus the config
 * body, schema-driven when the profile's plugin type declares a form schema
 * (SchemaForm), raw JSON otherwise (a plain textarea owned by this panel).
 * Saving PUTs the whole definition; the gateway re-resolves and recompiles
 * every referencing policy atomically (400 surfaces in the toast on
 * breaking edits).
 *
 * @remarks
 * The JSON-fallback textarea's buffer is owned here (not delegated to a
 * separate "Apply" step like NodeInspector's JsonConfigEditor) so that Save
 * always parses the exact text on screen: invalid JSON blocks the save with
 * an inline error instead of silently persisting a stale config.
 *
 * @module components/PluginConfigPanel
 */
import { useState } from 'react';
import type { PluginConfigDef } from '../types';
import { getPluginMeta } from '../pluginMeta';
import { getPluginConfigSchema } from '../pluginConfig';
import { SchemaForm } from './SchemaForm';

interface PluginConfigPanelProps {
  /** The shared config being edited (a copy; edits are local until Save). */
  def: PluginConfigDef;
  /** Fires with the edited definition when Save is clicked. */
  onSave: (def: PluginConfigDef) => void;
}

export function PluginConfigPanel({ def, onSave }: PluginConfigPanelProps) {
  const [description, setDescription] = useState(def.description ?? '');
  const [config, setConfig] = useState<Record<string, unknown>>(def.config ?? {});
  // JSON-fallback buffer (schema-less types only). Seeded once from
  // `def.config`; kept in sync with the textarea on every keystroke so Save
  // reads exactly what is on screen.
  const [configJson, setConfigJson] = useState(() => JSON.stringify(def.config ?? {}, null, 2));
  const [jsonError, setJsonError] = useState('');
  const meta = getPluginMeta(def.type);
  const Icon = meta.icon;
  const schema = getPluginConfigSchema(def.type);

  const handleSave = () => {
    if (schema.length > 0) {
      onSave({ ...def, description: description || undefined, config });
      return;
    }
    // Raw-JSON path: parse the live buffer rather than trusting `config`,
    // which nothing keeps up to date without a separate "Apply" click.
    try {
      const parsed = JSON.parse(configJson) as Record<string, unknown>;
      setJsonError('');
      onSave({ ...def, description: description || undefined, config: parsed });
    } catch {
      setJsonError('Invalid JSON');
    }
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
              background: meta.color,
              color: '#fff',
            }}
          >
            <Icon size={15} />
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
              shared config for <code>{def.type}</code>
            </p>
          </div>
        </div>
        <p style={{ fontSize: 'var(--text-xs)', color: 'var(--text-muted)', margin: '8px 0 16px' }}>
          Nodes referencing this config inherit every key here; their local keys override.
          Saving applies to all of them atomically.
        </p>

        <label
          style={{
            display: 'block',
            fontSize: 'var(--text-xs)',
            fontWeight: 500,
            color: 'var(--text-secondary)',
            marginBottom: 4,
          }}
        >
          Description
        </label>
        <input
          type="text"
          value={description}
          onChange={(e) => setDescription(e.target.value)}
          className="w-full"
          style={{
            padding: '6px 10px',
            marginBottom: 16,
            borderRadius: 'var(--radius-sm)',
            fontSize: 'var(--text-sm)',
            background: 'var(--surface-input)',
            color: 'var(--text-primary)',
            border: '1px solid var(--border)',
          }}
        />

        {schema.length > 0 ? (
          <SchemaForm schema={schema} value={config} onChange={setConfig} />
        ) : (
          <div>
            <label
              style={{
                display: 'block',
                fontSize: 'var(--text-xs)',
                fontWeight: 500,
                color: 'var(--text-secondary)',
                marginBottom: 4,
              }}
            >
              Configuration (JSON)
            </label>
            <textarea
              value={configJson}
              onChange={(e) => {
                setConfigJson(e.target.value);
                if (jsonError) setJsonError('');
              }}
              rows={12}
              className="w-full resize-y"
              style={{
                padding: '8px 10px',
                borderRadius: 'var(--radius-sm)',
                fontFamily: 'var(--font-mono)',
                fontSize: 'var(--text-xs)',
                background: 'var(--surface-sunken)',
                color: 'var(--text-primary)',
                border: `1px solid ${jsonError ? 'var(--error)' : 'var(--border)'}`,
              }}
            />
            {jsonError && (
              <p
                style={{
                  fontFamily: 'var(--font-mono)',
                  fontSize: 'var(--text-xs)',
                  color: 'var(--error)',
                  marginTop: 4,
                }}
              >
                {jsonError}
              </p>
            )}
          </div>
        )}

        <button
          onClick={handleSave}
          className="w-full transition-colors"
          style={{
            marginTop: 16,
            padding: '8px 0',
            borderRadius: 'var(--radius-sm)',
            fontSize: 'var(--text-sm)',
            fontWeight: 500,
            background: 'var(--accent)',
            color: 'var(--text-on-accent)',
          }}
        >
          Save Plugin Config
        </button>
      </div>
    </div>
  );
}
