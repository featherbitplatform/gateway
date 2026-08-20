/**
 * Right-hand inspector panel for the node selected on the policy canvas.
 * For regular plugin nodes, offers a shared-config picker (`configRef`), a
 * "Save as shared config" extraction flow, and edits the node's local
 * `config` override either schema-driven (SchemaForm, showing inherited
 * values inline with override/added flags when a shared config is selected)
 * or as raw JSON (JsonConfigEditor fallback, with the inherited config shown
 * read-only above it), and offers node deletion for non-fixed nodes.
 *
 * @module components/NodeInspector
 */
import { useState } from 'react';
import { Braces, X } from 'lucide-react';
import type { Node } from '@xyflow/react';
import type { PluginNodeData } from './PluginNode';
import type { DebugConfig, PluginConfigDef } from '../types';
import { getPluginMeta } from '../pluginMeta';
import { getPluginConfigSchema } from '../pluginConfig';
import { mergeEffective } from '../configInheritance';
import { Dialog, DialogButton, DialogField } from './Dialog';
import { SchemaForm } from './SchemaForm';
import { VarLegend } from './VarLegend';
import { useContextSuggestions } from '../varSuggestions';

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
  /**
   * Persists a new shared plugin config extracted from the selected node
   * ("Save as shared config"); resolves `true` on success (the inspector then
   * re-links the node to it via `onUpdateConfigRef`/`onUpdateConfig`).
   */
  onExtractPluginConfig: (def: PluginConfigDef) => Promise<boolean>;
  /** Fires with the node id when the Delete Node button is clicked. */
  onDeleteNode: (nodeId: string) => void;
  /** Fires when the close (X) button is clicked. */
  onClose: () => void;
  /** Name of the policy being edited (null in supernode-definition mode); scopes the trace lookup. */
  policyName: string | null;
  /**
   * Node id feeding the selected node's `in` handle, for the var-suggestion
   * hook: `undefined` = no incoming edge, `null` = the incoming edge comes
   * from the pipeline entry (listener/input) so preview from the trace's
   * initial snapshot, a string = a real predecessor node id. See
   * varSuggestions.ts's module doc comment for the full encoding.
   */
  predecessorId: string | null | undefined;
  /** Debug settings; drives whether live previews are attempted at all. */
  debugConfig: DebugConfig | null;
  /** Whether the canvas is editing a policy or a supernode definition. */
  kind: 'policy' | 'supernode';
  /**
   * Opens GraphCanvas's rename dialog for this node id. Only supplied in
   * supernode-definition mode; the inspector merely opens the dialog —
   * validation lives in GraphCanvas's `submitPortDialog`, the one path
   * shared with adding a new output-port boundary.
   */
  onRenameNode?: (nodeId: string) => void;
}

/** Plugin types with no configuration of their own — fixed pipeline endpoints and supernode boundary pseudo-nodes. */
const FIXED_TYPES = ['listener', 'client', 'input', 'output', 'error'];

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
 * `onUpdateConfigRef`, plus a "Save as shared config" button (shown whenever
 * the node's effective config is non-empty) that extracts the effective
 * config into a new shared config via `onExtractPluginConfig` and re-links
 * the node to it. Then picks the config editor: `listener` and `client` are
 * fixed pipeline endpoints — no configuration and no Delete Node button;
 * types with a schema from getPluginConfigSchema get a {@link SchemaForm}
 * that calls `onUpdateConfig` on every field change and, when a shared
 * config is selected, shows its values inline as the inherited layer with
 * override/added flags (see SchemaForm's `inherited` prop); all other types
 * fall back to `JsonConfigEditor`, which updates only on explicit apply and
 * keeps the read-only inherited-config blob above it. Updates replace the
 * node's entire local `config` object (the overrides layered on top of the
 * inherited config, if any), which is what gets serialized into the policy
 * YAML on save.
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
  onExtractPluginConfig,
  onDeleteNode,
  onClose,
  policyName,
  predecessorId,
  debugConfig,
  kind,
  onRenameNode,
}: NodeInspectorProps) {
  // Computed ahead of the `!node` early return below so the hooks that
  // follow (useState, useContextSuggestions) run unconditionally on every
  // render — conditioning them on `node` would violate the rules of hooks
  // whenever the selection changes to/from null.
  const pendingData = node?.data as unknown as PluginNodeData | undefined;
  const isFixedNode = pendingData ? FIXED_TYPES.includes(pendingData.pluginType) : false;
  const isSupernodeNode = pendingData?.pluginType === 'supernode';
  // The hook must not run network calls for fixed/supernode/boundary nodes
  // (nor when nothing is selected): pass nodeId: null to skip fetching.
  const skipFetch = !node || isFixedNode || isSupernodeNode;

  const [legendOpen, setLegendOpen] = useState(false);
  // "Save as shared config" dialog state.
  const [extractOpen, setExtractOpen] = useState(false);
  const [extractName, setExtractName] = useState('');
  const [extractDesc, setExtractDesc] = useState('');
  const [extractError, setExtractError] = useState('');
  const { suggestions, availability, catalog } = useContextSuggestions({
    policyName,
    nodeId: !skipFetch && node ? node.id : null,
    predecessorId,
    kind,
    debugEnabled: debugConfig?.enabled ?? false,
    captureBodies: debugConfig?.capture_bodies ?? false,
  });

  if (!node) return null;

  const data = node.data as unknown as PluginNodeData;
  const meta = getPluginMeta(data.pluginType);
  const schema = getPluginConfigSchema(data.pluginType);
  const isFixed = isFixedNode;
  const isSupernode = isSupernodeNode;

  // Config inherited from the selected shared config (undefined without a
  // ref), and the node's effective config — what "Save as shared config"
  // captures, matching the gateway's shallow compile-time merge.
  const inheritedConfig = data.configRef
    ? (pluginConfigs.find((p) => p.name === data.configRef)?.config ?? {})
    : undefined;
  const effectiveConfig = mergeEffective(inheritedConfig ?? {}, data.config ?? {});

  const openExtract = () => {
    setExtractName('');
    setExtractDesc('');
    setExtractError('');
    setExtractOpen(true);
  };

  const submitExtract = async () => {
    const name = extractName.trim();
    if (!name) return;
    if (pluginConfigs.some((p) => p.name === name)) {
      setExtractError(`A shared config named "${name}" already exists`);
      return;
    }
    const saved = await onExtractPluginConfig({
      name,
      type: data.pluginType,
      description: extractDesc.trim() || undefined,
      config: effectiveConfig,
    });
    if (saved) {
      // Re-link the node: same effective config, now inherited from the ref.
      onUpdateConfigRef(node.id, name);
      onUpdateConfig(node.id, {});
      setExtractOpen(false);
    }
  };

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
        <div className="flex items-center" style={{ gap: 4 }}>
          <button
            onClick={() => setLegendOpen(true)}
            className="flex items-center justify-center rounded transition-colors"
            style={{ width: 26, height: 26, color: 'var(--text-secondary)' }}
            onMouseEnter={(e) => (e.currentTarget.style.background = 'var(--surface-hover)')}
            onMouseLeave={(e) => (e.currentTarget.style.background = 'transparent')}
            aria-label="Context vars reference"
            title="Context vars reference"
          >
            <Braces size={15} />
          </button>
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
      </div>

      <div className="flex-1 overflow-y-auto p-4 space-y-4">
        {/* Node ID */}
        <div>
          <label style={labelStyle}>Node ID</label>
          <div className="flex items-center" style={{ gap: 6 }}>
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
            {kind === 'supernode' && data.pluginType === 'output' && onRenameNode && (
              <button
                onClick={() => onRenameNode(node.id)}
                className="shrink-0 transition-colors"
                style={{
                  padding: '6px 10px',
                  borderRadius: 'var(--radius-sm)',
                  fontSize: 'var(--text-xs)',
                  fontWeight: 500,
                  background: 'var(--surface-raised)',
                  color: 'var(--text-primary)',
                  border: '1px solid var(--border)',
                }}
              >
                Rename
              </button>
            )}
          </div>
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
            {/* Schema-driven forms show inherited values inline (with override
                flags), so the raw blob is only needed for the JSON fallback. */}
            {data.configRef && schema.length === 0 && (
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
                  {JSON.stringify(inheritedConfig ?? {}, null, 2)}
                </div>
              </div>
            )}
            {Object.keys(effectiveConfig).length > 0 && (
              <button
                onClick={openExtract}
                className="w-full transition-colors"
                style={{
                  marginTop: 8,
                  padding: '6px 0',
                  borderRadius: 'var(--radius-sm)',
                  fontSize: 'var(--text-xs)',
                  fontWeight: 500,
                  background: 'transparent',
                  color: 'var(--accent-hover)',
                  border: '1px dashed var(--border-strong)',
                }}
                onMouseEnter={(e) => {
                  e.currentTarget.style.borderColor = 'var(--accent-ring)';
                  e.currentTarget.style.background = 'var(--accent-soft)';
                }}
                onMouseLeave={(e) => {
                  e.currentTarget.style.borderColor = 'var(--border-strong)';
                  e.currentTarget.style.background = 'transparent';
                }}
              >
                Save as shared config
              </button>
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
            varContext={{ suggestions, availability, onOpenLegend: () => setLegendOpen(true) }}
            inherited={inheritedConfig}
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

      <VarLegend
        open={legendOpen}
        onClose={() => setLegendOpen(false)}
        catalog={catalog}
        suggestions={suggestions}
        availability={availability}
      />

      <Dialog
        open={extractOpen}
        title="Save as shared config"
        onClose={() => setExtractOpen(false)}
        footer={
          <>
            <DialogButton variant="ghost" onClick={() => setExtractOpen(false)}>
              Cancel
            </DialogButton>
            <DialogButton onClick={submitExtract} disabled={!extractName.trim()}>
              Save shared config
            </DialogButton>
          </>
        }
      >
        <p style={{ fontSize: 'var(--text-xs)', color: 'var(--text-muted)', margin: '0 0 12px' }}>
          Saves this node&apos;s current configuration as a shared <code>{data.pluginType}</code>{' '}
          config and links the node to it. Other nodes can then reference it too.
        </p>
        <DialogField
          label="Config name"
          value={extractName}
          onChange={(v) => {
            setExtractName(v);
            if (extractError) setExtractError('');
          }}
          placeholder="my-shared-config"
          mono
          autoFocus
        />
        <DialogField
          label="Description (optional)"
          value={extractDesc}
          onChange={setExtractDesc}
          placeholder="What this profile is for"
        />
        {extractError && (
          <p style={{ fontSize: 'var(--text-xs)', color: 'var(--error)', margin: 0 }}>
            {extractError}
          </p>
        )}
      </Dialog>
    </div>
  );
}
