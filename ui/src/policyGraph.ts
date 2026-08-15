/**
 * Policy ⇄ ReactFlow conversion helpers shared by the policy editor canvas
 * (GraphCanvas) and the read-only supernode preview (SupernodePreview).
 *
 * A gateway {@link Policy} — and a `Supernode`, which shares the same
 * nodes/edges shape — converts to ReactFlow nodes/edges with `node_id.port`
 * endpoints split per src/graph/engine.rs::parse_edge_endpoint, saved
 * positions honored, auto-layout for never-saved graphs, and edge styling
 * driven by each source port's declared kind.
 *
 * @module policyGraph
 */
import { MarkerType, type Edge, type Node } from '@xyflow/react';

import type { PluginNodeData } from './components/PluginNode';
import { resolveOutputs } from './nodeKinds';
import type { PortSpecLookup } from './portSpecs';
import type { PortDecl, Policy } from './types';

/** Stroke color for each port kind, used for both edges and connection previews. */
export const PORT_STROKE: Record<PortDecl['kind'], string> = {
  success: 'var(--success)',
  outcome: 'var(--accent)',
  error: 'var(--error)',
};

/**
 * Resolves the {@link PortDecl.kind} of a source node's output port, falling
 * back to `success` styling for a port name the type's catalog spec doesn't
 * declare (or a type missing from the catalog entirely) — mirroring
 * PluginNode's default-pair fallback so an unknown port never renders as an
 * error edge by mistake.
 *
 * Resolves the node's effective outputs via `resolveOutputs` — the same
 * helper PluginNode uses to render handles — so entry/terminal types (e.g.
 * the supernode boundary pseudo-nodes) are classified identically here and
 * on the canvas.
 *
 * @param sourceType - Plugin type of the edge's source node.
 * @param port - Source port name (already normalized; `out` should be
 *   resolved to `success` by the caller).
 * @param portSpecs - Catalog-derived lookup from `buildPortSpecs`.
 */
export function portKindFor(
  sourceType: string | undefined,
  port: string,
  portSpecs: PortSpecLookup
): PortDecl['kind'] {
  const outputs = sourceType ? resolveOutputs(sourceType, portSpecs[sourceType]) : undefined;
  const decl = outputs?.find((p) => p.name === port);
  return decl?.kind ?? (port === 'error' ? 'error' : 'success');
}

/**
 * Converts a gateway {@link Policy} into ReactFlow nodes.
 *
 * Saved `position` values on policy nodes win; nodes without one get an
 * auto-layout position: starting from the `listener` node, each success-port
 * successor is placed one column (250px) to the right on the same row, while
 * an error-port successor drops 1.5 rows (225px) below. Nodes unreachable
 * from the listener are appended in subsequent columns at y=300.
 *
 * @param policy - Policy whose `nodes`/`edges` describe the graph.
 * @param onSelect - Callback wired into each node's data so clicking a
 *   rendered {@link PluginNode} selects it in the canvas.
 * @param portSpecs - Catalog-derived lookup threaded into each node's
 *   {@link PluginNodeData.ports} so PluginNode can render the declared
 *   handles; a type missing from the lookup leaves `ports` undefined and
 *   PluginNode synthesizes the default success+error pair.
 * @param showPortNames - Current value of the persisted port-names
 *   preference, threaded into every node's {@link PluginNodeData.showPortNames}.
 * @returns ReactFlow nodes of type `pluginNode` carrying {@link PluginNodeData}.
 */
export function policyToNodes(
  policy: Policy,
  onSelect: (id: string) => void,
  portSpecs: PortSpecLookup,
  showPortNames: boolean
): Node[] {
  const positions = new Map<string, { x: number; y: number }>();

  // Auto-layout: place listener at left, then each connected node to the right
  const visited = new Set<string>();
  const successMap = new Map<string, string>();
  const errorMap = new Map<string, string>();

  for (const edge of policy.edges) {
    const [fromNode, fromPort] = splitEdge(edge.from);
    const [toNode] = splitEdge(edge.to);
    if (fromPort === 'error') {
      errorMap.set(fromNode, toNode);
    } else {
      successMap.set(fromNode, toNode);
    }
  }

  let col = 0;
  function layout(nodeId: string, row: number) {
    if (visited.has(nodeId)) return;
    visited.add(nodeId);
    positions.set(nodeId, { x: col * 250, y: row * 150 });
    col++;
    const next = successMap.get(nodeId);
    if (next) layout(next, row);
    const errNext = errorMap.get(nodeId);
    if (errNext) layout(errNext, row + 1.5);
  }

  const entryNode = policy.nodes.find((n) => n.type === 'listener' || n.type === 'input');
  if (entryNode) layout(entryNode.id, 1);

  // Place any unvisited nodes
  for (const node of policy.nodes) {
    if (!visited.has(node.id)) {
      positions.set(node.id, { x: col * 250, y: 300 });
      col++;
    }
  }

  return policy.nodes.map((node) => ({
    id: node.id,
    type: 'pluginNode',
    position: node.position || positions.get(node.id) || { x: 0, y: 0 },
    data: {
      label:
        node.type === 'supernode' && typeof node.config?.name === 'string'
          ? `⬡ ${node.config.name}`
          : node.id,
      pluginType: node.type,
      config: node.config || {},
      configRef: node.config_ref,
      ports: portSpecs[node.type],
      onSelect: onSelect,
      showPortNames,
    } satisfies PluginNodeData,
  }));
}

/**
 * Converts a gateway {@link Policy}'s edges into styled ReactFlow edges.
 *
 * Each `node_id.port` endpoint is split with `splitEdge`. A source port
 * of `out` is normalized to the `success` handle (PluginNode renders no `out`
 * handle). Each edge's color/animation is driven by its source port's
 * declared {@link PortDecl.kind} (via `portKindFor`): `error` → red
 * (`var(--error)`) and animated, `outcome` → accent (`var(--accent)`),
 * `success` (or an unknown port/type) → green (`var(--success)`). Edge ids
 * are positional (`e-<index>`).
 *
 * @param policy - Policy whose edges use the `node_id.port` endpoint format.
 * @param portSpecs - Catalog-derived lookup used to resolve each source
 *   port's kind.
 * @returns ReactFlow edges targeting each node's `in` handle.
 *
 * @remarks
 * The endpoint format mirrors what the Rust engine parses in
 * src/graph/engine.rs (parse_edge_endpoint), where error-port edges feed the
 * error-routing table used by CompiledGraph::execute and outcome-port edges
 * feed the named-port routing table.
 */
export function policyToEdges(policy: Policy, portSpecs: PortSpecLookup): Edge[] {
  return policy.edges.map((edge, i) => {
    const [fromNode, fromPort] = splitEdge(edge.from);
    const [toNode] = splitEdge(edge.to);
    const sourceHandle = fromPort === 'out' ? 'success' : fromPort;
    const sourceType = policy.nodes.find((n) => n.id === fromNode)?.type;
    const kind = portKindFor(sourceType, sourceHandle, portSpecs);
    const color = PORT_STROKE[kind];

    return {
      id: `e-${i}`,
      source: fromNode,
      sourceHandle,
      target: toNode,
      targetHandle: 'in',
      animated: kind === 'error',
      style: { stroke: color, strokeWidth: 2 },
      markerEnd: { type: MarkerType.ArrowClosed, color },
    };
  });
}

/**
 * Splits a `node_id.port` edge endpoint into its node id and port.
 *
 * Splits on the last dot so node ids containing dots stay intact; an
 * endpoint with no dot yields the default port `out`.
 *
 * @param endpoint - Endpoint string such as `upstream.error` or `listener.out`.
 * @returns Tuple of `[nodeId, port]`.
 *
 * @remarks
 * TypeScript counterpart of parse_edge_endpoint in src/graph/engine.rs —
 * the two must agree for policies to round-trip between UI and gateway.
 */
export function splitEdge(endpoint: string): [string, string] {
  const dot = endpoint.lastIndexOf('.');
  if (dot === -1) return [endpoint, 'out'];
  return [endpoint.substring(0, dot), endpoint.substring(dot + 1)];
}
