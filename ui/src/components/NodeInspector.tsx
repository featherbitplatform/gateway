/**
 * Right-hand inspector panel for the node selected on the policy canvas.
 * For regular plugin nodes, offers a shared-config picker (`configRef`) that
 * shows the inherited config read-only above the editor, then edits the
 * node's local `config` override either schema-driven (SchemaForm, when the
 * plugin type declares a config schema) or as raw JSON (JsonConfigEditor
 * fallback), and offers node deletion for non-fixed nodes.
 *
 * @module components/NodeInspector
 */
import { useState } from 'react';
import { X } from 'lucide-react';
import type { Node } from '@xyflow/react';
import type { PluginNodeData } from './PluginNode';
import type { PluginConfigDef } from '../types';
import { getPluginMeta } from '../pluginMeta';
import { getPluginConfigSchema } from '../pluginConfig';
import { SchemaForm } from './SchemaForm';

/** Props for {@link NodeInspector}. */
interface NodeInspectorProps {
  /** Selected ReactFlow node (data is {@link PluginNodeData}); `null` renders nothing. */
  node: Node | null;
  /** Named shared plugin configs available for the picker (from GET /api/plugin-configs). */
  pluginConfigs: PluginConfigDef[];
  /** Fires with the node id and full replacement config on every config change/apply. */
  onUpdateConfig: (nodeId: string, config: Record<string, unknown>) => void;
  /** Fires with the node id and the selected shared config name (or `undefined` to clear it). */
  onUpdateConfigRef: (nodeId: string, ref: string | undefined) => void;
  /** Fires with the node id when the Delete Node button is clicked. */
  onDeleteNode: (nodeId: string) => void;
  /** Fires when the close (X) button is clicked. */
  onClose: () => void;
}

const labelStyle: React.CSSProperties = {
  display: 'block',
  fontSize: 'var(--text-xs)',
  fontWeight: 500,
  color: 'var(--text-secondary)',
  marginBottom: 4,
};

/**
 * Raw JSON fallback editor for plugin types without a declared config schema.
 *
 * Holds the JSON text locally and only propagates on Apply Config: valid
 * JSON is parsed and passed to `onApply`, invalid JSON shows an inline
 * "Invalid JSON" error and leaves the node config untouched. The initial
 * text is seeded from `config` once; NodeInspector remounts it per node
 * (keyed by node id) so switching nodes resets the buffer.
 *
 * @param config - Current node config used to seed the textarea.
 * @param onApply - Receives the parsed config object on successful apply.
 */
function JsonConfigEditor({
  config,
  onApply,
}: {
  config: Record<string, unknown>;
  onApply: (config: Record<string, unknown>) => void;
}) {
  const [configJson, setConfigJson] = useState(() => JSON.stringify(config, null, 2));
  const [error, setError] = useState('');

  const handleApply = () => {
    try {
      onApply(JSON.parse(configJson));
      setError('');
    } catch {
      setError('Invalid JSON');
    }
  };

  return (
    <div>
      <label style={labelStyle}>Configuration (JSON)</label>
      <textarea
        value={configJson}
        onChange={(e) => setConfigJson(e.target.value)}
        rows={12}
        className="w-full resize-y"
        style={{
          padding: '8px 10px',
          borderRadius: 'var(--radius-sm)',
          fontFamily: 'var(--font-mono)',
          fontSize: 'var(--text-xs)',
          background: 'var(--surface-sunken)',
          color: 'var(--text-primary)',
          border: `1px solid ${error ? 'var(--error)' : 'var(--border)'}`,
        }}
      />
      {error && (
        <p
          style={{
            fontFamily: 'var(--font-mono)',
            fontSize: 'var(--text-xs)',
            color: 'var(--error)',
            marginTop: 4,
          }}
        >
          {error}
        </p>
      )}
      <button
        onClick={handleApply}
        className="mt-2 w-full transition-colors"
        style={{
          padding: '7px 0',
          borderRadius: 'var(--radius-sm)',
          fontSize: 'var(--text-sm)',
          fontWeight: 500,
          background: 'var(--accent)',
          color: 'var(--text-on-accent)',
        }}
      >
        Apply Config
      </button>
    </div>
  );
}

/**
 * Inspector panel for the selected policy node.
 *
 * Shows the plugin type and read-only node id. For regular (non-fixed,
 * non-supernode) nodes, first renders a "Shared config" picker sourced from
 * `pluginConfigs` (filtered to the node's plugin type) that calls
 * `onUpdateConfigRef`; when a config is selected, its full definition is
 * shown read-only beneath the picker as the inherited base. Then picks the
 * config editor: `listener` and `client` are fixed pipeline endpoints — no
 * configuration and no Delete Node button; types with a schema from
 * getPluginConfigSchema get a {@link SchemaForm} that calls `onUpdateConfig`
 * on every field change; all other types fall back to `JsonConfigEditor`,
 * which updates only on explicit apply. Updates replace the node's entire
 * local `config` object (the overrides layered on top of the inherited
 * config, if any), which is what gets serialized into the policy YAML on
 * save.
 *
 * @remarks
 * The edited config is the same `config` block the Rust plugins deserialize
 * when instantiated via create_plugin in src/plugins/mod.rs. A local key
 * (including an explicit `null`) always wins over the same key inherited
 * from `config_ref` — merging happens at compile time on the gateway side.
 */
export function NodeInspector({
  node,
  pluginConfigs,
  onUpdateConfig,
  onUpdateConfigRef,
  onDeleteNode,
  onClose,
}: NodeInspectorProps) {
  if (!node) return null;

  const data = node.data as unknown as PluginNodeData;
  const meta = getPluginMeta(data.pluginType);
  const schema = getPluginConfigSchema(data.pluginType);
  const isFixed = ['listener', 'client', 'input', 'output', 'error'].includes(data.pluginType);
  const isSupernode = data.pluginType === 'supernode';

  return (
    <div
      className="absolute right-0 top-0 h-full z-40 flex flex-col"
      style={{
        width: 'var(--rail-inspector)',
        background: 'var(--surface)',
        borderLeft: '1px solid var(--border)',
        boxShadow: 'var(--shadow-panel)',
      }}
    >
      <div
        className="flex items-center justify-between"
        style={{
          padding: '14px 16px',
          borderBottom: '1px solid var(--border)',
          borderTop: `2px solid ${meta.color}`,
        }}
      >
        <div>
          <span
            style={{
              fontFamily: 'var(--font-mono)',
              fontSize: 'var(--text-base)',
              fontWeight: 600,
              color: 'var(--text-primary)',
            }}
          >
            {data.pluginType}
          </span>
          <p
            style={{
              fontFamily: 'var(--font-mono)',
              fontSize: 'var(--text-xs)',
              color: 'var(--text-muted)',
              margin: 0,
            }}
          >
            {node.id}
          </p>
        </div>
        <button
          onClick={onClose}
          className="flex items-center justify-center rounded transition-colors"
          style={{ width: 26, height: 26, color: 'var(--text-secondary)' }}
          onMouseEnter={(e) => (e.currentTarget.style.background = 'var(--surface-hover)')}
          onMouseLeave={(e) => (e.currentTarget.style.background = 'transparent')}
          aria-label="Close"
        >
          <X size={15} />
        </button>
      </div>

      <div className="flex-1 overflow-y-auto p-4 space-y-4">
        {/* Node ID */}
        <div>
          <label style={labelStyle}>Node ID</label>
          <input
            type="text"
            value={node.id}
            readOnly
            className="w-full"
            style={{
              padding: '6px 10px',
              borderRadius: 'var(--radius-sm)',
              fontFamily: 'var(--font-mono)',
              fontSize: 'var(--text-sm)',
              background: 'var(--surface-input)',
              color: 'var(--text-primary)',
              border: '1px solid var(--border)',
            }}
          />
        </div>

        {/* Shared config picker */}
        {!isFixed && !isSupernode && (
          <div>
            <label style={labelStyle}>Shared config</label>
            <select
              value={data.configRef ?? ''}
              onChange={(e) => onUpdateConfigRef(node.id, e.target.value || undefined)}
              className="w-full"
              style={{
                padding: '6px 10px',
                borderRadius: 'var(--radius-sm)',
                fontFamily: 'var(--font-mono)',
                fontSize: 'var(--text-sm)',
                background: 'var(--surface-input)',
                color: 'var(--text-primary)',
                border: '1px solid var(--border)',
              }}
            >
              <option value="">None</option>
              {pluginConfigs
                .filter((p) => p.type === data.pluginType)
                .map((p) => (
                  <option key={p.name} value={p.name}>
                    {p.name}
                  </option>
                ))}
            </select>
            {data.configRef && (
              <div style={{ marginTop: 8 }}>
                <label style={labelStyle}>
                  Inherited from {data.configRef} (local keys below override)
                </label>
                <div
                  style={{
                    padding: '8px 10px',
                    borderRadius: 'var(--radius-sm)',
                    fontFamily: 'var(--font-mono)',
                    fontSize: 'var(--text-xs)',
                    background: 'var(--surface-sunken)',
                    color: 'var(--text-muted)',
                    border: '1px solid var(--border)',
                    maxHeight: 160,
                    overflowY: 'auto',
                    whiteSpace: 'pre',
                  }}
                >
                  {JSON.stringify(
                    pluginConfigs.find((p) => p.name === data.configRef)?.config ?? {},
                    null,
                    2
                  )}
                </div>
              </div>
            )}
          </div>
        )}

        {/* Configuration */}
        {isFixed ? (
          <p style={{ fontSize: 'var(--text-xs)', color: 'var(--text-muted)', margin: 0 }}>
            This node takes no configuration.
          </p>
        ) : isSupernode ? (
          <div>
            <label style={labelStyle}>Supernode</label>
            <div
              style={{
                padding: '6px 10px',
                borderRadius: 'var(--radius-sm)',
                fontFamily: 'var(--font-mono)',
                fontSize: 'var(--text-sm)',
                background: 'var(--surface-input)',
                color: 'var(--text-primary)',
                border: '1px solid var(--border)',
              }}
            >
              {String(data.config?.name ?? '')}
            </div>
            <p style={{ fontSize: 'var(--text-xs)', color: 'var(--text-muted)', marginTop: 6 }}>
              Reusable subgraph — edit its definition from the Supernodes section in the sidebar.
            </p>
          </div>
        ) : schema.length > 0 ? (
          <SchemaForm
            schema={schema}
            value={data.config || {}}
            onChange={(config) => onUpdateConfig(node.id, config)}
          />
        ) : (
          <JsonConfigEditor
            key={node.id}
            config={data.config || {}}
            onApply={(config) => onUpdateConfig(node.id, config)}
          />
        )}
      </div>

      {/* Delete */}
      {!isFixed && (
        <div style={{ padding: 16, borderTop: '1px solid var(--border)' }}>
          <button
            onClick={() => onDeleteNode(node.id)}
            className="w-full transition-colors"
            style={{
              padding: '7px 0',
              borderRadius: 'var(--radius-sm)',
              fontSize: 'var(--text-sm)',
              fontWeight: 500,
              background: 'var(--error)',
              color: '#fff',
            }}
          >
            Delete Node
          </button>
        </div>
      )}
    </div>
  );
}
