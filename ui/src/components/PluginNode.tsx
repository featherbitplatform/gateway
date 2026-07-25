/**
 * Custom ReactFlow node for the policy editor. Renders a gateway plugin node
 * with a per-type colored header and the in/success/error connection handles
 * that realize the gateway's success/error port routing model.
 *
 * @module components/PluginNode
 */
import { Handle, Position, type NodeProps } from '@xyflow/react';
import { getPluginMeta } from '../pluginMeta';

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
  /** Called with the node id when the node is clicked; used by GraphCanvas to open the inspector. */
  onSelect?: (nodeId: string) => void;
  /** Index signature required by ReactFlow's node data constraint. */
  [key: string]: unknown;
}

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
 * - `in` (left, target) — omitted on `listener`, the pipeline entry point.
 * - `success` (right, source) — omitted on `client`, the terminal node;
 *   centered on `listener`, otherwise paired with the error handle.
 * - `error` (right, source, lower) — only on regular plugin nodes, i.e.
 *   neither `listener` nor `client`.
 *
 * Clicking the node invokes `data.onSelect(id)`; selection is shown with an
 * accent border and ring.
 *
 * @remarks
 * These handle ids are the ports serialized as `node_id.port` edge endpoints,
 * matching the success/error routing executed in src/graph/engine.rs.
 */
export function PluginNode({ id, data, selected }: NodeProps) {
  const nodeData = data as unknown as PluginNodeData;
  const meta = getPluginMeta(nodeData.pluginType);
  const Icon = meta.icon;
  const isListener = nodeData.pluginType === 'listener';
  const isClient = nodeData.pluginType === 'client';

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

      {/* Input handle — not on listener (it's the entry point) */}
      {!isListener && (
        <Handle
          type="target"
          position={Position.Left}
          id="in"
          style={handleStyle('var(--accent)')}
        />
      )}

      {/* Success output — not on client (it's the terminal) */}
      {!isClient && (
        <Handle
          type="source"
          position={Position.Right}
          id="success"
          style={{
            ...handleStyle('var(--success)'),
            top: isListener ? '50%' : '36%',
          }}
        />
      )}

      {/* Error output — only on regular plugin nodes */}
      {!isListener && !isClient && (
        <Handle
          type="source"
          position={Position.Right}
          id="error"
          style={{ ...handleStyle('var(--error)'), top: '68%' }}
        />
      )}
    </div>
  );
}
