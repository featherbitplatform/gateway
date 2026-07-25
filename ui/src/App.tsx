/**
 * Root component of the admin UI: owns all server-backed state and wires
 * the sidebar (route list) to the graph canvas (policy editor), talking to
 * the gateway Admin API through the shared api client.
 *
 * @module App
 */
import { useState, useEffect, useCallback } from 'react';
import { Sidebar } from './components/Sidebar';
import { GraphCanvas } from './components/GraphCanvas';
import { Dialog, DialogButton, DialogField } from './components/Dialog';
import { DebugPanel } from './components/DebugPanel';
import { Toast, type ToastData } from './components/Toast';
import { api } from './api/client';
import type { Route, Policy, PluginType, ScriptFile, DebugConfig } from './types';

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
  const [plugins, setPlugins] = useState<PluginType[]>([]);
  const [scripts, setScripts] = useState<ScriptFile[]>([]);
  const [selectedRoute, setSelectedRoute] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [toast, setToast] = useState<ToastData | null>(null);

  // Create-route dialog state
  const [createOpen, setCreateOpen] = useState(false);
  const [newName, setNewName] = useState('');
  const [newPath, setNewPath] = useState('/*');

  // Delete-route confirmation state
  const [deleteTarget, setDeleteTarget] = useState<string | null>(null);

  // View-YAML dialog state: null when closed, the exported YAML string when open.
  const [yamlView, setYamlView] = useState<string | null>(null);

  // Debug panel state. `debugConfig` is fetched even when debug is off, so the
  // panel can explain why it is unavailable rather than appearing broken.
  const [debugOpen, setDebugOpen] = useState(false);
  const [debugConfig, setDebugConfig] = useState<DebugConfig | null>(null);

  const loadData = useCallback(async () => {
    try {
      const [r, p, pl, sc] = await Promise.all([
        api.listRoutes(),
        api.listPolicies(),
        api.listPlugins(),
        api.listScripts(),
      ]);
      setRoutes(r);
      setPolicies(p);
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

  const handleCreateRoute = () => {
    setNewName('');
    setNewPath('/*');
    setCreateOpen(true);
  };

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

  const handleViewYaml = async () => {
    try {
      const yaml = await api.exportConfig();
      setYamlView(yaml);
    } catch (e) {
      setToast({ tone: 'error', title: 'Failed to export config', message: `${e}` });
    }
  };

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

  const handleReload = async () => {
    try {
      await api.reload();
      await loadData();
      setToast({ tone: 'success', title: 'Config reloaded' });
    } catch (e) {
      setToast({ tone: 'error', title: 'Reload failed', message: `${e}` });
    }
  };

  const handleSavePolicy = async (policy: Policy) => {
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
  };

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
        onSelectRoute={setSelectedRoute}
        onCreateRoute={handleCreateRoute}
        onDeleteRoute={(name) => setDeleteTarget(name)}
        onReload={handleReload}
        onViewYaml={handleViewYaml}
        onOpenDebug={() => setDebugOpen(true)}
        debugEnabled={debugConfig?.enabled ?? false}
      />
      {/* Keyed by policy name: switching policies remounts the canvas so
          nodes/edges/selection re-sync from the prop (see GraphCanvas docs). */}
      <GraphCanvas
        key={selectedPolicy?.name ?? ''}
        policy={selectedPolicy}
        plugins={plugins}
        scripts={scripts}
        onSavePolicy={handleSavePolicy}
      />

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
