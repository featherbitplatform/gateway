/**
 * Read-only, zoomed-out rendering of a supernode's inner graph, hosted
 * inside an expanded supernode card on the policy canvas (see PluginNode).
 *
 * A second, miniature ReactFlow instance (its own provider) fed by the same
 * Policy→ReactFlow conversion as the main canvas — a {@link Supernode}
 * shares the Policy nodes/edges shape, and the shared auto-layout already
 * treats the `input` boundary pseudo-node as the entry. Every interaction
 * flag is off; the host panel in PluginNode carries the `nowheel nopan
 * nodrag` classes that keep preview events from panning or zooming the
 * outer canvas.
 *
 * @module components/SupernodePreview
 */
import { useMemo } from 'react';
import { Background, ReactFlow, ReactFlowProvider } from '@xyflow/react';

import { policyToEdges, policyToNodes } from '../policyGraph';
import type { PortSpecLookup } from '../portSpecs';
import type { Supernode } from '../types';
import { PluginNode } from './PluginNode';

/**
 * Nested-instance node registry; inner nodes reuse the main canvas renderer.
 * PluginNode ↔ SupernodePreview is a module cycle (the preview renders the
 * same node component that hosts it); PluginNode is a hoisted function
 * declaration, so referencing it at module scope here is initialization-safe.
 */
const nodeTypes = { pluginNode: PluginNode };

/** Inner nodes are inert; the conversion still requires a select callback. */
const noSelect = () => {};

/** Props for {@link SupernodePreview}. */
interface SupernodePreviewProps {
  /** Referenced definition name (the instance's `config.name`), for the not-found message. */
  name?: string;
  /** Resolved definition; undefined renders the inline not-found state instead of a graph. */
  supernode?: Supernode;
  /** Catalog port-spec lookup driving the inner nodes' handles and edge colors. */
  portSpecs: PortSpecLookup;
}

/**
 * Renders the inner graph zoomed out to fit the host panel, or an inline
 * error when the reference does not resolve (stale/missing definition —
 * the editor keeps working; the server rejects such a save anyway).
 */
export function SupernodePreview({ name, supernode, portSpecs }: SupernodePreviewProps) {
  const graph = useMemo(() => {
    if (!supernode) return null;
    const asPolicy = { name: supernode.name, nodes: supernode.nodes, edges: supernode.edges };
    return {
      // Port labels off: at thumbnail scale they are noise. Individual
      // interaction flags off on every node, belt-and-braces alongside the
      // instance-wide flags below.
      nodes: policyToNodes(asPolicy, noSelect, portSpecs, false).map((n) => ({
        ...n,
        draggable: false,
        connectable: false,
        selectable: false,
      })),
      edges: policyToEdges(asPolicy, portSpecs),
    };
  }, [supernode, portSpecs]);

  if (!graph) {
    return (
      <div
        className="flex items-center justify-center h-full"
        style={{
          fontFamily: 'var(--font-mono)',
          fontSize: 'var(--text-xs)',
          color: 'var(--text-muted)',
          padding: 12,
          textAlign: 'center',
        }}
      >
        {name ? `supernode '${name}' not found` : 'no supernode selected'}
      </div>
    );
  }

  return (
    <ReactFlowProvider>
      <ReactFlow
        nodes={graph.nodes}
        edges={graph.edges}
        nodeTypes={nodeTypes}
        fitView
        fitViewOptions={{ padding: 0.15 }}
        minZoom={0.05}
        nodesDraggable={false}
        nodesConnectable={false}
        nodesFocusable={false}
        edgesFocusable={false}
        elementsSelectable={false}
        panOnDrag={false}
        panOnScroll={false}
        zoomOnScroll={false}
        zoomOnPinch={false}
        zoomOnDoubleClick={false}
        preventScrolling={false}
      >
        <Background gap={20} size={1} color="var(--grid-dot)" />
      </ReactFlow>
    </ReactFlowProvider>
  );
}
