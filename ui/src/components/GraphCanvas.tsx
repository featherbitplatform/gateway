/**
 * Node-graph policy editor canvas built on ReactFlow (@xyflow/react).
 * Round-trips a gateway {@link Policy} (YAML contract: nodes plus edges with
 * `node_id.port` endpoints) to and from the ReactFlow graph, hosts the
 * add-node drawer and node inspector, and emits the rebuilt policy on save.
 *
 * @module components/GraphCanvas
 */
import { useCallback, useEffect, useMemo, useState } from 'react';
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

import { Command, GitFork, Plus, Save, Trash2 } from 'lucide-react';
import { PluginNode, type PluginNodeData } from './PluginNode';
import { PluginDrawer } from './PluginDrawer';
import { NodeInspector } from './NodeInspector';
import { ThemeToggle } from './ThemeToggle';
import { useRegisterEditorAction } from '../editorActions';
import type {
  DebugConfig,
  Policy,
  PluginConfigDef,
  PluginType,
  ScriptFile,
  Supernode,
} from '../types';
import { edgesAfterConnect } from '../connectionRules';
import { buildPortSpecs, type PortSpecLookup } from '../portSpecs';
import { resolveOutputs } from '../nodeKinds';
import { PORT_STROKE, policyToEdges, policyToNodes, portKindFor } from '../policyGraph';

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
  /**
   * Fires just before `onSavePolicy`, only when the graph has mandatory
   * (`success`/`outcome`) ports with no outgoing edge — a client-side
   * heads-up ahead of the server's authoritative "must be wired" rejection
   * (see `findUnwiredPorts`); the save attempt proceeds regardless.
   */
  onSaveWarning?: (title: string, message: string) => void;
  /** Whether the canvas is editing a policy or a supernode definition. */
  kind: 'policy' | 'supernode';
  /** Supernode definitions offered in the policy palette (empty in supernode mode). */
  supernodes: Supernode[];
  /** Named shared plugin configs offered by the inspector's picker (from GET /api/plugin-configs). */
  pluginConfigs: PluginConfigDef[];
  /** Debug settings (enabled/capture_bodies/...), threaded to the inspector's var-suggestion hook. */
  debugConfig: DebugConfig | null;
  /**
   * Current value of the persisted port-names preference. Owned by App (a
   * single `usePortNames()` call) so the command palette's toggle and this
   * canvas always agree — see the `showPortNames` doc on {@link policyToNodes}.
   */
  showPortNames: boolean;
  /** Opens the App-level command palette; omitted renders no toolbar button. */
  onOpenPalette?: () => void;
}

/** ReactFlow custom node-type registry; every policy node renders as a {@link PluginNode}. */
const nodeTypes = { pluginNode: PluginNode };

/**
 * Finds every `success`/`outcome` port across a policy's nodes that has no
 * outgoing edge.
 *
 * The gateway rejects a saved policy that leaves a mandatory port unwired
 * (error ports are optional — the engine falls back to the policy's
 * `error_handler`, then a generic 500), so this lets the editor warn before
 * the round-trip to the server, without duplicating or overriding that
 * server-side validation.
 *
 * Uses `resolveOutputs` — the same entry/terminal-aware helper PluginNode
 * uses to render handles — rather than reading the catalog spec directly.
 * That matters for the supernode boundary pseudo-nodes: `output`/`error`
 * are terminal (no outputs at all, and `src/graph/validation.rs::validate_supernode`
 * forbids any outgoing edge from them), but neither has a catalog entry, so
 * reading the catalog spec naively falls back to the default success+error
 * pair and wrongly demands an unwired `output.success`/`error.success` edge
 * on every supernode. `resolveOutputs` special-cases them to zero outputs.
 *
 * @param policy - Policy already rebuilt by `nodesToPolicy` (so `edge.from`
 *   is already the exact `node_id.port` string to match against).
 * @param portSpecs - Catalog-derived lookup used to enumerate each node
 *   type's declared outputs.
 * @returns `node_id.port` strings for every unwired mandatory port, in node order.
 */
function findUnwiredPorts(policy: Policy, portSpecs: PortSpecLookup): string[] {
  const wired = new Set(policy.edges.map((e) => e.from));
  const missing: string[] = [];
  for (const node of policy.nodes) {
    const outputs = resolveOutputs(node.type, portSpecs[node.type]);
    for (const port of outputs) {
      if (port.kind === 'error') continue;
      const key = `${node.id}.${port.name}`;
      if (!wired.has(key)) missing.push(key);
    }
  }
  return missing;
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
        ...(data.configRef ? { config_ref: data.configRef } : {}),
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
 * - While mounted, registers `add-plugin`/`save-graph` with the App-level
 *   command palette (see {@link useRegisterEditorAction}), so the palette's
 *   corresponding entries and shortcuts are live only when a canvas is open.
 *
 * @remarks
 * Success/error handles correspond to the port routing model executed by
 * CompiledGraph::execute in src/graph/engine.rs.
 */
export function GraphCanvas({
  policy,
  plugins,
  scripts,
  onSavePolicy,
  onSaveWarning,
  kind,
  supernodes,
  pluginConfigs,
  debugConfig,
  showPortNames,
  onOpenPalette,
}: GraphCanvasProps) {
  const [selectedNodeId, setSelectedNodeId] = useState<string | null>(null);
  const [selectedEdgeId, setSelectedEdgeId] = useState<string | null>(null);
  const [drawerOpen, setDrawerOpen] = useState(false);

  const handleSelect = useCallback((id: string) => {
    setSelectedNodeId(id);
    setDrawerOpen(false);
  }, []);

  // Supernode definitions can't contain endpoint nodes (spec §6): a
  // supernode has no listener to bind and its instance already stands in
  // for a client via the success/error ports. PluginDrawer filters
  // 'listener'/'script' internally for every mode; the 'client' exclusion
  // is supernode-only, so it's applied here rather than inside the drawer.
  const drawerPlugins = useMemo(
    () =>
      kind === 'supernode'
        ? plugins.filter((p) => p.type !== 'listener' && p.type !== 'client')
        : plugins,
    [plugins, kind]
  );

  // Catalog-derived port declarations, keyed by plugin type; threaded into
  // every node's data so PluginNode can render its declared handles, and
  // consulted for edge coloring and the unwired-port save warning below.
  const portSpecs = useMemo(() => buildPortSpecs(plugins), [plugins]);

  // The parent keys this component by policy name, so a different policy
  // remounts the canvas and nodes/edges/selection all start fresh from the
  // prop. Refetches of the same policy keep the local (unsaved) graph state.
  const initialNodes = useMemo(
    () => (policy ? policyToNodes(policy, handleSelect, portSpecs, showPortNames) : []),
    [policy, handleSelect, portSpecs, showPortNames]
  );
  const initialEdges = useMemo(
    () => (policy ? policyToEdges(policy, portSpecs) : []),
    [policy, portSpecs]
  );

  const [nodes, setNodes, onNodesChange] = useNodesState(initialNodes);
  const [edges, setEdges, onEdgesChange] = useEdgesState(initialEdges);

  // Node `data` is captured at conversion time, so flipping the preference
  // after the initial render needs an explicit rewrite of every existing node.
  useEffect(() => {
    setNodes((nds) => nds.map((n) => ({ ...n, data: { ...n.data, showPortNames } })));
  }, [showPortNames, setNodes]);

  const onConnect = useCallback(
    (connection: Connection) => {
      setEdges((eds) => {
        // Cardinality rules (connectionRules.ts): an occupied single-input
        // target rejects the edge; an occupied source port is rewired so a
        // port never fans out — the compiler would reject the save anyway.
        const targetNode = nodes.find((n) => n.id === connection.target);
        const targetType = (targetNode?.data as unknown as PluginNodeData)?.pluginType;
        const base = edgesAfterConnect(eds, connection, targetType);
        if (base === null) {
          return eds;
        }

        const sourceNode = nodes.find((n) => n.id === connection.source);
        const sourceType = (sourceNode?.data as unknown as PluginNodeData)?.pluginType;
        const kind = portKindFor(sourceType, connection.sourceHandle || 'success', portSpecs);
        const color = PORT_STROKE[kind];
        return addEdge(
          {
            ...connection,
            animated: kind === 'error',
            style: { stroke: color, strokeWidth: 2 },
            markerEnd: {
              type: MarkerType.ArrowClosed,
              color,
            },
          },
          base
        );
      });
    },
    [setEdges, nodes, portSpecs]
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

  // Predecessor lookup for the var-suggestion hook (NodeInspector): the
  // incoming edge feeding the selected node's `in` handle, preferring the
  // success-port edge over an error-port one when both exist. `undefined` =
  // no incoming edge yet (nothing to preview from); `null` = the predecessor
  // is the pipeline entry (listener/input), so preview from trace.initial
  // rather than a node step. See varSuggestions.ts's module doc comment for
  // the full encoding.
  const incoming = edges.filter((e) => e.target === selectedNodeId);
  const successEdge = incoming.find((e) => e.sourceHandle !== 'error') ?? incoming[0];
  const predecessorType =
    successEdge === undefined
      ? undefined
      : (nodes.find((n) => n.id === successEdge.source)?.data as PluginNodeData | undefined)
          ?.pluginType;
  const predecessorId =
    successEdge === undefined
      ? undefined
      : predecessorType === 'listener' || predecessorType === 'input'
        ? null
        : successEdge.source;

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
        ports: portSpecs[type],
        onSelect: handleSelect,
        showPortNames,
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
        ports: portSpecs['script'],
        onSelect: handleSelect,
        showPortNames,
      } satisfies PluginNodeData,
    };
    setNodes((nds) => [...nds, newNode]);
    setSelectedNodeId(id);
    setDrawerOpen(false);
  };

  const handleAddSupernode = (sn: Supernode) => {
    const id = `${sn.name}-${Date.now().toString(36)}`;
    const newNode: Node = {
      id,
      type: 'pluginNode',
      position: { x: 300, y: 200 + nodes.length * 80 },
      data: {
        label: `⬡ ${sn.name}`,
        pluginType: 'supernode',
        config: { name: sn.name },
        // 'supernode' has no catalog entry (it's not a src/plugins/mod.rs
        // type); portSpecs lookup misses and PluginNode falls back to the
        // default success+error pair, matching a supernode instance's fixed
        // output/error boundary exits (src/graph/expand.rs).
        ports: portSpecs['supernode'],
        onSelect: handleSelect,
        showPortNames,
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

  const handleUpdateConfigRef = (nodeId: string, ref: string | undefined) => {
    setNodes((nds) =>
      nds.map((n) => (n.id === nodeId ? { ...n, data: { ...n.data, configRef: ref } } : n))
    );
  };

  const handleDeleteNode = (nodeId: string) => {
    setNodes((nds) => nds.filter((n) => n.id !== nodeId));
    setEdges((eds) => eds.filter((e) => e.source !== nodeId && e.target !== nodeId));
    setSelectedNodeId(null);
  };

  const handleSave = useCallback(() => {
    if (!policy) return;
    const updated = nodesToPolicy(policy.name, nodes, edges, policy.error_handler);

    // Client-side heads-up only: the server is the authority (it rejects the
    // save outright with a "must be wired — add an edge from ..." message,
    // which the existing error toast already surfaces), so this warns without
    // blocking the attempt.
    const unwired = findUnwiredPorts(updated, portSpecs);
    if (unwired.length > 0) {
      onSaveWarning?.(
        'Unwired ports',
        `No outgoing edge for: ${unwired.join(', ')} — the save may be rejected.`
      );
    }

    console.log('Saving policy:', JSON.stringify(updated, null, 2));
    onSavePolicy(updated);
  }, [policy, nodes, edges, portSpecs, onSaveWarning, onSavePolicy]);

  // Exposes canvas-owned actions to the App-level command palette (see
  // editorActions.tsx) for as long as this canvas is mounted. These hook
  // calls must stay ABOVE the `if (!policy)` early return below, or hook
  // order would vary between the empty state and the editor — which means
  // registration alone does not imply a graph is open. The palette's
  // `when()` pairs it with `CommandContext.editorOpen` for that.
  useRegisterEditorAction(
    'add-plugin',
    useCallback(() => {
      setSelectedNodeId(null);
      setDrawerOpen(true);
    }, [])
  );
  useRegisterEditorAction('save-graph', handleSave);

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
            <button
              onClick={onOpenPalette}
              className="flex items-center justify-center transition-colors"
              style={{
                width: 28,
                height: 28,
                borderRadius: 'var(--radius-sm)',
                background: 'transparent',
                color: 'var(--text-secondary)',
              }}
              onMouseEnter={(e) => (e.currentTarget.style.background = 'var(--surface-hover)')}
              onMouseLeave={(e) => (e.currentTarget.style.background = 'transparent')}
              title="Command palette (Ctrl+K)"
              aria-label="Open command palette"
            >
              <Command size={15} />
            </button>
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
              {kind === 'supernode' ? 'Save Supernode' : 'Save Policy'}
            </button>
          </div>
        </Panel>
      </ReactFlow>

      <PluginDrawer
        plugins={drawerPlugins}
        scripts={scripts}
        supernodes={kind === 'policy' ? supernodes : []}
        onAddPlugin={handleAddPlugin}
        onAddScript={handleAddScript}
        onAddSupernode={handleAddSupernode}
        isOpen={drawerOpen}
        onClose={() => setDrawerOpen(false)}
      />

      {selectedNodeId && !drawerOpen && (
        <NodeInspector
          node={selectedNode}
          pluginConfigs={pluginConfigs}
          onUpdateConfig={handleUpdateConfig}
          onUpdateConfigRef={handleUpdateConfigRef}
          onDeleteNode={handleDeleteNode}
          onClose={() => setSelectedNodeId(null)}
          policyName={policy?.name ?? null}
          predecessorId={predecessorId}
          debugConfig={debugConfig}
          kind={kind}
        />
      )}
    </div>
  );
}
