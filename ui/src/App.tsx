/**
 * Root component of the admin UI: owns all server-backed state and wires
 * the sidebar (route list) to the graph canvas (policy editor), talking to
 * the gateway Admin API through the shared api client.
 *
 * @module App
 */
import { useState, useEffect, useCallback, useMemo } from 'react';
import { Sidebar } from './components/Sidebar';
import { GraphCanvas } from './components/GraphCanvas';
import { PluginConfigPanel } from './components/PluginConfigPanel';
import { StoresPanel } from './components/StoresPanel';
import { Dialog, DialogButton, DialogField } from './components/Dialog';
import { DebugPanel } from './components/DebugPanel';
import { Toast, type ToastData } from './components/Toast';
import { CommandPalette } from './components/CommandPalette';
import { buildCommands, matchesShortcut, type CommandContext } from './commands';
import { useEditorActions } from './editorActions';
import { usePortNames } from './usePortNames';
import { toggleTheme } from './theme';
import { api } from './api/client';
import { parseApiError } from './apiError';
import type {
  Route,
  Policy,
  Supernode,
  PluginConfigDef,
  PluginType,
  ScriptFile,
  DebugConfig,
  StoreConfig,
} from './types';

/**
 * Top-level application component and single owner of server state.
 *
 * On mount it loads routes, policies, plugin types, and script files in
 * parallel via the api client and keeps them in local state; every
 * mutation (create/delete route, save policy, reload config) round-trips
 * through the Admin API and then re-fetches everything, so the UI never
 * holds edits the gateway has not accepted.
 *
 * Data flow: route selection lives here as `selectedRoute`; the routes
 * list and selection callbacks go down to Sidebar, while the policy
 * resolved from the selected route (plus the plugin/script catalogs) goes
 * down to GraphCanvas, which hands edited policies back via
 * `onSavePolicy`. Dialog state for route creation/deletion and toast
 * notifications are also owned here. If the initial load fails, the whole
 * screen is replaced by a connection-error panel with a retry button.
 *
 * Creating a route also creates a matching `<name>-policy` seeded with a
 * `listener.out -> client.in` edge; deleting a route keeps its policy.
 *
 * @remarks
 * The Admin API served from src/admin/ persists these changes into the
 * gateway's shared state (src/state.rs) used by the data plane.
 */
export default function App() {
  const [routes, setRoutes] = useState<Route[]>([]);
  const [policies, setPolicies] = useState<Policy[]>([]);
  const [supernodes, setSupernodes] = useState<Supernode[]>([]);
  const [pluginConfigs, setPluginConfigs] = useState<PluginConfigDef[]>([]);
  const [stores, setStores] = useState<StoreConfig[]>([]);
  const [plugins, setPlugins] = useState<PluginType[]>([]);
  const [scripts, setScripts] = useState<ScriptFile[]>([]);
  const [selectedRoute, setSelectedRoute] = useState<string | null>(null);
  const [selectedSupernode, setSelectedSupernode] = useState<string | null>(null);
  const [selectedPluginConfig, setSelectedPluginConfig] = useState<string | null>(null);
  const [selectedStore, setSelectedStore] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [toast, setToast] = useState<ToastData | null>(null);

  // Create-route dialog state
  const [createOpen, setCreateOpen] = useState(false);
  const [newName, setNewName] = useState('');
  const [newPath, setNewPath] = useState('/*');

  // Delete-route confirmation state
  const [deleteTarget, setDeleteTarget] = useState<string | null>(null);

  // Create-supernode dialog state
  const [createSupernodeOpen, setCreateSupernodeOpen] = useState(false);
  const [newSupernodeName, setNewSupernodeName] = useState('');

  // Delete-supernode confirmation state
  const [deleteSupernodeTarget, setDeleteSupernodeTarget] = useState<string | null>(null);

  // Create-plugin-config dialog state
  const [createPluginConfigOpen, setCreatePluginConfigOpen] = useState(false);
  const [newPcName, setNewPcName] = useState('');
  const [newPcType, setNewPcType] = useState('');

  // Delete-plugin-config confirmation state
  const [deletePluginConfigTarget, setDeletePluginConfigTarget] = useState<string | null>(null);

  // Create-store dialog state
  const [createStoreOpen, setCreateStoreOpen] = useState(false);
  const [newStoreName, setNewStoreName] = useState('');
  const [newStoreType, setNewStoreType] = useState('redis');
  const [newStoreUrl, setNewStoreUrl] = useState('');

  // Delete-store confirmation state
  const [deleteStoreTarget, setDeleteStoreTarget] = useState<string | null>(null);

  // View-YAML dialog state: null when closed, the exported YAML string when open.
  const [yamlView, setYamlView] = useState<string | null>(null);

  // Debug panel state. `debugConfig` is fetched even when debug is off, so the
  // panel can explain why it is unavailable rather than appearing broken.
  const [debugOpen, setDebugOpen] = useState(false);
  const [debugConfig, setDebugConfig] = useState<DebugConfig | null>(null);

  // Port-name visibility (P) and the command palette (Ctrl+K). Owned here —
  // a single usePortNames() call — so the palette's toggle and the canvas
  // it re-renders can never see two different copies of the preference.
  const [showPortNames, togglePortNames] = usePortNames();
  const [paletteOpen, setPaletteOpen] = useState(false);
  const editorActions = useEditorActions();

  const loadData = useCallback(async () => {
    try {
      const [r, p, sn, pc, st, pl, sc] = await Promise.all([
        api.listRoutes(),
        api.listPolicies(),
        api.listSupernodes(),
        api.listPluginConfigs(),
        api.listStores(),
        api.listPlugins(),
        api.listScripts(),
      ]);
      setRoutes(r);
      setPolicies(p);
      setSupernodes(sn);
      setPluginConfigs(pc);
      setStores(st);
      setPlugins(pl);
      setScripts(sc);
      setError(null);
    } catch (e) {
      setError(`Failed to connect to gateway: ${e}`);
    }
    // Debug settings are advisory: a failure here must not block the editor,
    // so this is fetched separately from the required data above.
    try {
      setDebugConfig(await api.debugConfig());
    } catch {
      setDebugConfig(null);
    }
  }, []);

  // Initial fetch. Scheduled as a promise callback rather than called
  // directly: every setState in loadData already runs after an await, and
  // this keeps the effect body itself setState-free (react-hooks lint).
  useEffect(() => {
    Promise.resolve().then(loadData);
  }, [loadData]);

  const selectedPolicy = (() => {
    const route = routes.find((r) => r.name === selectedRoute);
    if (!route) return null;
    return policies.find((p) => p.name === route.policy) || null;
  })();

  const selectedSupernodeDef = supernodes.find((s) => s.name === selectedSupernode) || null;
  // A supernode is edited through the same canvas contract as a policy.
  const canvasPolicy: Policy | null = selectedSupernodeDef
    ? { name: selectedSupernodeDef.name, nodes: selectedSupernodeDef.nodes, edges: selectedSupernodeDef.edges }
    : selectedPolicy;

  const selectedPluginConfigDef = pluginConfigs.find((pc) => pc.name === selectedPluginConfig) || null;

  const selectedStoreDef = stores.find((s) => s.name === selectedStore) || null;

  // Declared stores as select options for `optionsFrom: 'stores'` fields
  // (SchemaForm's dynamicOptions), shared by NodeInspector and PluginConfigPanel.
  const storeOptions = useMemo(
    () => stores.map((s) => ({ value: s.name, label: `${s.name} (${s.type})` })),
    [stores]
  );

  // Shared configs only make sense for plugin nodes with real config, so the
  // create dialog's type picker excludes the boundary/no-config types —
  // mirrors the node palette's exclusions (supernode and other boundary
  // types are not in the catalog at all).
  // - listener/client: pipeline endpoints, not real plugin nodes — mirrors
  //   RESERVED_TYPES in src/config/resolve.rs (supernode/boundary types
  //   never appear in the catalog either).
  // - script: excluded because a script node's config is file-bound
  //   (runtime + source path), a poor fit for a shared, reusable profile.
  const pluginConfigTypeOptions = plugins.filter(
    (p) => p.type !== 'listener' && p.type !== 'client' && p.type !== 'script'
  );

  // Selection is mutually exclusive across the routes list, the supernodes
  // list, the plugin configs list, and the stores list: picking one clears
  // the other three so the main panel always reflects a single, unambiguous
  // selection.
  const handleSelectRoute = (name: string) => {
    setSelectedSupernode(null);
    setSelectedPluginConfig(null);
    setSelectedStore(null);
    setSelectedRoute(name);
  };

  const handleSelectSupernode = (name: string) => {
    setSelectedRoute(null);
    setSelectedPluginConfig(null);
    setSelectedStore(null);
    setSelectedSupernode(name);
  };

  const handleSelectPluginConfig = (name: string) => {
    setSelectedRoute(null);
    setSelectedSupernode(null);
    setSelectedStore(null);
    setSelectedPluginConfig(name);
  };

  const handleSelectStore = (name: string) => {
    setSelectedRoute(null);
    setSelectedSupernode(null);
    setSelectedPluginConfig(null);
    setSelectedStore(name);
  };

  // The create/view/reload handlers below are useCallback'd because they are
  // fields of the memoized `commandCtx`, which keys the global keydown effect
  // (an unstable field there would resubscribe the listener every render).
  const handleCreateRoute = useCallback(() => {
    setNewName('');
    setNewPath('/*');
    setCreateOpen(true);
  }, []);

  const submitCreateRoute = async () => {
    const name = newName.trim();
    const path = newPath.trim();
    if (!name || !path) return;
    setCreateOpen(false);

    const policyName = `${name}-policy`;
    try {
      // Create a default policy with listener and client
      await api.updatePolicy(policyName, {
        name: policyName,
        nodes: [
          { id: 'listener', type: 'listener', config: {} },
          { id: 'client', type: 'client', config: {} },
        ],
        edges: [
          { from: 'listener.out', to: 'client.in' },
        ],
      });
      await api.createRoute({
        name,
        match: { path, methods: ['GET', 'POST', 'PUT', 'DELETE'] },
        policy: policyName,
      });
      await loadData();
      setSelectedRoute(name);
      setToast({ tone: 'success', title: 'Route created', message: `${name} · ${path}` });
    } catch (e) {
      setToast({ tone: 'error', title: 'Failed to create route', message: `${e}` });
    }
  };

  const submitDeleteRoute = async () => {
    const name = deleteTarget;
    setDeleteTarget(null);
    if (!name) return;
    try {
      await api.deleteRoute(name);
      await loadData();
      if (selectedRoute === name) setSelectedRoute(null);
      setToast({ tone: 'success', title: 'Route deleted', message: name });
    } catch (e) {
      setToast({ tone: 'error', title: 'Failed to delete route', message: `${e}` });
    }
  };

  const handleCreateSupernode = useCallback(() => {
    setNewSupernodeName('');
    setCreateSupernodeOpen(true);
  }, []);

  const submitCreateSupernode = async () => {
    const name = newSupernodeName.trim();
    if (!name) return;
    setCreateSupernodeOpen(false);

    try {
      // Seed a minimal pass-through definition: input -> output directly,
      // with an unwired error boundary node. This validates and compiles
      // fine as-is (expansion supports the pass-through input.out ->
      // output.in form) — it is a deliberately minimal starting point, not
      // something to "fill in" further here.
      await api.updateSupernode(name, {
        name,
        nodes: [
          { id: 'input', type: 'input', config: {}, position: { x: 0, y: 150 } },
          { id: 'output', type: 'output', config: {}, position: { x: 500, y: 150 } },
          { id: 'error', type: 'error', config: {}, position: { x: 500, y: 330 } },
        ],
        edges: [{ from: 'input.out', to: 'output.in' }],
      });
      await loadData();
      handleSelectSupernode(name);
      setToast({ tone: 'success', title: 'Supernode created', message: name });
    } catch (e) {
      setToast({ tone: 'error', title: 'Failed to create supernode', message: `${e}` });
    }
  };

  // Creates a supernode definition extracted from the policy canvas (see
  // GraphCanvas's submitExtract); passed down as onCreateSupernodeDef. Unlike
  // submitCreateSupernode above, the definition already exists in full (the
  // extraction helper built it), so this is a straight persist-and-refresh.
  const handleCreateSupernodeDef = useCallback(async (sn: Supernode): Promise<boolean> => {
    try {
      await api.updateSupernode(sn.name, sn);
      await loadData();
      setToast({ tone: 'success', title: 'Supernode created', message: sn.name });
      return true;
    } catch (e) {
      setToast({ tone: 'error', title: 'Failed to create supernode', message: `${e}` });
      return false;
    }
  }, [loadData]);

  const submitDeleteSupernode = async () => {
    const name = deleteSupernodeTarget;
    setDeleteSupernodeTarget(null);
    if (!name) return;
    try {
      await api.deleteSupernode(name);
      await loadData();
      if (selectedSupernode === name) setSelectedSupernode(null);
      setToast({ tone: 'success', title: 'Supernode deleted', message: name });
    } catch (e) {
      setToast({ tone: 'error', title: 'Failed to delete supernode', message: `${e}` });
    }
  };

  const handleCreatePluginConfig = useCallback(() => {
    setNewPcName('');
    setNewPcType('');
    setCreatePluginConfigOpen(true);
  }, []);

  const submitCreatePluginConfig = async () => {
    const name = newPcName.trim();
    const type = newPcType;
    if (!name || !type) return;
    setCreatePluginConfigOpen(false);

    try {
      await api.updatePluginConfig(name, { name, type, config: {} });
      await loadData();
      handleSelectPluginConfig(name);
      setToast({ tone: 'success', title: 'Plugin config created', message: `${name} · ${type}` });
    } catch (e) {
      setToast({ tone: 'error', title: 'Failed to create plugin config', message: `${e}` });
    }
  };

  const submitDeletePluginConfig = async () => {
    const name = deletePluginConfigTarget;
    setDeletePluginConfigTarget(null);
    if (!name) return;
    try {
      await api.deletePluginConfig(name);
      await loadData();
      if (selectedPluginConfig === name) setSelectedPluginConfig(null);
      setToast({ tone: 'success', title: 'Plugin config deleted', message: name });
    } catch (e) {
      setToast({ tone: 'error', title: 'Failed to delete plugin config', message: `${e}` });
    }
  };

  const handleCreateStore = useCallback(() => {
    setNewStoreName('');
    setNewStoreType('redis');
    setNewStoreUrl('');
    setCreateStoreOpen(true);
  }, []);

  const submitCreateStore = async () => {
    const name = newStoreName.trim();
    const url = newStoreUrl.trim();
    if (!name || !url) return;
    setCreateStoreOpen(false);

    try {
      await api.createStore({
        name,
        type: newStoreType,
        url,
        key_prefix: 'fb',
        connect_timeout_ms: 2000,
      });
      await loadData();
      handleSelectStore(name);
      setToast({ tone: 'success', title: 'Store created', message: `${name} · ${newStoreType}` });
    } catch (e) {
      setToast({ tone: 'error', title: 'Failed to create store', message: `${e}` });
    }
  };

  const submitDeleteStore = async () => {
    const name = deleteStoreTarget;
    setDeleteStoreTarget(null);
    if (!name) return;
    try {
      await api.deleteStore(name);
      await loadData();
      if (selectedStore === name) setSelectedStore(null);
      setToast({ tone: 'success', title: 'Store deleted', message: name });
    } catch (e) {
      const parsed = parseApiError(e);
      setToast({
        tone: 'error',
        title: 'Failed to delete store',
        message:
          parsed.error === 'in_use'
            ? `Referenced by: ${parsed.referrers.join(', ')}`
            : `${e}`,
      });
    }
  };

  const handleSaveStore = async (store: StoreConfig) => {
    try {
      await api.updateStore(store.name, store);
      await loadData();
      setToast({ tone: 'success', title: 'Store saved', message: store.name });
    } catch (e) {
      setToast({ tone: 'error', title: 'Failed to save store', message: `${e}` });
    }
  };

  /**
   * Persists a shared config extracted from a policy node (the inspector's
   * "Save as shared config" flow). Returns whether the save succeeded so the
   * inspector only re-links the node to the new config on success.
   */
  const handleExtractPluginConfig = async (def: PluginConfigDef): Promise<boolean> => {
    try {
      await api.updatePluginConfig(def.name, def);
      await loadData();
      setToast({ tone: 'success', title: 'Shared config saved', message: `${def.name} · ${def.type}` });
      return true;
    } catch (e) {
      setToast({ tone: 'error', title: 'Failed to save shared config', message: `${e}` });
      return false;
    }
  };

  const handleSavePluginConfig = async (def: PluginConfigDef) => {
    try {
      await api.updatePluginConfig(def.name, def);
      await loadData();
      setToast({ tone: 'success', title: 'Plugin config saved', message: def.name });
    } catch (e) {
      setToast({ tone: 'error', title: 'Failed to save plugin config', message: `${e}` });
    }
  };

  const handleViewYaml = useCallback(async () => {
    try {
      const yaml = await api.exportConfig();
      setYamlView(yaml);
    } catch (e) {
      setToast({ tone: 'error', title: 'Failed to export config', message: `${e}` });
    }
  }, []);

  const copyYaml = async () => {
    if (yamlView == null) return;
    try {
      await navigator.clipboard.writeText(yamlView);
      setToast({ tone: 'success', title: 'Copied to clipboard' });
    } catch (e) {
      setToast({ tone: 'error', title: 'Copy failed', message: `${e}` });
    }
  };

  const downloadYaml = () => {
    if (yamlView == null) return;
    const blob = new Blob([yamlView], { type: 'text/yaml' });
    const url = URL.createObjectURL(blob);
    const a = document.createElement('a');
    a.href = url;
    a.download = 'gateway.yaml';
    a.click();
    URL.revokeObjectURL(url);
  };

  const handleReload = useCallback(async () => {
    try {
      await api.reload();
      await loadData();
      setToast({ tone: 'success', title: 'Config reloaded' });
    } catch (e) {
      setToast({ tone: 'error', title: 'Reload failed', message: `${e}` });
    }
  }, [loadData]);

  // Wrapped in useCallback (rather than a plain function, as most handlers
  // in this file are) because it's registered as the canvas's `save-graph`
  // editor action (see GraphCanvas's `useRegisterEditorAction('save-graph',
  // handleSave)`, where `handleSave` closes over `onSavePolicy` — this
  // function). An unstable identity here would flow through and destabilize
  // `handleSave` too, churning that registration on every unrelated App
  // re-render. Deps are exactly the free variables read below; `loadData`
  // and `setToast` are already stable (see their own definitions).
  const handleSavePolicy = useCallback(
    async (policy: Policy) => {
      try {
        await api.updatePolicy(policy.name, policy);
        await loadData();
        setToast({
          tone: 'success',
          title: 'Policy saved',
          message: `${policy.name} · ${policy.nodes.length} nodes persisted`,
        });
      } catch (e) {
        setToast({ tone: 'error', title: 'Failed to save policy', message: `${e}` });
      }
    },
    [loadData]
  );

  // Same stability requirement as handleSavePolicy above — this is the
  // function actually passed as GraphCanvas's `onSavePolicy`.
  // `selectedSupernodeDef` is a `.find()` result over `supernodes`, so its
  // identity only changes when the underlying list or selection changes,
  // not on every render.
  const handleSaveGraph = useCallback(
    async (graph: Policy) => {
      if (selectedSupernodeDef) {
        try {
          await api.updateSupernode(graph.name, {
            name: graph.name,
            description: selectedSupernodeDef.description,
            nodes: graph.nodes,
            edges: graph.edges,
          });
          await loadData();
          setToast({ tone: 'success', title: 'Supernode saved', message: graph.name });
        } catch (e) {
          setToast({ tone: 'error', title: 'Failed to save supernode', message: `${e}` });
        }
        return;
      }
      await handleSavePolicy(graph);
    },
    [selectedSupernodeDef, loadData, handleSavePolicy]
  );

  // Hoisted out of the GraphCanvas JSX (where an inline arrow would be a
  // fresh function every render) for the same reason: it's a dependency of
  // GraphCanvas's `handleSave`, which is registered as an editor action.
  // `setToast` is a stable setState setter, so this has no real deps.
  const handleSaveWarning = useCallback((title: string, message: string) => {
    setToast({ tone: 'warning', title, message });
  }, []);

  // Selection across routes/supernodes/plugin configs/stores is mutually
  // exclusive (see handleSelect* above), so any one of them being set means
  // "something is selected" for the view-yaml command's `when`.
  const hasSelection =
    selectedRoute !== null ||
    selectedSupernode !== null ||
    selectedPluginConfig !== null ||
    selectedStore !== null;

  // Memoized: this object is the only non-primitive dependency of the global
  // keydown effect below, so a fresh literal every render would tear down and
  // re-add the window listener on every render. It is also CommandPalette's
  // `ctx` prop, and the palette memoizes its filtered list on it — that memo
  // only ever hits because this identity is stable.
  const editorOpen = canvasPolicy !== null;
  const commandCtx: CommandContext = useMemo(
    () => ({
      editorOpen,
      hasSelection,
      togglePortNames,
      createRoute: handleCreateRoute,
      createSupernode: handleCreateSupernode,
      createPluginConfig: handleCreatePluginConfig,
      viewYaml: handleViewYaml,
      reloadConfig: handleReload,
      toggleTheme,
      // Bridged to whatever GraphCanvas has registered (see editorActions.tsx).
      // Registration alone is not "a graph is open" — GraphCanvas registers
      // even when mounted with `policy={null}` — so the canvas commands' when()
      // pairs `hasEditorAction` with `editorOpen` (see commands.ts).
      invokeEditorAction: editorActions.invoke,
      hasEditorAction: editorActions.has,
    }),
    [
      editorOpen,
      hasSelection,
      togglePortNames,
      handleCreateRoute,
      handleCreateSupernode,
      handleCreatePluginConfig,
      handleViewYaml,
      handleReload,
      editorActions,
    ]
  );

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if ((e.ctrlKey || e.metaKey) && e.key.toLowerCase() === 'k') {
        e.preventDefault();
        setPaletteOpen((v) => !v);
        return;
      }
      if (paletteOpen) {
        // The palette owns keys while it is open — but its own Escape binding
        // lives on the search input, and a click on the list padding or the
        // "No matching command" row blurs focus to <body>. Handling Escape
        // here keeps "Escape closes it" true regardless of where focus is,
        // without touching the input's autoFocus.
        if (e.key === 'Escape') {
          e.preventDefault();
          setPaletteOpen(false);
        }
        return;
      }

      const commands = buildCommands();

      // Modifier shortcuts run ABOVE the text-field guard. Ctrl+S must never
      // reach the browser's Save Page dialog — not from the inspector's
      // raw-config textarea, and not when `save-graph` happens to be
      // unavailable either. So preventDefault() fires for any registered
      // Ctrl+* binding; only run() is gated on when().
      for (const cmd of commands) {
        if (!cmd.shortcut?.startsWith('Ctrl+') || !matchesShortcut(e, cmd.shortcut)) continue;
        e.preventDefault();
        if (cmd.when && !cmd.when(commandCtx)) return;
        cmd.run(commandCtx);
        return;
      }

      // Bare single letters are typing, not commands, wherever text is being
      // entered. SELECT counts: a native <select> uses letters for type-ahead,
      // and preventDefault() here would kill it (see NodeInspector's shared-
      // config picker and SchemaForm's enum fields).
      const t = e.target as HTMLElement | null;
      if (
        t &&
        (t.tagName === 'INPUT' ||
          t.tagName === 'TEXTAREA' ||
          t.tagName === 'SELECT' ||
          t.isContentEditable)
      )
        return;
      // Nor are they commands behind a modal dialog: Dialog has no focus trap,
      // so clicking its body blurs the autofocused field and any bare letter
      // would stack a second dialog at the same z-index. (The palette itself
      // never reaches here — it returns above.)
      if (document.querySelector('[role="dialog"]')) return;

      for (const cmd of commands) {
        if (!cmd.shortcut || cmd.shortcut.startsWith('Ctrl+')) continue;
        if (!matchesShortcut(e, cmd.shortcut)) continue;
        if (cmd.when && !cmd.when(commandCtx)) continue;
        e.preventDefault();
        cmd.run(commandCtx);
        return;
      }
    };
    window.addEventListener('keydown', onKey);
    return () => window.removeEventListener('keydown', onKey);
  }, [paletteOpen, commandCtx]);

  if (error) {
    return (
      <div className="h-screen flex items-center justify-center" style={{ background: 'var(--bg-app)' }}>
        <div
          className="text-center"
          style={{
            padding: 32,
            borderRadius: 'var(--radius-md)',
            background: 'var(--surface)',
            border: '1px solid var(--border)',
            boxShadow: 'var(--shadow-md)',
            maxWidth: 420,
          }}
        >
          <h2
            style={{
              fontSize: 'var(--text-lg)',
              fontWeight: 600,
              color: 'var(--error)',
              margin: '0 0 8px',
            }}
          >
            Connection Error
          </h2>
          <p
            style={{
              fontFamily: 'var(--font-mono)',
              fontSize: 'var(--text-sm)',
              color: 'var(--text-secondary)',
              margin: '0 0 16px',
              overflowWrap: 'anywhere',
            }}
          >
            {error}
          </p>
          <button
            onClick={loadData}
            style={{
              padding: '7px 18px',
              borderRadius: 'var(--radius-sm)',
              fontSize: 'var(--text-sm)',
              fontWeight: 500,
              background: 'var(--accent)',
              color: 'var(--text-on-accent)',
            }}
          >
            Retry
          </button>
        </div>
      </div>
    );
  }

  return (
    <div className="h-screen flex" style={{ background: 'var(--bg-app)' }}>
      <Sidebar
        routes={routes}
        selectedRoute={selectedRoute}
        onSelectRoute={handleSelectRoute}
        onCreateRoute={handleCreateRoute}
        onDeleteRoute={(name) => setDeleteTarget(name)}
        supernodes={supernodes}
        selectedSupernode={selectedSupernode}
        onSelectSupernode={handleSelectSupernode}
        onCreateSupernode={handleCreateSupernode}
        onDeleteSupernode={(name) => setDeleteSupernodeTarget(name)}
        pluginConfigs={pluginConfigs}
        selectedPluginConfig={selectedPluginConfig}
        onSelectPluginConfig={handleSelectPluginConfig}
        onCreatePluginConfig={handleCreatePluginConfig}
        onDeletePluginConfig={(name) => setDeletePluginConfigTarget(name)}
        stores={stores}
        selectedStore={selectedStore}
        onSelectStore={handleSelectStore}
        onCreateStore={handleCreateStore}
        onDeleteStore={(name) => setDeleteStoreTarget(name)}
        onReload={handleReload}
        onViewYaml={handleViewYaml}
        onOpenDebug={() => setDebugOpen(true)}
        debugEnabled={debugConfig?.enabled ?? false}
      />
      {selectedStoreDef ? (
        <StoresPanel
          key={selectedStoreDef.name}
          def={selectedStoreDef}
          onSave={handleSaveStore}
          onError={(title, message) => setToast({ tone: 'error', title, message })}
        />
      ) : selectedPluginConfigDef ? (
        <PluginConfigPanel
          key={selectedPluginConfigDef.name}
          def={selectedPluginConfigDef}
          onSave={handleSavePluginConfig}
          storeOptions={storeOptions}
        />
      ) : (
        // Keyed by policy/supernode name: switching the selection remounts
        // the canvas so nodes/edges/selection re-sync from the prop (see
        // GraphCanvas docs).
        <GraphCanvas
          key={canvasPolicy?.name ?? ''}
          policy={canvasPolicy}
          plugins={plugins}
          scripts={scripts}
          onSavePolicy={handleSaveGraph}
          onSaveWarning={handleSaveWarning}
          kind={selectedSupernodeDef ? 'supernode' : 'policy'}
          supernodes={supernodes}
          pluginConfigs={pluginConfigs}
          onExtractPluginConfig={handleExtractPluginConfig}
          debugConfig={debugConfig}
          showPortNames={showPortNames}
          onOpenPalette={() => setPaletteOpen(true)}
          onCreateSupernodeDef={handleCreateSupernodeDef}
          storeOptions={storeOptions}
        />
      )}

      <CommandPalette open={paletteOpen} onClose={() => setPaletteOpen(false)} ctx={commandCtx} />

      <Dialog
        open={createOpen}
        title="New route"
        onClose={() => setCreateOpen(false)}
        footer={
          <>
            <DialogButton variant="ghost" onClick={() => setCreateOpen(false)}>
              Cancel
            </DialogButton>
            <DialogButton onClick={submitCreateRoute}>Create route</DialogButton>
          </>
        }
      >
        <DialogField label="Route name" value={newName} onChange={setNewName} placeholder="echo-api" autoFocus />
        <DialogField label="Match path" value={newPath} onChange={setNewPath} placeholder="/api/*" mono />
      </Dialog>

      <Dialog
        open={deleteTarget !== null}
        title="Delete route"
        onClose={() => setDeleteTarget(null)}
        footer={
          <>
            <DialogButton variant="ghost" onClick={() => setDeleteTarget(null)}>
              Cancel
            </DialogButton>
            <DialogButton variant="danger" onClick={submitDeleteRoute}>
              Delete
            </DialogButton>
          </>
        }
      >
        <p style={{ fontSize: 'var(--text-sm)', color: 'var(--text-secondary)', margin: 0 }}>
          Delete route <code style={{ color: 'var(--text-primary)' }}>{deleteTarget}</code>? Its
          policy is kept and can be reattached.
        </p>
      </Dialog>

      <Dialog
        open={createSupernodeOpen}
        title="New supernode"
        onClose={() => setCreateSupernodeOpen(false)}
        footer={
          <>
            <DialogButton variant="ghost" onClick={() => setCreateSupernodeOpen(false)}>
              Cancel
            </DialogButton>
            <DialogButton onClick={submitCreateSupernode}>Create supernode</DialogButton>
          </>
        }
      >
        <DialogField
          label="Supernode name"
          value={newSupernodeName}
          onChange={setNewSupernodeName}
          placeholder="rate-limit-bundle"
          autoFocus
        />
      </Dialog>

      <Dialog
        open={deleteSupernodeTarget !== null}
        title="Delete supernode"
        onClose={() => setDeleteSupernodeTarget(null)}
        footer={
          <>
            <DialogButton variant="ghost" onClick={() => setDeleteSupernodeTarget(null)}>
              Cancel
            </DialogButton>
            <DialogButton variant="danger" onClick={submitDeleteSupernode}>
              Delete
            </DialogButton>
          </>
        }
      >
        <p style={{ fontSize: 'var(--text-sm)', color: 'var(--text-secondary)', margin: 0 }}>
          Delete supernode <code style={{ color: 'var(--text-primary)' }}>{deleteSupernodeTarget}</code>?
          Deletion fails while any policy still references it.
        </p>
      </Dialog>

      <Dialog
        open={createPluginConfigOpen}
        title="New plugin config"
        onClose={() => setCreatePluginConfigOpen(false)}
        footer={
          <>
            <DialogButton variant="ghost" onClick={() => setCreatePluginConfigOpen(false)}>
              Cancel
            </DialogButton>
            <DialogButton
              onClick={submitCreatePluginConfig}
              disabled={!newPcName.trim() || !newPcType}
            >
              Create plugin config
            </DialogButton>
          </>
        }
      >
        <DialogField
          label="Config name"
          value={newPcName}
          onChange={setNewPcName}
          placeholder="shared-rate-limit"
          autoFocus
        />
        <div style={{ marginBottom: 12 }}>
          <label
            style={{
              display: 'block',
              fontSize: 'var(--text-xs)',
              fontWeight: 500,
              color: 'var(--text-secondary)',
              marginBottom: 4,
            }}
          >
            Plugin type
          </label>
          <select
            value={newPcType}
            onChange={(e) => setNewPcType(e.target.value)}
            className="w-full"
            style={{
              padding: '7px 10px',
              borderRadius: 'var(--radius-sm)',
              fontSize: 'var(--text-sm)',
              background: 'var(--surface-input)',
              color: 'var(--text-primary)',
              border: '1px solid var(--border)',
            }}
          >
            <option value="" disabled>
              Select a plugin type&hellip;
            </option>
            {pluginConfigTypeOptions.map((p) => (
              <option key={p.type} value={p.type}>
                {p.type}
              </option>
            ))}
          </select>
        </div>
      </Dialog>

      <Dialog
        open={deletePluginConfigTarget !== null}
        title="Delete plugin config"
        onClose={() => setDeletePluginConfigTarget(null)}
        footer={
          <>
            <DialogButton variant="ghost" onClick={() => setDeletePluginConfigTarget(null)}>
              Cancel
            </DialogButton>
            <DialogButton variant="danger" onClick={submitDeletePluginConfig}>
              Delete
            </DialogButton>
          </>
        }
      >
        <p style={{ fontSize: 'var(--text-sm)', color: 'var(--text-secondary)', margin: 0 }}>
          Delete plugin config{' '}
          <code style={{ color: 'var(--text-primary)' }}>{deletePluginConfigTarget}</code>? Deletion
          fails while any node still references it.
        </p>
      </Dialog>

      <Dialog
        open={createStoreOpen}
        title="New store"
        onClose={() => setCreateStoreOpen(false)}
        footer={
          <>
            <DialogButton variant="ghost" onClick={() => setCreateStoreOpen(false)}>
              Cancel
            </DialogButton>
            <DialogButton onClick={submitCreateStore} disabled={!newStoreName.trim() || !newStoreUrl.trim()}>
              Create store
            </DialogButton>
          </>
        }
      >
        <DialogField
          label="Store name"
          value={newStoreName}
          onChange={setNewStoreName}
          placeholder="sessions-redis"
          autoFocus
        />
        <div style={{ marginBottom: 12 }}>
          <label
            style={{
              display: 'block',
              fontSize: 'var(--text-xs)',
              fontWeight: 500,
              color: 'var(--text-secondary)',
              marginBottom: 4,
            }}
          >
            Type
          </label>
          <select
            value={newStoreType}
            onChange={(e) => setNewStoreType(e.target.value)}
            className="w-full"
            style={{
              padding: '7px 10px',
              borderRadius: 'var(--radius-sm)',
              fontSize: 'var(--text-sm)',
              background: 'var(--surface-input)',
              color: 'var(--text-primary)',
              border: '1px solid var(--border)',
            }}
          >
            <option value="redis">redis</option>
            <option value="valkey">valkey</option>
          </select>
        </div>
        <DialogField
          label="URL"
          value={newStoreUrl}
          onChange={setNewStoreUrl}
          placeholder="redis://127.0.0.1:6379"
          mono
        />
      </Dialog>

      <Dialog
        open={deleteStoreTarget !== null}
        title="Delete store"
        onClose={() => setDeleteStoreTarget(null)}
        footer={
          <>
            <DialogButton variant="ghost" onClick={() => setDeleteStoreTarget(null)}>
              Cancel
            </DialogButton>
            <DialogButton variant="danger" onClick={submitDeleteStore}>
              Delete
            </DialogButton>
          </>
        }
      >
        <p style={{ fontSize: 'var(--text-sm)', color: 'var(--text-secondary)', margin: 0 }}>
          Delete store <code style={{ color: 'var(--text-primary)' }}>{deleteStoreTarget}</code>?
          Deletion is blocked while any plugin config references it.
        </p>
      </Dialog>

      <Dialog
        open={yamlView !== null}
        title="Gateway configuration (YAML)"
        width={720}
        onClose={() => setYamlView(null)}
        footer={
          <>
            <DialogButton variant="ghost" onClick={() => setYamlView(null)}>
              Close
            </DialogButton>
            <DialogButton variant="ghost" onClick={downloadYaml}>
              Download
            </DialogButton>
            <DialogButton onClick={copyYaml}>Copy</DialogButton>
          </>
        }
      >
        <p style={{ fontSize: 'var(--text-xs)', color: 'var(--text-muted)', margin: '0 0 10px' }}>
          The live in-memory config the gateway is running — routes and policies together.
          Values keep their <code>{'${ENV_VAR}'}</code> templates; env vars resolve when a policy
          is compiled, not here.
        </p>
        <pre
          style={{
            margin: 0,
            padding: 12,
            maxHeight: '60vh',
            overflow: 'auto',
            borderRadius: 'var(--radius-sm)',
            background: 'var(--surface-input)',
            border: '1px solid var(--border)',
            fontFamily: 'var(--font-mono)',
            fontSize: 'var(--text-xs)',
            lineHeight: 1.5,
            color: 'var(--text-primary)',
            whiteSpace: 'pre',
          }}
        >
          {yamlView}
        </pre>
      </Dialog>

      <DebugPanel
        open={debugOpen}
        onClose={() => setDebugOpen(false)}
        config={debugConfig}
        policies={policies}
        selectedPolicy={selectedPolicy?.name ?? null}
        onError={(title, message) => setToast({ tone: 'error', title, message })}
      />

      <Toast toast={toast} onDismiss={() => setToast(null)} />
    </div>
  );
}
