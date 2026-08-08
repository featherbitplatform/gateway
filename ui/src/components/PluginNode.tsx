/**
 * Custom ReactFlow node for the policy editor. Renders a gateway plugin node
 * with a per-type colored header and the in/success/error connection handles
 * that realize the gateway's success/error port routing model.
 *
 * @module components/PluginNode
 */
import { Handle, Position, type NodeProps } from '@xyflow/react';
import { Link2 } from 'lucide-react';
import { getPluginMeta } from '../pluginMeta';
import { DEFAULT_PORT_SPEC } from '../portSpecs';
import type { PortDecl, PortSpec } from '../types';

/**
 * Data payload stored on every `pluginNode` ReactFlow node. GraphCanvas
 * writes it when converting a Policy to nodes and reads it back on save,
 * so it must carry everything needed to reconstruct a policy node.
 */
export interface PluginNodeData {
  /** Text shown in the node body (usually the node id; script nodes append the runtime). */
  label: string;
  /** Gateway plugin type, e.g. `listener`, `upstream`, `key-auth`, `script`. */
  pluginType: string;
  /** Plugin configuration, serialized verbatim into the policy node's `config` on save. */
  config: Record<string, unknown>;
  /** Optional name of a shared plugin config this node inherits from. */
  configRef?: string;
  /**
   * Declared ports for this node's plugin type, from the `GET /api/plugins`
   * catalog (GraphCanvas looks this up by `pluginType` when building nodes).
   * Undefined for supernode boundary pseudo-nodes and any type missing from
   * the catalog; PluginNode falls back to {@link DEFAULT_PORT_SPEC} for the
   * latter and to a hard-coded single-port treatment for the former.
   */
  ports?: PortSpec;
  /** Called with the node id when the node is clicked; used by GraphCanvas to open the inspector. */
  onSelect?: (nodeId: string) => void;
  /** Index signature required by ReactFlow's node data constraint. */
  [key: string]: unknown;
}

/** Stroke/handle color for each port kind. */
const PORT_COLOR: Record<PortDecl['kind'], string> = {
  success: 'var(--success)',
  outcome: 'var(--accent)',
  error: 'var(--error)',
};

/**
 * Builds the inline style for a connection handle dot.
 *
 * @param color - Handle color (accent for input, success/error for outputs).
 * @returns Style with a matching soft glow.
 */
const handleStyle = (color: string): React.CSSProperties => ({
  background: color,
  width: 11,
  height: 11,
  border: '2px solid var(--surface-sunken)',
  boxShadow: `0 0 6px ${color}`,
});

/**
 * Renders one plugin node on the canvas.
 *
 * Handle layout encodes the port model:
 * - `in` (left, target) — omitted on entry-like nodes (`listener`, `input`).
 * - Outputs (right, source) — omitted on terminal-like nodes (`client`, `output`,
 *   `error`); a single centered `success` handle on entry-like nodes; otherwise
 *   one handle per port declared in `data.ports.outputs` (falling back to the
 *   default success+error pair when the type has no catalog entry), evenly
 *   spaced top to bottom and colored by {@link PortDecl.kind}. Nodes with more
 *   than two outputs get a small port-name label next to each handle, since
 *   three-plus unlabeled dots aren't distinguishable.
 *
 * Clicking the node invokes `data.onSelect(id)`; selection is shown with an
 * accent border and ring.
 *
 * @remarks
 * Handle ids are the ports serialized as `node_id.port` edge endpoints,
 * matching the success/outcome/error routing executed in src/graph/engine.rs
 * and declared in src/plugins/ports.rs. Entry-like and terminal-like nodes
 * are part of the supernode boundary pseudo-nodes (src/graph/expand.rs)
 * alongside listener/client; they are not catalog types, so their handle
 * counts stay hard-coded rather than reading `data.ports`.
 */
export function PluginNode({ id, data, selected }: NodeProps) {
  const nodeData = data as unknown as PluginNodeData;
  const meta = getPluginMeta(nodeData.pluginType);
  const Icon = meta.icon;
  // Entry-like nodes have no input handle; terminal-like nodes have no
  // outputs. `input`/`output`/`error` are supernode boundary pseudo-nodes
  // (see src/graph/expand.rs) and mirror listener/client on the canvas.
  const isEntry = nodeData.pluginType === 'listener' || nodeData.pluginType === 'input';
  const isTerminal =
    nodeData.pluginType === 'client' ||
    nodeData.pluginType === 'output' ||
    nodeData.pluginType === 'error';

  // Entry-like nodes always get the single fixed `success` port (matching
  // src/plugins/ports.rs::LISTENER_SPEC) regardless of what the catalog
  // reports for `listener`, so the boundary `input` pseudo-node — which has
  // no catalog entry at all — renders identically. Terminal-like nodes have
  // no outputs, matching CLIENT_SPEC, likewise regardless of catalog lookup.
  const outputs: PortDecl[] = isTerminal
    ? []
    : isEntry
      ? [{ name: 'success', kind: 'success', description: 'Entry into the policy pipeline.' }]
      : (nodeData.ports?.outputs ?? DEFAULT_PORT_SPEC.outputs);
  const showLabels = outputs.length > 2;

  return (
    <div
      onClick={() => nodeData.onSelect?.(id)}
      className="cursor-pointer"
      style={{
        minWidth: 'var(--node-min-w)',
        background: 'var(--surface-raised)',
        border: `2px solid ${selected ? 'var(--accent)' : 'var(--border)'}`,
        borderRadius: 'var(--radius-md)',
        boxShadow: selected
          ? '0 0 0 3px var(--accent-soft), var(--shadow-md)'
          : 'var(--shadow-sm)',
        transition:
          'border-color var(--dur-fast) var(--ease-out), box-shadow var(--dur-fast) var(--ease-out)',
      }}
    >
      {/* Header — per-type color bar */}
      <div
        className="flex items-center"
        style={{
          gap: 7,
          padding: '6px 10px',
          background: meta.color,
          borderRadius: '6px 6px 0 0',
          color: '#fff',
        }}
      >
        <Icon size={13} strokeWidth={2} style={{ opacity: 0.95, flexShrink: 0 }} />
        <span
          style={{
            fontFamily: 'var(--font-mono)',
            fontSize: 'var(--text-xs)',
            fontWeight: 'var(--weight-semibold)' as never,
            letterSpacing: 'var(--tracking-tight)',
          }}
        >
          {nodeData.pluginType}
        </span>
      </div>

      {/* Body */}
      <div
        style={{
          padding: '8px 10px',
          fontFamily: 'var(--font-mono)',
          fontSize: 'var(--text-xs)',
          color: 'var(--text-secondary)',
        }}
      >
        {nodeData.label}
      </div>

      {nodeData.configRef && (
        <div
          className="flex items-center"
          style={{
            gap: 4,
            padding: '0 10px 8px',
            fontFamily: 'var(--font-mono)',
            fontSize: 'var(--text-2xs)',
            color: 'var(--text-muted)',
          }}
          title={`Inherits shared config '${nodeData.configRef}'`}
        >
          <Link2 size={10} style={{ flexShrink: 0 }} />
          {nodeData.configRef}
        </div>
      )}

      {/* Input handle — not on entry-like nodes (listener, input) */}
      {!isEntry && (
        <Handle
          type="target"
          position={Position.Left}
          id="in"
          title={nodeData.ports?.input ?? undefined}
          style={handleStyle('var(--accent)')}
        />
      )}

      {/* Declared outputs — one handle per catalog port, colored by kind and
          evenly spaced; empty on terminal-like nodes (client, output, error). */}
      {outputs.map((p, i) => {
        const top = outputs.length === 1 ? 50 : 25 + (i * 50) / (outputs.length - 1);
        return (
          <div key={p.name}>
            <Handle
              type="source"
              position={Position.Right}
              id={p.name}
              title={`${p.name} — ${p.description}`}
              style={{
                ...handleStyle(PORT_COLOR[p.kind]),
                top: `${top}%`,
              }}
            />
            {showLabels && (
              <div
                style={{
                  position: 'absolute',
                  top: `${top}%`,
                  right: -8,
                  transform: 'translate(100%, -50%)',
                  fontSize: 'var(--text-2xs)',
                  color: 'var(--text-muted)',
                  whiteSpace: 'nowrap',
                  pointerEvents: 'none',
                }}
              >
                {p.name}
              </div>
            )}
          </div>
        );
      })}
    </div>
  );
}
