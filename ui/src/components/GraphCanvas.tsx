/**
 * Node-graph policy editor canvas built on ReactFlow (@xyflow/react).
 * Round-trips a gateway {@link Policy} (YAML contract: nodes plus edges with
 * `node_id.port` endpoints) to and from the ReactFlow graph, hosts the
 * add-node drawer and node inspector, and emits the rebuilt policy on save.
 *
 * @module components/GraphCanvas
 */
import { useCallback, useMemo, useState } from 'react';
import {
  ReactFlow,
  Background,
  Controls,
  MiniMap,
  addEdge,
  useNodesState,
  useEdgesState,
  type Connection,
  type Edge,
  type Node,
  MarkerType,
  Panel,
} from '@xyflow/react';
import '@xyflow/react/dist/style.css';

import { GitFork, Plus, Save, Trash2 } from 'lucide-react';
import { PluginNode, type PluginNodeData } from './PluginNode';
import { PluginDrawer } from './PluginDrawer';
import { NodeInspector } from './NodeInspector';
import { ThemeToggle } from './ThemeToggle';
import type { Policy, PluginType, ScriptFile } from '../types';

/**
 * Builds the shared inline style for floating-toolbar buttons.
 *
 * @param bg - CSS background value (typically a design-token variable).
 * @returns Style object for a compact icon-plus-label toolbar button.
 */
const toolbarButtonStyle = (bg: string): React.CSSProperties => ({
  display: 'flex',
  alignItems: 'center',
  gap: 6,
  padding: '5px 10px',
  borderRadius: 'var(--radius-sm)',
  fontSize: 'var(--text-xs)',
  fontWeight: 500,
  background: bg,
  color: 'var(--text-on-accent)',
  transition: 'filter var(--dur-fast) var(--ease-out)',
});

/** Props for {@link GraphCanvas}. */
interface GraphCanvasProps {
  /** Policy being edited; `null` renders the empty "Select a route" state. */
  policy: Policy | null;
  /** Native plugin types available in the add-node drawer (from GET /api/plugins). */
  plugins: PluginType[];
  /** Script files available as script nodes in the drawer (from GET /api/scripts). */
  scripts: ScriptFile[];
  /** Fires when the user clicks Save Policy, with the graph converted back to the Policy contract. */
  onSavePolicy: (policy: Policy) => void;
}

/** ReactFlow custom node-type registry; every policy node renders as a {@link PluginNode}. */
const nodeTypes = { pluginNode: PluginNode };

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
 * @returns ReactFlow nodes of type `pluginNode` carrying {@link PluginNodeData}.
 */
function policyToNodes(policy: Policy, onSelect: (id: string) => void): Node[] {
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

  const listenerNode = policy.nodes.find((n) => n.type === 'listener');
  if (listenerNode) layout(listenerNode.id, 1);

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
      label: node.id,
      pluginType: node.type,
      config: node.config || {},
      onSelect: onSelect,
    } satisfies PluginNodeData,
  }));
}

/**
 * Converts a gateway {@link Policy}'s edges into styled ReactFlow edges.
 *
 * Each `node_id.port` endpoint is split with `splitEdge`. A source port
 * of `out` is normalized to the `success` handle (PluginNode renders no `out`
 * handle). Edges leaving an `error` port are identified by that source port
 * alone and styled distinctly: animated, red (`var(--error)`) stroke and
 * arrowhead; all other edges are green (`var(--success)`). Edge ids are
 * positional (`e-<index>`).
 *
 * @param policy - Policy whose edges use the `node_id.port` endpoint format.
 * @returns ReactFlow edges targeting each node's `in` handle.
 *
 * @remarks
 * The endpoint format mirrors what the Rust engine parses in
 * src/graph/engine.rs (parse_edge_endpoint), where error-port edges feed the
 * error-routing table used by CompiledGraph::execute.
 */
function policyToEdges(policy: Policy): Edge[] {
  return policy.edges.map((edge, i) => {
    const [fromNode, fromPort] = splitEdge(edge.from);
    const [toNode] = splitEdge(edge.to);
    const isError = fromPort === 'error';

    return {
      id: `e-${i}`,
      source: fromNode,
      sourceHandle: fromPort === 'out' ? 'success' : fromPort,
      target: toNode,
      targetHandle: 'in',
      animated: isError,
      style: { stroke: isError ? 'var(--error)' : 'var(--success)', strokeWidth: 2 },
      markerEnd: { type: MarkerType.ArrowClosed, color: isError ? 'var(--error)' : 'var(--success)' },
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
function splitEdge(endpoint: string): [string, string] {
  const dot = endpoint.lastIndexOf('.');
  if (dot === -1) return [endpoint, 'out'];
  return [endpoint.substring(0, dot), endpoint.substring(dot + 1)];
}

/**
 * Converts the ReactFlow graph back into the gateway {@link Policy} contract
 * (inverse of `policyToNodes`/`policyToEdges`, used on save).
 *
 * Every node's current canvas position is rounded to whole pixels and
 * persisted as `position`, so auto-layout only ever runs on policies that
 * have never been saved from the UI. Edge endpoints are re-serialized as
 * `node_id.port`, defaulting missing handles to `success` (source) and `in`
 * (target); node type and config are taken from each node's
 * {@link PluginNodeData}, preserving the ids/types/configs/edges round-trip.
 *
 * @param policyName - Policy name to keep (not editable on the canvas).
 * @param nodes - Current ReactFlow nodes.
 * @param edges - Current ReactFlow edges.
 * @param errorHandler - Optional catch-all error-handler node id, passed
 *   through unchanged as `error_handler`.
 * @returns Policy in the shape the gateway's YAML/Admin API expects.
 *
 * @remarks
 * The resulting edges are what src/graph/engine.rs compiles into a
 * CompiledGraph; `error_handler` becomes its catch-all handler.
 */
function nodesToPolicy(
  policyName: string,
  nodes: Node[],
  edges: Edge[],
  errorHandler?: string
): Policy {
  return {
    name: policyName,
    error_handler: errorHandler,
    nodes: nodes.map((n) => {
      const data = n.data as unknown as PluginNodeData;
      return {
        id: n.id,
        type: data.pluginType,
        config: data.config || {},
        position: { x: Math.round(n.position.x), y: Math.round(n.position.y) },
      };
    }),
    edges: edges.map((e) => ({
      from: `${e.source}.${e.sourceHandle || 'success'}`,
      to: `${e.target}.${e.targetHandle || 'in'}`,
    })),
  };
}

/**
 * Interactive policy editor: renders the policy as a ReactFlow graph and lets
 * the user add nodes (via {@link PluginDrawer}), edit node config (via
 * {@link NodeInspector}), draw/delete edges, and reposition nodes.
 *
 * Behavior contracts:
 * - The canvas re-syncs from the `policy` prop only when the policy *name*
 *   changes (the parent keys this component by name, so that is a remount);
 *   in-canvas edits are local until Save Policy invokes `onSavePolicy` with
 *   the graph rebuilt by `nodesToPolicy`.
 * - New connections enforce single-edge-per-input, except `client` targets
 *   (multiple paths return the response) and `error-handler` targets
 *   (collect errors from many nodes). Edges drawn from an `error` handle get
 *   the animated red error styling.
 * - Selecting a node opens the inspector (unless the drawer is open);
 *   clicking an edge selects it and reveals a Delete Edge button; clicking
 *   the pane clears both selections. Deleting a node also removes its edges.
 *
 * @remarks
 * Success/error handles correspond to the port routing model executed by
 * CompiledGraph::execute in src/graph/engine.rs.
 */
export function GraphCanvas({ policy, plugins, scripts, onSavePolicy }: GraphCanvasProps) {
  const [selectedNodeId, setSelectedNodeId] = useState<string | null>(null);
  const [selectedEdgeId, setSelectedEdgeId] = useState<string | null>(null);
  const [drawerOpen, setDrawerOpen] = useState(false);

  const handleSelect = useCallback((id: string) => {
    setSelectedNodeId(id);
    setDrawerOpen(false);
  }, []);

  // The parent keys this component by policy name, so a different policy
  // remounts the canvas and nodes/edges/selection all start fresh from the
  // prop. Refetches of the same policy keep the local (unsaved) graph state.
  const initialNodes = useMemo(
    () => (policy ? policyToNodes(policy, handleSelect) : []),
    [policy, handleSelect]
  );
  const initialEdges = useMemo(() => (policy ? policyToEdges(policy) : []), [policy]);

  const [nodes, setNodes, onNodesChange] = useNodesState(initialNodes);
  const [edges, setEdges, onEdgesChange] = useEdgesState(initialEdges);

  const onConnect = useCallback(
    (connection: Connection) => {
      setEdges((eds) => {
        // Check if the target input already has an edge.
        // Exceptions: listener.in (multiple paths return response) and
        // error-handler nodes (receive errors from multiple nodes).
        const targetNode = nodes.find((n) => n.id === connection.target);
        const targetType = (targetNode?.data as unknown as PluginNodeData)?.pluginType;
        const isClient = targetType === 'client';
        const isErrorHandler = targetType === 'error-handler';

        if (!isClient && !isErrorHandler) {
          const alreadyConnected = eds.some(
            (e) => e.target === connection.target && e.targetHandle === (connection.targetHandle || 'in')
          );
          if (alreadyConnected) {
            return eds;
          }
        }

        const isError = connection.sourceHandle === 'error';
        return addEdge(
          {
            ...connection,
            animated: isError,
            style: { stroke: isError ? 'var(--error)' : 'var(--success)', strokeWidth: 2 },
            markerEnd: {
              type: MarkerType.ArrowClosed,
              color: isError ? 'var(--error)' : 'var(--success)',
            },
          },
          eds
        );
      });
    },
    [setEdges, nodes]
  );

  const onEdgeClick = useCallback((_event: React.MouseEvent, edge: Edge) => {
    setSelectedEdgeId(edge.id);
    setSelectedNodeId(null);
  }, []);

  const handleDeleteEdge = useCallback(() => {
    if (selectedEdgeId) {
      setEdges((eds) => eds.filter((e) => e.id !== selectedEdgeId));
      setSelectedEdgeId(null);
    }
  }, [selectedEdgeId, setEdges]);

  const selectedNode = nodes.find((n) => n.id === selectedNodeId) || null;

  const handleAddPlugin = (type: string) => {
    const id = `${type}-${Date.now().toString(36)}`;
    const newNode: Node = {
      id,
      type: 'pluginNode',
      position: { x: 300, y: 200 + nodes.length * 80 },
      data: {
        label: id,
        pluginType: type,
        config: {},
        onSelect: handleSelect,
      } satisfies PluginNodeData,
    };
    setNodes((nds) => [...nds, newNode]);
    setSelectedNodeId(id);
    setDrawerOpen(false);
  };

  const handleAddScript = (script: ScriptFile) => {
    const id = `${script.name}-${Date.now().toString(36)}`;
    const newNode: Node = {
      id,
      type: 'pluginNode',
      position: { x: 300, y: 200 + nodes.length * 80 },
      data: {
        label: `${script.name} (${script.runtime})`,
        pluginType: 'script',
        config: {
          runtime: script.runtime,
          source: script.file,
        },
        onSelect: handleSelect,
      } satisfies PluginNodeData,
    };
    setNodes((nds) => [...nds, newNode]);
    setSelectedNodeId(id);
    setDrawerOpen(false);
  };

  const handleUpdateConfig = (nodeId: string, config: Record<string, unknown>) => {
    setNodes((nds) =>
      nds.map((n) =>
        n.id === nodeId
          ? { ...n, data: { ...n.data, config } }
          : n
      )
    );
  };

  const handleDeleteNode = (nodeId: string) => {
    setNodes((nds) => nds.filter((n) => n.id !== nodeId));
    setEdges((eds) => eds.filter((e) => e.source !== nodeId && e.target !== nodeId));
    setSelectedNodeId(null);
  };

  const handleSave = () => {
    if (!policy) return;
    const updated = nodesToPolicy(policy.name, nodes, edges, policy.error_handler);
    console.log('Saving policy:', JSON.stringify(updated, null, 2));
    onSavePolicy(updated);
  };

  if (!policy) {
    return (
      <div
        className="flex-1 flex items-center justify-center"
        style={{
          backgroundColor: 'var(--bg-canvas)',
          backgroundImage: 'radial-gradient(var(--grid-dot) 1px, transparent 0)',
          backgroundSize: 'var(--grid-gap) var(--grid-gap)',
          backgroundPosition: '-1px -1px',
        }}
      >
        <div className="text-center">
          <GitFork
            size={28}
            strokeWidth={1.5}
            style={{ color: 'var(--text-muted)', margin: '0 auto 12px' }}
          />
          <p
            style={{
              fontSize: 'var(--text-md)',
              fontWeight: 600,
              color: 'var(--text-primary)',
              margin: '0 0 4px',
            }}
          >
            Select a route
          </p>
          <p style={{ fontSize: 'var(--text-sm)', color: 'var(--text-secondary)', margin: 0 }}>
            Choose a route to edit its routing policy
          </p>
        </div>
      </div>
    );
  }

  return (
    <div className="flex-1 relative" style={{ background: 'var(--bg-canvas)' }}>
      <ReactFlow
        nodes={nodes}
        edges={edges.map((e) => ({
          ...e,
          selected: e.id === selectedEdgeId,
          style: {
            ...e.style,
            strokeWidth: e.id === selectedEdgeId ? 4 : 2,
            filter: e.id === selectedEdgeId ? 'drop-shadow(0 0 4px var(--accent))' : undefined,
          },
        }))}
        onNodesChange={onNodesChange}
        onEdgesChange={onEdgesChange}
        onConnect={onConnect}
        onEdgeClick={onEdgeClick}
        nodeTypes={nodeTypes}
        deleteKeyCode={['Backspace', 'Delete']}
        fitView
        snapToGrid
        snapGrid={[20, 20]}
        onPaneClick={() => { setSelectedNodeId(null); setSelectedEdgeId(null); }}
      >
        <Background gap={20} size={1} color="var(--grid-dot)" />
        <Controls />
        <MiniMap maskColor="rgba(8,11,20,0.35)" nodeColor="var(--surface-input)" />
        <Panel position="top-right">
          {/* Floating toolbar — glassy cluster */}
          <div
            className="flex items-center gap-1.5"
            style={{
              padding: 6,
              borderRadius: 'var(--radius-md)',
              background: 'color-mix(in srgb, var(--surface) 78%, transparent)',
              backdropFilter: 'blur(10px)',
              WebkitBackdropFilter: 'blur(10px)',
              border: '1px solid var(--border)',
              boxShadow: 'var(--shadow-md)',
            }}
          >
            <ThemeToggle />
            <span
              style={{ width: 1, height: 18, background: 'var(--border)', margin: '0 2px' }}
            />
            <button
              onClick={() => {
                setDrawerOpen(!drawerOpen);
                setSelectedNodeId(null);
              }}
              style={{
                ...toolbarButtonStyle('var(--surface-input)'),
                color: 'var(--text-primary)',
                border: '1px solid var(--border)',
              }}
              onMouseEnter={(e) => (e.currentTarget.style.filter = 'brightness(1.08)')}
              onMouseLeave={(e) => (e.currentTarget.style.filter = 'none')}
            >
              <Plus size={13} />
              Add Node
            </button>
            {selectedEdgeId && (
              <button
                onClick={handleDeleteEdge}
                style={toolbarButtonStyle('var(--error)')}
                onMouseEnter={(e) => (e.currentTarget.style.filter = 'brightness(1.08)')}
                onMouseLeave={(e) => (e.currentTarget.style.filter = 'none')}
              >
                <Trash2 size={13} />
                Delete Edge
              </button>
            )}
            <button
              onClick={handleSave}
              style={toolbarButtonStyle('var(--accent)')}
              onMouseEnter={(e) => (e.currentTarget.style.filter = 'brightness(1.08)')}
              onMouseLeave={(e) => (e.currentTarget.style.filter = 'none')}
            >
              <Save size={13} />
              Save Policy
            </button>
          </div>
        </Panel>
      </ReactFlow>

      <PluginDrawer
        plugins={plugins}
        scripts={scripts}
        onAddPlugin={handleAddPlugin}
        onAddScript={handleAddScript}
        isOpen={drawerOpen}
        onClose={() => setDrawerOpen(false)}
      />

      {selectedNodeId && !drawerOpen && (
        <NodeInspector
          node={selectedNode}
          onUpdateConfig={handleUpdateConfig}
          onDeleteNode={handleDeleteNode}
          onClose={() => setSelectedNodeId(null)}
        />
      )}
    </div>
  );
}
