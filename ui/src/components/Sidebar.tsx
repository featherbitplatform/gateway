/**
 * Left navigation rail of the admin UI: featherbit branding with live gateway
 * status, the selectable route list, and the create-route / delete-route /
 * reload-config actions that back the gateway's admin API.
 *
 * @module components/Sidebar
 */
import { useEffect, useState } from 'react';
import { Plus, RotateCw, X, FileCode, Bug, KeyRound, ShieldCheck, Bell } from 'lucide-react';
import type { Route, Supernode, PluginConfigDef, StoreConfig, GatewayStatus } from '../types';
import { api } from '../api/client';

/** Props for Sidebar. Route data and mutations are owned by the parent (App). */
interface SidebarProps {
  /** Routes to list, as fetched from the admin API's GET /api/routes. */
  routes: Route[];
  /** Name of the currently selected route, or null when none is selected. */
  selectedRoute: string | null;
  /** Called with a route's name when its row is clicked. */
  onSelectRoute: (name: string) => void;
  /** Called when the "New" button is clicked; the parent opens the create-route dialog. */
  onCreateRoute: () => void;
  /** Called with the route's name when its hover-revealed delete button is clicked. */
  onDeleteRoute: (name: string) => void;
  /** Supernode library to list, as fetched from the admin API's GET /api/supernodes. */
  supernodes: Supernode[];
  /** Name of the currently selected supernode, or null when none is selected. */
  selectedSupernode: string | null;
  /** Called with a supernode's name when its row is clicked. */
  onSelectSupernode: (name: string) => void;
  /** Called when the supernodes "New" button is clicked; the parent opens the create-supernode dialog. */
  onCreateSupernode: () => void;
  /** Called with the supernode's name when its hover-revealed delete button is clicked. */
  onDeleteSupernode: (name: string) => void;
  /** Shared plugin config library to list, as fetched from the admin API's GET /api/plugin-configs. */
  pluginConfigs: PluginConfigDef[];
  /** Name of the currently selected plugin config, or null when none is selected. */
  selectedPluginConfig: string | null;
  /** Called with a plugin config's name when its row is clicked. */
  onSelectPluginConfig: (name: string) => void;
  /** Called when the plugin configs "New" button is clicked; the parent opens the create-plugin-config dialog. */
  onCreatePluginConfig: () => void;
  /** Called with the plugin config's name when its hover-revealed delete button is clicked. */
  onDeletePluginConfig: (name: string) => void;
  /**
   * Declared stores to list, as fetched from the admin API's GET /api/stores.
   * Also gates the Sessions footer button's dimmed/tooltip state (empty =
   * no store to list sessions from).
   */
  stores: StoreConfig[];
  /** Name of the currently selected store, or null when none is selected. */
  selectedStore: string | null;
  /** Called with a store's name when its row is clicked. */
  onSelectStore: (name: string) => void;
  /** Called when the stores "New" button is clicked; the parent opens the create-store dialog. */
  onCreateStore: () => void;
  /** Called with the store's name when its hover-revealed delete button is clicked. */
  onDeleteStore: (name: string) => void;
  /** Called when "Reload Config" is clicked; the parent triggers POST /api/config/reload. */
  onReload: () => void;
  /** Called when "View YAML" is clicked; the parent fetches GET /api/config/export and shows it. */
  onViewYaml: () => void;
  /** Called when "Debug" is clicked; the parent opens the trace/sandbox panel. */
  onOpenDebug: () => void;
  /** Whether debug mode is on. When false the Debug button is disabled with an explanatory tooltip. */
  debugEnabled: boolean;
  /** Called when "Sessions" is clicked; the parent opens the sessions panel. */
  onOpenSessions: () => void;
  /** Called when "Certificates" is clicked; the parent opens the certificates panel. */
  onOpenCertificates: () => void;
  /** Called when the bell is clicked; the parent opens the notifications panel. */
  onOpenNotifications: () => void;
  /** Error notifications raised since the panel was last opened; shown as the bell's badge when > 0. */
  unreadNotifications: number;
}

/**
 * Fixed-width sidebar with three sections: a header showing the gateway
 * version and route count, a scrollable route list (name plus match path,
 * with per-row delete on hover), and a footer "Reload Config" button.
 *
 * Fetches GET /api/status on mount and refetches whenever `routes` changes,
 * so the header count stays in sync after create/delete; status fetch
 * failures are silently ignored.
 *
 * @remarks Status and reload requests go through the api client
 * (ui/src/api/client.ts), which talks to the gateway's axum admin API.
 */
export function Sidebar({
  routes,
  selectedRoute,
  onSelectRoute,
  onCreateRoute,
  onDeleteRoute,
  supernodes,
  selectedSupernode,
  onSelectSupernode,
  onCreateSupernode,
  onDeleteSupernode,
  pluginConfigs,
  selectedPluginConfig,
  onSelectPluginConfig,
  onCreatePluginConfig,
  onDeletePluginConfig,
  stores,
  selectedStore,
  onSelectStore,
  onCreateStore,
  onDeleteStore,
  onReload,
  onViewYaml,
  onOpenDebug,
  debugEnabled,
  onOpenSessions,
  onOpenCertificates,
  onOpenNotifications,
  unreadNotifications,
}: SidebarProps) {
  const [status, setStatus] = useState<GatewayStatus | null>(null);

  useEffect(() => {
    api.status().then(setStatus).catch(() => {});
  }, [routes]);

  return (
    <div
      className="h-full flex flex-col shrink-0"
      style={{
        width: 'var(--rail-sidebar)',
        background: 'var(--surface)',
        borderRight: '1px solid var(--border)',
      }}
    >
      {/* Header */}
      <div
        className="flex items-center gap-2.5"
        style={{
          height: 'var(--topbar-h)',
          padding: '0 16px',
          borderBottom: '1px solid var(--border)',
        }}
      >
        <img
          src="/featherbit-mark.png"
          alt=""
          style={{ height: 26, width: 'auto', filter: 'drop-shadow(var(--glow-violet))' }}
        />
        <div className="min-w-0">
          <h1
            style={{
              fontSize: 'var(--text-md)',
              fontWeight: 'var(--weight-semibold)' as never,
              letterSpacing: 'var(--tracking-tight)',
              color: 'var(--text-primary)',
              margin: 0,
              lineHeight: 1.2,
            }}
          >
            featherbit
          </h1>
          {status && (
            <p
              style={{
                fontFamily: 'var(--font-mono)',
                fontSize: 'var(--text-2xs)',
                color: 'var(--text-muted)',
                margin: 0,
              }}
            >
              v{status.version} &middot; {status.routes} {status.routes === 1 ? 'route' : 'routes'}
            </p>
          )}
        </div>
      </div>

      {/* Routes */}
      <div className="flex-1 overflow-y-auto">
        <div className="p-3 flex items-center justify-between">
          <span className="eyebrow">Routes</span>
          <button
            onClick={onCreateRoute}
            aria-label="New route"
            className="flex items-center gap-1 transition-colors"
            style={{
              fontSize: 'var(--text-xs)',
              fontWeight: 'var(--weight-medium)' as never,
              padding: '3px 8px',
              borderRadius: 'var(--radius-sm)',
              background: 'var(--accent)',
              color: 'var(--text-on-accent)',
            }}
            onMouseEnter={(e) => (e.currentTarget.style.background = 'var(--accent-hover)')}
            onMouseLeave={(e) => (e.currentTarget.style.background = 'var(--accent)')}
          >
            <Plus size={12} />
            New
          </button>
        </div>
        {routes.map((route) => {
          const isSelected = selectedRoute === route.name;
          return (
            <div
              key={route.name}
              onClick={() => onSelectRoute(route.name)}
              className="mx-2 mb-1 cursor-pointer flex items-center justify-between group"
              style={{
                padding: '8px 10px',
                borderRadius: 'var(--radius-sm)',
                background: isSelected ? 'var(--surface-active)' : 'transparent',
                boxShadow: isSelected ? 'inset 0 0 0 1px var(--accent-ring)' : 'none',
                transition: 'background var(--dur-fast) var(--ease-out)',
              }}
              onMouseEnter={(e) => {
                if (!isSelected) e.currentTarget.style.background = 'var(--surface-hover)';
              }}
              onMouseLeave={(e) => {
                if (!isSelected) e.currentTarget.style.background = 'transparent';
              }}
            >
              <div className="flex flex-col min-w-0">
                <span
                  className="truncate"
                  style={{
                    fontSize: 'var(--text-sm)',
                    fontWeight: 'var(--weight-medium)' as never,
                    color: 'var(--text-primary)',
                  }}
                >
                  {route.name}
                </span>
                <span
                  className="truncate"
                  style={{
                    fontFamily: 'var(--font-mono)',
                    fontSize: 'var(--text-xs)',
                    color: 'var(--text-muted)',
                  }}
                >
                  {route.match?.path || '/'}
                </span>
              </div>
              <button
                onClick={(e) => {
                  e.stopPropagation();
                  onDeleteRoute(route.name);
                }}
                className="opacity-0 group-hover:opacity-100 flex items-center justify-center rounded transition-all"
                style={{ width: 22, height: 22, color: 'var(--error)' }}
                aria-label={`Delete route ${route.name}`}
              >
                <X size={13} />
              </button>
            </div>
          );
        })}
      </div>

      {/* Supernodes */}
      <div className="shrink-0" style={{ borderTop: '1px solid var(--border)', maxHeight: 260, overflowY: 'auto' }}>
        <div className="p-3 flex items-center justify-between">
          <span className="eyebrow">Supernodes</span>
          <button
            onClick={onCreateSupernode}
            aria-label="New supernode"
            className="flex items-center gap-1 transition-colors"
            style={{
              fontSize: 'var(--text-xs)',
              fontWeight: 'var(--weight-medium)' as never,
              padding: '3px 8px',
              borderRadius: 'var(--radius-sm)',
              background: 'var(--accent)',
              color: 'var(--text-on-accent)',
            }}
            onMouseEnter={(e) => (e.currentTarget.style.background = 'var(--accent-hover)')}
            onMouseLeave={(e) => (e.currentTarget.style.background = 'var(--accent)')}
          >
            <Plus size={12} />
            New
          </button>
        </div>
        {supernodes.map((supernode) => {
          const isSelected = selectedSupernode === supernode.name;
          return (
            <div
              key={supernode.name}
              onClick={() => onSelectSupernode(supernode.name)}
              className="mx-2 mb-1 cursor-pointer flex items-center justify-between group"
              style={{
                padding: '8px 10px',
                borderRadius: 'var(--radius-sm)',
                background: isSelected ? 'var(--surface-active)' : 'transparent',
                boxShadow: isSelected ? 'inset 0 0 0 1px var(--accent-ring)' : 'none',
                transition: 'background var(--dur-fast) var(--ease-out)',
              }}
              onMouseEnter={(e) => {
                if (!isSelected) e.currentTarget.style.background = 'var(--surface-hover)';
              }}
              onMouseLeave={(e) => {
                if (!isSelected) e.currentTarget.style.background = 'transparent';
              }}
            >
              <div className="flex flex-col min-w-0">
                <span
                  className="truncate"
                  style={{
                    fontSize: 'var(--text-sm)',
                    fontWeight: 'var(--weight-medium)' as never,
                    color: 'var(--text-primary)',
                  }}
                >
                  {supernode.name}
                </span>
                <span
                  className="truncate"
                  style={{
                    fontFamily: 'var(--font-mono)',
                    fontSize: 'var(--text-xs)',
                    color: 'var(--text-muted)',
                  }}
                >
                  {supernode.description || `${supernode.nodes.length} nodes`}
                </span>
              </div>
              <button
                onClick={(e) => {
                  e.stopPropagation();
                  onDeleteSupernode(supernode.name);
                }}
                className="opacity-0 group-hover:opacity-100 flex items-center justify-center rounded transition-all"
                style={{ width: 22, height: 22, color: 'var(--error)' }}
                aria-label={`Delete supernode ${supernode.name}`}
              >
                <X size={13} />
              </button>
            </div>
          );
        })}
      </div>

      {/* Plugin Configs */}
      <div className="shrink-0" style={{ borderTop: '1px solid var(--border)', maxHeight: 260, overflowY: 'auto' }}>
        <div className="p-3 flex items-center justify-between">
          <span className="eyebrow">Plugin Configs</span>
          <button
            onClick={onCreatePluginConfig}
            aria-label="New plugin config"
            className="flex items-center gap-1 transition-colors"
            style={{
              fontSize: 'var(--text-xs)',
              fontWeight: 'var(--weight-medium)' as never,
              padding: '3px 8px',
              borderRadius: 'var(--radius-sm)',
              background: 'var(--accent)',
              color: 'var(--text-on-accent)',
            }}
            onMouseEnter={(e) => (e.currentTarget.style.background = 'var(--accent-hover)')}
            onMouseLeave={(e) => (e.currentTarget.style.background = 'var(--accent)')}
          >
            <Plus size={12} />
            New
          </button>
        </div>
        {pluginConfigs.map((pc) => {
          const isSelected = selectedPluginConfig === pc.name;
          return (
            <div
              key={pc.name}
              onClick={() => onSelectPluginConfig(pc.name)}
              className="mx-2 mb-1 cursor-pointer flex items-center justify-between group"
              style={{
                padding: '8px 10px',
                borderRadius: 'var(--radius-sm)',
                background: isSelected ? 'var(--surface-active)' : 'transparent',
                boxShadow: isSelected ? 'inset 0 0 0 1px var(--accent-ring)' : 'none',
                transition: 'background var(--dur-fast) var(--ease-out)',
              }}
              onMouseEnter={(e) => {
                if (!isSelected) e.currentTarget.style.background = 'var(--surface-hover)';
              }}
              onMouseLeave={(e) => {
                if (!isSelected) e.currentTarget.style.background = 'transparent';
              }}
            >
              <div className="flex flex-col min-w-0">
                <span
                  className="truncate"
                  style={{
                    fontSize: 'var(--text-sm)',
                    fontWeight: 'var(--weight-medium)' as never,
                    color: 'var(--text-primary)',
                  }}
                >
                  {pc.name}
                </span>
                <span
                  className="truncate"
                  style={{
                    fontFamily: 'var(--font-mono)',
                    fontSize: 'var(--text-xs)',
                    color: 'var(--text-muted)',
                  }}
                >
                  {pc.description || pc.type}
                </span>
              </div>
              <button
                onClick={(e) => {
                  e.stopPropagation();
                  onDeletePluginConfig(pc.name);
                }}
                className="opacity-0 group-hover:opacity-100 flex items-center justify-center rounded transition-all"
                style={{ width: 22, height: 22, color: 'var(--error)' }}
                aria-label={`Delete plugin config ${pc.name}`}
              >
                <X size={13} />
              </button>
            </div>
          );
        })}
      </div>

      {/* Stores */}
      <div className="shrink-0" style={{ borderTop: '1px solid var(--border)', maxHeight: 260, overflowY: 'auto' }}>
        <div className="p-3 flex items-center justify-between">
          <span className="eyebrow">Stores</span>
          <button
            onClick={onCreateStore}
            aria-label="New store"
            className="flex items-center gap-1 transition-colors"
            style={{
              fontSize: 'var(--text-xs)',
              fontWeight: 'var(--weight-medium)' as never,
              padding: '3px 8px',
              borderRadius: 'var(--radius-sm)',
              background: 'var(--accent)',
              color: 'var(--text-on-accent)',
            }}
            onMouseEnter={(e) => (e.currentTarget.style.background = 'var(--accent-hover)')}
            onMouseLeave={(e) => (e.currentTarget.style.background = 'var(--accent)')}
          >
            <Plus size={12} />
            New
          </button>
        </div>
        {stores.map((s) => {
          const isSelected = selectedStore === s.name;
          return (
            <div
              key={s.name}
              onClick={() => onSelectStore(s.name)}
              className="mx-2 mb-1 cursor-pointer flex items-center justify-between group"
              style={{
                padding: '8px 10px',
                borderRadius: 'var(--radius-sm)',
                background: isSelected ? 'var(--surface-active)' : 'transparent',
                boxShadow: isSelected ? 'inset 0 0 0 1px var(--accent-ring)' : 'none',
                transition: 'background var(--dur-fast) var(--ease-out)',
              }}
              onMouseEnter={(e) => {
                if (!isSelected) e.currentTarget.style.background = 'var(--surface-hover)';
              }}
              onMouseLeave={(e) => {
                if (!isSelected) e.currentTarget.style.background = 'transparent';
              }}
            >
              <div className="flex flex-col min-w-0">
                <span
                  className="truncate"
                  style={{
                    fontSize: 'var(--text-sm)',
                    fontWeight: 'var(--weight-medium)' as never,
                    color: 'var(--text-primary)',
                  }}
                >
                  {s.name}
                </span>
                <span
                  className="truncate"
                  style={{
                    fontFamily: 'var(--font-mono)',
                    fontSize: 'var(--text-xs)',
                    color: 'var(--text-muted)',
                  }}
                >
                  {s.description || `${s.type} · ${s.url}`}
                </span>
              </div>
              <button
                onClick={(e) => {
                  e.stopPropagation();
                  onDeleteStore(s.name);
                }}
                className="opacity-0 group-hover:opacity-100 flex items-center justify-center rounded transition-all"
                style={{ width: 22, height: 22, color: 'var(--error)' }}
                aria-label={`Delete store ${s.name}`}
              >
                <X size={13} />
              </button>
            </div>
          );
        })}
      </div>

      {/* Footer */}
      <div style={{ padding: 12, borderTop: '1px solid var(--border)', display: 'flex', flexDirection: 'column', gap: 8 }}>
        {/* Persistent log of every toast: a rejected save that flashed by
            during a busy moment stays inspectable here, server reason included. */}
        <button
          onClick={onOpenNotifications}
          aria-label="Notifications"
          title={
            unreadNotifications > 0
              ? `${unreadNotifications} unread error${unreadNotifications === 1 ? '' : 's'} — open the notification log`
              : 'Notification log — every save outcome, inspectable afterwards'
          }
          className="w-full flex items-center justify-center gap-1.5 transition-colors"
          style={{
            padding: '7px 0',
            borderRadius: 'var(--radius-sm)',
            fontSize: 'var(--text-xs)',
            fontWeight: 'var(--weight-medium)' as never,
            background: 'var(--surface-input)',
            color: 'var(--text-primary)',
            border: `1px solid ${unreadNotifications > 0 ? 'var(--error)' : 'var(--border)'}`,
          }}
          onMouseEnter={(e) => (e.currentTarget.style.filter = 'brightness(1.08)')}
          onMouseLeave={(e) => (e.currentTarget.style.filter = 'none')}
        >
          <Bell size={12} />
          Notifications
          {unreadNotifications > 0 && (
            <span
              data-testid="notifications-badge"
              style={{
                minWidth: 16,
                padding: '0 5px',
                borderRadius: 999,
                fontFamily: 'var(--font-mono)',
                fontSize: 10,
                lineHeight: '16px',
                fontWeight: 700,
                background: 'var(--error)',
                color: '#fff',
              }}
            >
              {unreadNotifications}
            </span>
          )}
        </button>
        <button
          onClick={onOpenCertificates}
          aria-label="Certificates"
          title="ACME-managed TLS certificates"
          className="w-full flex items-center justify-center gap-1.5 transition-colors"
          style={{
            padding: '7px 0',
            borderRadius: 'var(--radius-sm)',
            fontSize: 'var(--text-xs)',
            fontWeight: 'var(--weight-medium)' as never,
            background: 'var(--surface-input)',
            color: 'var(--text-primary)',
            border: '1px solid var(--border)',
          }}
          onMouseEnter={(e) => (e.currentTarget.style.filter = 'brightness(1.08)')}
          onMouseLeave={(e) => (e.currentTarget.style.filter = 'none')}
        >
          <ShieldCheck size={12} />
          Certificates
        </button>
        {/* Always rendered, even with no stores declared: a developer who
            cannot find the button files a bug, one who sees it greyed out
            fixes their config (declares a redis/valkey store). */}
        <button
          onClick={onOpenSessions}
          title={
            stores.length === 0
              ? 'Server-side sessions — requires a declared redis/valkey store'
              : 'List and revoke server-side sessions'
          }
          aria-label="Sessions"
          className="w-full flex items-center justify-center gap-1.5 transition-colors"
          style={{
            padding: '7px 0',
            borderRadius: 'var(--radius-sm)',
            fontSize: 'var(--text-xs)',
            fontWeight: 'var(--weight-medium)' as never,
            background: 'var(--surface-input)',
            color: stores.length === 0 ? 'var(--text-muted)' : 'var(--text-primary)',
            border: '1px solid var(--border)',
          }}
          onMouseEnter={(e) => (e.currentTarget.style.filter = 'brightness(1.08)')}
          onMouseLeave={(e) => (e.currentTarget.style.filter = 'none')}
        >
          <KeyRound size={12} />
          Sessions
        </button>
        {/* Always rendered, even when debug is off: a developer who cannot find
            the button files a bug, one who sees it greyed out fixes their config. */}
        <button
          onClick={onOpenDebug}
          title={
            debugEnabled
              ? 'Browse policy traces and run the plugin sandbox'
              : 'Debug mode is off — set debug.enabled in system.yaml and restart'
          }
          className="w-full flex items-center justify-center gap-1.5 transition-colors"
          style={{
            padding: '7px 0',
            borderRadius: 'var(--radius-sm)',
            fontSize: 'var(--text-xs)',
            fontWeight: 'var(--weight-medium)' as never,
            background: 'var(--surface-input)',
            color: debugEnabled ? 'var(--text-primary)' : 'var(--text-muted)',
            border: '1px solid var(--border)',
          }}
          onMouseEnter={(e) => (e.currentTarget.style.filter = 'brightness(1.08)')}
          onMouseLeave={(e) => (e.currentTarget.style.filter = 'none')}
        >
          <Bug size={12} />
          Debug
        </button>
        <button
          onClick={onViewYaml}
          className="w-full flex items-center justify-center gap-1.5 transition-colors"
          style={{
            padding: '7px 0',
            borderRadius: 'var(--radius-sm)',
            fontSize: 'var(--text-xs)',
            fontWeight: 'var(--weight-medium)' as never,
            background: 'var(--surface-input)',
            color: 'var(--text-primary)',
            border: '1px solid var(--border)',
          }}
          onMouseEnter={(e) => (e.currentTarget.style.filter = 'brightness(1.08)')}
          onMouseLeave={(e) => (e.currentTarget.style.filter = 'none')}
        >
          <FileCode size={12} />
          View YAML
        </button>
        <button
          onClick={onReload}
          className="w-full flex items-center justify-center gap-1.5 transition-colors"
          style={{
            padding: '7px 0',
            borderRadius: 'var(--radius-sm)',
            fontSize: 'var(--text-xs)',
            fontWeight: 'var(--weight-medium)' as never,
            background: 'var(--surface-input)',
            color: 'var(--text-primary)',
            border: '1px solid var(--border)',
          }}
          onMouseEnter={(e) => (e.currentTarget.style.filter = 'brightness(1.08)')}
          onMouseLeave={(e) => (e.currentTarget.style.filter = 'none')}
        >
          <RotateCw size={12} />
          Reload Config
        </button>
      </div>
    </div>
  );
}
