/**
 * Custom ReactFlow node for the policy editor. Renders a gateway plugin node
 * with a tinted per-type icon chip and the in/success/error connection handles
 * that realize the gateway's success/error port routing model.
 *
 * @module components/PluginNode
 */
import { Handle, Position, type NodeProps } from '@xyflow/react';
import { ChevronDown, ChevronUp, Link2 } from 'lucide-react';
import { getPluginMeta } from '../pluginMeta';
import { isEntryType, resolveOutputs } from '../nodeKinds';
import { SupernodePreview } from './SupernodePreview';
import type { PortSpecLookup } from '../portSpecs';
import type { PortDecl, PortSpec, Supernode } from '../types';

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
   * the catalog; PluginNode resolves the effective outputs via
   * {@link module:nodeKinds.resolveOutputs}, which applies the entry/terminal
   * special cases and the default success+error fallback.
   */
  ports?: PortSpec;
  /** Called with the node id when the node is clicked; used by GraphCanvas to open the inspector. */
  onSelect?: (nodeId: string) => void;
  /** When false, ports render as bare handles with hover tooltips; default true renders labeled rows. */
  showPortNames?: boolean;
  /** Resolved supernode definition for `supernode` nodes; undefined = unresolved (stale/missing reference). */
  supernodeDef?: Supernode;
  /** Catalog port-spec lookup, threaded to the expanded preview's inner nodes (supernode nodes only). */
  portSpecs?: PortSpecLookup;
  /** Whether this supernode instance is expanded to its inline preview. */
  expanded?: boolean;
  /** Called with the node id when the expand/fold chevron is clicked (supernode nodes only). */
  onToggleExpand?: (nodeId: string) => void;
  /** Index signature required by ReactFlow's node data constraint. */
  [key: string]: unknown;
}

/**
 * Handle color for each output port kind. Success and error carry the
 * routing semantics; deliberate outcome ports (denied, redirect, limited,
 * true/false, ...) stay neutral so the canvas keeps one accent hue.
 */
const PORT_COLOR: Record<PortDecl['kind'], string> = {
  success: 'var(--success)',
  outcome: 'var(--text-muted)',
  error: 'var(--error)',
};

/** Input handles use the selection violet. */
const INPUT_COLOR = 'var(--accent-border)';

/**
 * Builds the inline style for a connection handle dot: a 9px port ringed in
 * the canvas color so it reads as cut out of the card edge. No glow.
 *
 * @param color - Handle color (accent for input, success/error/neutral for outputs).
 */
const handleStyle = (color: string): React.CSSProperties => ({
  background: color,
  width: 9,
  height: 9,
  minWidth: 9,
  minHeight: 9,
  border: '2px solid var(--bg-canvas)',
  boxSizing: 'content-box',
});

/** One port row: relative so its Handle anchors to the row, not the node. */
const portRowStyle = (align: 'left' | 'right'): React.CSSProperties => ({
  position: 'relative',
  height: 18,
  lineHeight: '18px',
  padding: '0 10px',
  textAlign: align,
  fontFamily: 'var(--font-mono)',
  fontSize: 'var(--text-2xs)',
  color: 'var(--text-muted)',
  whiteSpace: 'nowrap',
});

/**
 * Renders one plugin node on the canvas.
 *
 * Handle layout encodes the port model (see `../nodeKinds` for the shared
 * entry/terminal classification and output resolution):
 * - `in` (left, target) — omitted on entry-like nodes (`listener`, `input`).
 * - Outputs (right, source) — omitted on terminal-like nodes (`client`, `output`,
 *   `error`, none of which get a `success` output either); a single `success`
 *   handle on entry-like nodes (`listener` reads its own catalog spec, which
 *   happens to be exactly one `success` port; the non-catalog `input`
 *   pseudo-node uses a hard-coded fallback of the same shape); otherwise one
 *   handle per port declared in `data.ports.outputs` (falling back to the
 *   default success+error pair when the type has no catalog entry), colored
 *   by {@link PortDecl.kind}. When `data.showPortNames` is not `false`
 *   (the default), each port renders as a labeled row inside the node body,
 *   with its handle centered on the row; otherwise handles fall back to the
 *   previous evenly-spaced absolute placement with only a hover tooltip.
 *
 * Clicking the node invokes `data.onSelect(id)`; selection is shown with an
 * accent border and ring.
 *
 * @remarks
 * Handle ids are the ports serialized as `node_id.port` edge endpoints,
 * matching the success/outcome/error routing executed in src/graph/engine.rs
 * and declared in src/plugins/ports.rs. `input`/`output`/`error` are the
 * supernode boundary pseudo-nodes (src/graph/expand.rs) alongside
 * listener/client; they are not catalog types, so their handle counts fall
 * back to the hard-coded shapes in `../nodeKinds` rather than reading
 * `data.ports`.
 */
export function PluginNode({ id, data, selected }: NodeProps) {
  const nodeData = data as unknown as PluginNodeData;
  const meta = getPluginMeta(nodeData.pluginType);
  const Icon = meta.icon;
  // Entry-like nodes have no input handle; terminal-like nodes have no
  // outputs. `input`/`output`/`error` are supernode boundary pseudo-nodes
  // (see src/graph/expand.rs) and mirror listener/client on the canvas.
  // This classification (and the output-port resolution below) is shared
  // with GraphCanvas's unwired-port save check via ../nodeKinds, so the two
  // can't independently drift on what counts as an output.
  const isEntry = isEntryType(nodeData.pluginType);
  const outputs: PortDecl[] = resolveOutputs(nodeData.pluginType, nodeData.ports);
  const isSupernode = nodeData.pluginType === 'supernode';
  const isExpanded = isSupernode && nodeData.expanded;
  // The absolute/evenly-spaced handle layout (showNames === false) assumes a
  // fixed card height; expanding adds 320px of preview below the ports, which
  // would slide those handles down into the preview area. Force the
  // labeled-rows layout whenever expanded so handles stay anchored to their
  // rows regardless of the show-port-names preference. Collapsed nodes and
  // non-supernodes are unaffected.
  const showNames = nodeData.showPortNames !== false || isExpanded;

  return (
    <div
      onClick={() => nodeData.onSelect?.(id)}
      className="cursor-pointer"
      style={{
        minWidth: 'var(--node-min-w)',
        background: 'var(--surface-raised)',
        border: `1px solid ${selected ? 'var(--accent-border)' : 'var(--border)'}`,
        borderRadius: 'var(--radius-md)',
        boxShadow: selected
          ? '0 0 0 3px var(--accent-soft), var(--shadow-md), var(--shadow-inset)'
          : isExpanded
            ? 'var(--shadow-md), var(--shadow-inset)'
            : 'var(--shadow-sm), var(--shadow-inset)',
        transition:
          'border-color var(--dur-fast) ease, box-shadow var(--dur-fast) ease',
      }}
    >
      {/* Header: tinted icon chip in the plugin's identity color, type name,
          and the node id beneath it. The plugin color never fills the card. */}
      <div className="flex items-center" style={{ gap: 9, padding: '9px 10px 8px' }}>
        <span
          aria-hidden
          className="flex items-center justify-center"
          style={{
            width: 22,
            height: 22,
            flexShrink: 0,
            borderRadius: 'var(--radius-sm)',
            background: `color-mix(in oklch, ${meta.color} 16%, transparent)`,
            color: meta.color,
          }}
        >
          <Icon size={13} strokeWidth={1.75} />
        </span>
        <div className="flex flex-col" style={{ minWidth: 0, gap: 1 }}>
          <span
            style={{
              fontFamily: 'var(--font-sans)',
              fontSize: 'var(--text-sm)',
              fontWeight: 'var(--weight-medium)' as never,
              letterSpacing: 'var(--tracking-tight)',
              lineHeight: 1.25,
              color: 'var(--text-primary)',
              whiteSpace: 'nowrap',
            }}
          >
            {nodeData.pluginType}
          </span>
          <span
            style={{
              fontFamily: 'var(--font-mono)',
              fontSize: 11,
              lineHeight: 1.3,
              color: 'var(--text-muted)',
              whiteSpace: 'nowrap',
              overflow: 'hidden',
              textOverflow: 'ellipsis',
            }}
          >
            {nodeData.label}
          </span>
        </div>
        {isSupernode && (
          <button
            onClick={(e) => {
              e.stopPropagation();
              nodeData.onToggleExpand?.(id);
            }}
            aria-label={nodeData.expanded ? 'Collapse supernode preview' : 'Expand supernode preview'}
            title={nodeData.expanded ? 'Fold preview' : 'Preview contents'}
            className="rg-press rg-hover flex items-center justify-center"
            style={
              {
                marginLeft: 'auto',
                width: 22,
                height: 22,
                flexShrink: 0,
                borderRadius: 'var(--radius-sm)',
                border: '1px solid transparent',
                '--rg-fg': 'var(--text-muted)',
                '--rg-hover-fg': 'var(--text-primary)',
              } as React.CSSProperties
            }
          >
            {nodeData.expanded ? <ChevronUp size={14} /> : <ChevronDown size={14} />}
          </button>
        )}
      </div>

      {nodeData.configRef && (
        <div
          className="flex items-center"
          style={{
            gap: 4,
            padding: '0 10px 8px 41px',
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

      {/* Ports. With names shown, each port is a labeled row and its handle
          sits at the row's vertical centre — @xyflow anchors edges off DOM
          layout, so rows and handles stay aligned however tall the header
          and body grow. With names hidden, handles keep the previous
          evenly-spaced absolute placement. */}
      {showNames ? (
        <div style={{ borderTop: '1px solid var(--border-subtle)', padding: '4px 0' }}>
          {!isEntry && (
            <div style={portRowStyle('left')}>
              <Handle
                type="target"
                position={Position.Left}
                id="in"
                title={nodeData.ports?.input ?? undefined}
                style={{ ...handleStyle(INPUT_COLOR), top: '50%' }}
              />
              in
            </div>
          )}
          {outputs.map((p) => (
            <div key={p.name} style={portRowStyle('right')}>
              {p.name}
              <Handle
                type="source"
                position={Position.Right}
                id={p.name}
                title={`${p.name} — ${p.description}`}
                style={{ ...handleStyle(PORT_COLOR[p.kind]), top: '50%' }}
              />
            </div>
          ))}
        </div>
      ) : (
        <>
          {!isEntry && (
            <Handle
              type="target"
              position={Position.Left}
              id="in"
              title={nodeData.ports?.input ?? undefined}
              style={handleStyle(INPUT_COLOR)}
            />
          )}
          {outputs.map((p, i) => (
            <Handle
              key={p.name}
              type="source"
              position={Position.Right}
              id={p.name}
              title={`${p.name} — ${p.description}`}
              style={{
                ...handleStyle(PORT_COLOR[p.kind]),
                top: `${outputs.length === 1 ? 50 : 25 + (i * 50) / (outputs.length - 1)}%`,
              }}
            />
          ))}
        </>
      )}

      {/* Expanded supernode preview. Sits BELOW the port rows so the
          in/success/error handles barely move on expand — @xyflow re-anchors
          edges off DOM layout either way. nowheel/nopan/nodrag isolate
          preview events from the outer canvas; stopPropagation keeps a
          preview click from opening the inspector. */}
      {isSupernode && nodeData.expanded && (
        <div
          data-testid="supernode-preview"
          className="nowheel nopan nodrag"
          onClick={(e) => e.stopPropagation()}
          style={{
            width: 480,
            height: 320,
            borderTop: '1px solid var(--border-subtle)',
            borderRadius: '0 0 7px 7px',
            overflow: 'hidden',
            background: 'var(--bg-canvas)',
          }}
        >
          <SupernodePreview
            name={typeof nodeData.config?.name === 'string' ? nodeData.config.name : undefined}
            supernode={nodeData.supernodeDef}
            portSpecs={nodeData.portSpecs ?? {}}
          />
        </div>
      )}
    </div>
  );
}
