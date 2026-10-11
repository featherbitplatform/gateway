/**
 * Right-hand inspector panel for the node selected on the policy canvas.
 * For regular plugin nodes, offers a shared-config picker (`configRef`), a
 * "Save as shared config" extraction flow, and edits the node's local
 * `config` override either schema-driven (SchemaForm, showing inherited
 * values inline with override/added flags when a shared config is selected)
 * or as raw JSON (JsonConfigEditor fallback, with the inherited config shown
 * read-only above it), and offers node deletion for non-fixed nodes (plus,
 * in supernode-definition mode, output/error boundary nodes — guarded
 * against deleting the last of a kind via `boundaryDeleteBlocked`).
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
import type { FieldOption } from '../pluginConfig';
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
   * Opens GraphCanvas's rename dialog for this node id (fired for output
   * and error boundaries alike). Only supplied in supernode-definition
   * mode; the inspector merely opens the dialog — validation lives in
   * GraphCanvas's `submitPortDialog`, the one path shared with adding a
   * new output/error-port boundary.
   */
  onRenameNode?: (nodeId: string) => void;
  /**
   * When set, the Delete Node button for the selected output/error boundary
   * is disabled with this tooltip — GraphCanvas computes it from the count
   * of boundaries sharing the node's kind (a supernode needs at least one of
   * each; the server enforces this too, so a keyboard delete that bypasses
   * this guard fails on save with a clear message). Undefined for non-
   * boundary nodes and whenever deleting is safe.
   */
  boundaryDeleteBlocked?: string;
  /**
   * Declared `stores:` entries as select options (`{value, label}`), forwarded
   * into SchemaForm's `dynamicOptions.stores` for `optionsFrom: 'stores'`
   * fields. Computed once in App from `StoreConfig[]` and threaded through
   * GraphCanvas; shared with PluginConfigPanel.
   */
  storeOptions: FieldOption[];
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
        className="rg-field w-full resize-y"
        style={
          {
            padding: '8px 10px',
            borderRadius: 'var(--radius-sm)',
            fontFamily: 'var(--font-mono)',
            fontSize: 'var(--text-xs)',
            color: 'var(--text-primary)',
            outline: 'none',
            '--rg-bg': 'var(--surface-sunken)',
            ...(error
              ? { '--rg-bd': 'var(--error)', '--rg-hover-bd': 'var(--error)', '--rg-focus-bd': 'var(--error)', '--rg-focus-ring': 'var(--error-soft)' }
              : null),
          } as React.CSSProperties
        }
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
        className="rg-press rg-hover mt-2 w-full"
        style={
          {
            height: 32,
            borderRadius: 'var(--radius-sm)',
            border: '1px solid transparent',
            fontSize: 'var(--text-sm)',
            fontWeight: 500,
            '--rg-bg': 'var(--accent)',
            '--rg-fg': 'var(--text-on-accent)',
            '--rg-hover-bg': 'var(--accent-hover)',
          } as React.CSSProperties
        }
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
  boundaryDeleteBlocked,
  storeOptions,
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
  const isBoundaryPort =
    kind === 'supernode' && (data.pluginType === 'output' || data.pluginType === 'error');

  // Config inherited from the selected shared config (undefined without a
  // ref), and the node's effective config — what "Save as shared config"
  // captures, matching the gateway's shallow compile-time merge.
  const inheritedConfig = data.configRef
    ? (pluginConfigs.find((p) => p.name === data.configRef)?.config ?? {})
    : undefined;
  const effectiveConfig = mergeEffective(inheritedConfig ?? {}, data.config ?? {});

  const deleteDisabled = isBoundaryPort && !!boundaryDeleteBlocked;

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
      className="rg-panel-enter absolute right-0 top-0 h-full z-40 flex flex-col"
      style={{
        width: 'var(--rail-inspector)',
        background: 'var(--surface)',
        borderLeft: '1px solid var(--border-subtle)',
        boxShadow: 'var(--shadow-panel)',
      }}
    >
      <div
        className="flex items-center justify-between"
        style={{
          gap: 12,
          padding: '12px 12px 12px 16px',
          borderBottom: '1px solid var(--border-subtle)',
        }}
      >
        <div className="flex items-center" style={{ gap: 10, minWidth: 0 }}>
          <span
            aria-hidden
            className="flex items-center justify-center"
            style={{
              width: 28,
              height: 28,
              flexShrink: 0,
              borderRadius: 'var(--radius-sm)',
              background: `color-mix(in oklch, ${meta.color} 16%, transparent)`,
              color: meta.color,
            }}
          >
            <meta.icon size={15} strokeWidth={1.75} />
          </span>
          <div style={{ minWidth: 0 }}>
            <span
              style={{
                fontFamily: 'var(--font-sans)',
                fontSize: 'var(--text-base)',
                fontWeight: 600,
                letterSpacing: 'var(--tracking-tight)',
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
                whiteSpace: 'nowrap',
                overflow: 'hidden',
                textOverflow: 'ellipsis',
              }}
            >
              {node.id}
            </p>
          </div>
        </div>
        <div className="flex items-center" style={{ gap: 4 }}>
          <button
            onClick={() => setLegendOpen(true)}
            className="rg-press rg-hover flex items-center justify-center"
            style={
              {
                width: 28,
                height: 28,
                borderRadius: 'var(--radius-sm)',
                border: '1px solid transparent',
                '--rg-fg': 'var(--text-muted)',
                '--rg-hover-fg': 'var(--text-primary)',
              } as React.CSSProperties
            }
            aria-label="Context vars reference"
            title="Context vars reference"
          >
            <Braces size={15} />
          </button>
          <button
            onClick={onClose}
            className="rg-press rg-hover flex items-center justify-center"
            style={
              {
                width: 28,
                height: 28,
                borderRadius: 'var(--radius-sm)',
                border: '1px solid transparent',
                '--rg-fg': 'var(--text-muted)',
                '--rg-hover-fg': 'var(--text-primary)',
              } as React.CSSProperties
            }
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
                height: 32,
                padding: '0 10px',
                borderRadius: 'var(--radius-sm)',
                fontFamily: 'var(--font-mono)',
                fontSize: 'var(--text-sm)',
                background: 'var(--surface-sunken)',
                color: 'var(--text-secondary)',
                border: '1px solid var(--border-subtle)',
                outline: 'none',
              }}
            />
            {kind === 'supernode' &&
              (data.pluginType === 'output' || data.pluginType === 'error') &&
              onRenameNode && (
              <button
                onClick={() => onRenameNode(node.id)}
                className="rg-press rg-hover shrink-0"
                style={
                  {
                    height: 32,
                    padding: '0 10px',
                    borderRadius: 'var(--radius-sm)',
                    borderWidth: 1,
                    borderStyle: 'solid',
                    fontSize: 'var(--text-xs)',
                    fontWeight: 500,
                    '--rg-bg': 'var(--surface-raised)',
                    '--rg-fg': 'var(--text-primary)',
                    '--rg-bd': 'var(--border)',
                    '--rg-hover-bg': 'var(--surface-raised)',
                    '--rg-hover-bd': 'var(--border-strong)',
                  } as React.CSSProperties
                }
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
              className="rg-field w-full"
              style={{
                height: 32,
                padding: '0 10px',
                borderRadius: 'var(--radius-sm)',
                fontFamily: 'var(--font-mono)',
                fontSize: 'var(--text-sm)',
                color: 'var(--text-primary)',
                outline: 'none',
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
                className="rg-press rg-hover w-full"
                style={
                  {
                    marginTop: 8,
                    height: 30,
                    borderRadius: 'var(--radius-sm)',
                    borderWidth: 1,
                    borderStyle: 'dashed',
                    fontSize: 'var(--text-xs)',
                    fontWeight: 500,
                    '--rg-fg': 'var(--accent-fg)',
                    '--rg-bd': 'var(--border-strong)',
                    '--rg-hover-bg': 'var(--accent-soft)',
                    '--rg-hover-bd': 'var(--accent-ring)',
                  } as React.CSSProperties
                }
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
                background: 'var(--surface-sunken)',
                color: 'var(--text-primary)',
                border: '1px solid var(--border-subtle)',
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
            dynamicOptions={{ stores: storeOptions }}
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
      {(!isFixed || isBoundaryPort) && (
        <div style={{ padding: '12px 16px', borderTop: '1px solid var(--border-subtle)' }}>
          <button
            onClick={() => onDeleteNode(node.id)}
            disabled={deleteDisabled}
            title={isBoundaryPort ? boundaryDeleteBlocked : undefined}
            className="rg-press rg-hover w-full"
            style={
              {
                height: 32,
                borderRadius: 'var(--radius-sm)',
                borderWidth: 1,
                borderStyle: 'solid',
                fontSize: 'var(--text-sm)',
                fontWeight: 500,
                opacity: deleteDisabled ? 0.5 : 1,
                cursor: deleteDisabled ? 'not-allowed' : 'pointer',
                '--rg-fg': 'var(--error)',
                '--rg-bd': 'color-mix(in oklch, var(--error) 40%, transparent)',
                '--rg-hover-bg': 'var(--error-soft)',
              } as React.CSSProperties
            }
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
