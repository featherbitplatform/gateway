/**
 * Form editor for one shared plugin config: description plus the config
 * body, schema-driven when the profile's plugin type declares a form schema
 * (SchemaForm), raw JSON otherwise (JsonConfigEditor). Saving PUTs the whole
 * definition; the gateway re-resolves and recompiles every referencing
 * policy atomically (400 surfaces in the toast on breaking edits).
 *
 * @module components/PluginConfigPanel
 */
import { useState } from 'react';
import type { PluginConfigDef } from '../types';
import { getPluginMeta } from '../pluginMeta';
import { getPluginConfigSchema } from '../pluginConfig';
import { SchemaForm } from './SchemaForm';
import { JsonConfigEditor } from './NodeInspector';

interface PluginConfigPanelProps {
  /** The shared config being edited (a copy; edits are local until Save). */
  def: PluginConfigDef;
  /** Fires with the edited definition when Save is clicked. */
  onSave: (def: PluginConfigDef) => void;
}

export function PluginConfigPanel({ def, onSave }: PluginConfigPanelProps) {
  const [description, setDescription] = useState(def.description ?? '');
  const [config, setConfig] = useState<Record<string, unknown>>(def.config ?? {});
  const meta = getPluginMeta(def.type);
  const Icon = meta.icon;
  const schema = getPluginConfigSchema(def.type);

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
          <JsonConfigEditor key={def.name} config={config} onApply={setConfig} />
        )}

        <button
          onClick={() =>
            onSave({ ...def, description: description || undefined, config })
          }
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
