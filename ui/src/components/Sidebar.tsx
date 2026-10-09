/**
 * Left navigation rail of the admin UI: featherbit branding with live gateway
 * status, the selectable route list, and the create-route / delete-route /
 * reorder-route / reload-config actions that back the gateway's admin API.
 * Routes are listed in match order (first match wins): each row can be
 * dragged by its grip, or nudged with its up/down buttons, to change priority.
 *
 * The body shows one library at a time, chosen by the strip of buttons under
 * the header. Routes is the default and the common case, so it gets the whole
 * body instead of the quarter it had when all four libraries were stacked --
 * which was the point of the change: with supernodes, plugin configs and
 * stores each claiming a fixed 260px band, the route list was the first thing
 * squeezed and the last thing anyone wanted squeezed.
 *
 * @module components/Sidebar
 */
import { useEffect, useState, type CSSProperties } from 'react';
import {
  Plus,
  RotateCw,
  X,
  FileCode,
  Bug,
  KeyRound,
  ShieldCheck,
  Bell,
  Bot,
  MessageSquare,
  LogOut,
  TriangleAlert,
  Boxes,
  Puzzle,
  Database,
  GripVertical,
  ChevronUp,
  ChevronDown,
  RefreshCw,
  Pencil,
} from 'lucide-react';
import type { Route, Supernode, PluginConfigDef, StoreConfig, GatewayStatus } from '../types';
import { api } from '../api/client';
import { moveBy, moveTo } from '../routeOrder';
import { describeMatch } from '../routeMatch';
import { getUsername, signOut } from '../auth';

/** Types CSS custom properties (the rg-* hover / press vars) as a style object. */
const cssVars = (vars: Record<string, string>): CSSProperties => vars as CSSProperties;

/**
 * Shared style of the eight two-column footer buttons. The last four
 * properties keep a long label ("Notifications" plus its unread badge,
 * "Certificates", "Reload Config") on one line inside a half-width grid cell
 * instead of wrapping and growing the footer; the text and aria-label are
 * unchanged, only what an overflow does.
 */
const footerButtonStyle: CSSProperties = {
  padding: '7px 0',
  borderRadius: 'var(--radius-sm)',
  fontSize: 'var(--text-xs)',
  fontWeight: 'var(--weight-medium)' as never,
  borderWidth: 1,
  borderStyle: 'solid',
  ...cssVars({
    '--rg-bg': 'var(--surface-raised)',
    '--rg-fg': 'var(--text-primary)',
    '--rg-bd': 'var(--border)',
    '--rg-hover-bg': 'var(--surface-raised)',
    '--rg-hover-bd': 'var(--border-strong)',
  }),
  boxShadow: 'var(--shadow-inset)',
  minWidth: 0,
  whiteSpace: 'nowrap',
  overflow: 'hidden',
  textOverflow: 'ellipsis',
};

/**
 * Which library the sidebar body is showing. `routes` is the default and the
 * one operators live in; the other three are opened from the strip and step
 * aside again as soon as something is selected.
 */
type Library = 'routes' | 'supernodes' | 'pluginConfigs' | 'stores';

/** Secondary button vars: raised neutral, border brightens on hover. */
const secondaryButtonVars = cssVars({
  '--rg-bg': 'var(--surface-raised)',
  '--rg-fg': 'var(--text-primary)',
  '--rg-bd': 'var(--border)',
  '--rg-hover-bg': 'var(--surface-raised)',
  '--rg-hover-bd': 'var(--border-strong)',
});

/** Row background vars: quiet tint when selected, hover tint otherwise. */
const rowVars = (selected: boolean): CSSProperties =>
  cssVars({
    '--rg-bg': selected ? 'var(--surface-active)' : 'transparent',
    '--rg-hover-bg': selected ? 'var(--surface-active)' : 'var(--surface-hover)',
  });

/**
 * Selection marker for a list row: a 2px violet rail inside the row's left
 * edge, over the --surface-active tint. Selection is the most-seen state, so
 * it stays quiet: no full accent fill, no outlined box.
 */
const selectionRailStyle: CSSProperties = {
  position: 'absolute',
  left: 0,
  top: 7,
  bottom: 7,
  width: 2,
  borderRadius: 2,
  background: 'var(--accent-border)',
  pointerEvents: 'none',
};

/** Hover vars of a row's quiet actions (refresh, reorder, edit). */
const rowActionVars = cssVars({ '--rg-fg': 'var(--text-muted)', '--rg-hover-fg': 'var(--text-primary)' });

/** Hover vars of a row's delete action: muted until hovered, then red. */
const deleteActionVars = cssVars({
  '--rg-fg': 'var(--text-muted)',
  '--rg-hover-fg': 'var(--error)',
  '--rg-hover-bg': 'var(--error-soft)',
});

/** Shared style of the library strip's buttons. */
const stripButtonStyle: CSSProperties = {
  padding: '6px 0',
  borderRadius: 'var(--radius-sm)',
  fontSize: 'var(--text-xs)',
  display: 'flex',
  alignItems: 'center',
  justifyContent: 'center',
  gap: 5,
};

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
  /** Called with the route's name when its hover-revealed edit button is clicked; the parent opens the edit-route dialog. */
  onEditRoute: (name: string) => void;
  /** Called with every route name in the new match order after a drag or an up/down click. */
  onReorderRoutes: (order: string[]) => void;
  /** Re-fetches everything the UI shows from the admin API (no gateway-side reload). */
  onRefresh: () => Promise<void>;
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
  /** Opens the Agent (MCP) panel. */
  onOpenAgent: () => void;
  /** Whether the MCP server is on (dims the button when off, like Debug). */
  mcpEnabled: boolean;
  /** Opens the in-UI agent chat. */
  onOpenChat: () => void;
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
  onEditRoute,
  onReorderRoutes,
  onRefresh,
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
  onOpenAgent,
  mcpEnabled,
  onOpenChat,
}: SidebarProps) {
  const [status, setStatus] = useState<GatewayStatus | null>(null);
  const [library, setLibrary] = useState<Library>('routes');
  const [refreshing, setRefreshing] = useState(false);
  // Route drag state: the row being dragged, and the gap it would drop into
  // (0 = above the first row, routes.length = below the last).
  const [dragFrom, setDragFrom] = useState<number | null>(null);
  const [dropAt, setDropAt] = useState<number | null>(null);
  const routeNames = routes.map((r) => r.name);
  const reorder = (next: string[] | null) => {
    if (next) onReorderRoutes(next);
  };
  const endDrag = () => {
    setDragFrom(null);
    setDropAt(null);
  };
  const refresh = async () => {
    if (refreshing) return;
    setRefreshing(true);
    try {
      await onRefresh();
      setStatus(await api.status());
    } catch {
      // onRefresh reports its own failure; a status miss just keeps the old line.
    } finally {
      setRefreshing(false);
    }
  };

  /**
   * Picking something from a library is the end of that errand, so the body
   * goes back to routes. Leaving it open would hide the route list behind a
   * list nobody is reading any more -- the clutter this change removes.
   */
  const pick = <T,>(select: (value: T) => void) => (value: T) => {
    select(value);
    setLibrary('routes');
  };

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
        <button
          onClick={() => void refresh()}
          disabled={refreshing}
          aria-label="Refresh"
          title="Refresh: re-fetch routes, policies and libraries from the gateway (unsaved canvas edits are kept)"
          className="rg-press rg-hover ml-auto flex items-center justify-center rounded"
          style={{ width: 26, height: 26, flexShrink: 0, ...rowActionVars }}
        >
          <RefreshCw size={14} className={refreshing ? 'animate-spin' : undefined} />
        </button>
      </div>

      {/* Library strip: one button per library, each toggling the body. The
          counts are here so the badge answers "do I have any supernodes?"
          without opening anything -- the question the old always-visible
          lists answered by costing permanent height. */}
      <div
        style={{
          padding: '8px 10px',
          borderTop: '1px solid var(--border)',
          display: 'grid',
          gridTemplateColumns: 'repeat(3, 1fr)',
          gap: 6,
        }}
      >
        {(
          [
            ['supernodes', 'Supernodes', Boxes, supernodes.length],
            ['pluginConfigs', 'Plugin configs', Puzzle, pluginConfigs.length],
            ['stores', 'Stores', Database, stores.length],
          ] as const
        ).map(([id, label, Icon, count]) => {
          const active = library === id;
          return (
            <button
              key={id}
              onClick={() => setLibrary(active ? 'routes' : id)}
              aria-label={label}
              aria-pressed={active}
              title={`${label} (${count})`}
              className="rg-press rg-hover"
              style={{
                ...stripButtonStyle,
                borderWidth: 1,
                borderStyle: 'solid',
                ...cssVars({
                  '--rg-bg': active ? 'var(--surface-active)' : 'transparent',
                  '--rg-fg': active ? 'var(--accent-fg)' : 'var(--text-muted)',
                  '--rg-bd': active ? 'var(--accent-ring)' : 'var(--border-subtle)',
                  '--rg-hover-bg': active ? 'var(--surface-active)' : 'var(--surface-hover)',
                  '--rg-hover-fg': active ? 'var(--accent-fg)' : 'var(--text-primary)',
                }),
              }}
            >
              <Icon size={13} />
              {count}
            </button>
          );
        })}
      </div>

      {/* Each library is mounted only while it is the active one. Hiding with
          CSS would leave every list in the DOM, where a page-wide locator
          still finds it -- the shape of bug that made an e2e assertion pass
          against a form label instead of the result it meant to check. */}
      {library === 'routes' && (
      <div className="flex-1 overflow-y-auto min-h-40">
        <div className="p-3 flex items-center justify-between">
          <span
            className="eyebrow"
            title="Matched top to bottom: the first matching route wins. Drag a route by its grip, or use its arrows, to change priority."
          >
            Routes
          </span>
          <button
            onClick={onCreateRoute}
            aria-label="New route"
            className="rg-press rg-hover flex items-center gap-1"
            style={{
              fontSize: 'var(--text-xs)',
              fontWeight: 'var(--weight-medium)' as never,
              padding: '3px 8px',
              borderRadius: 'var(--radius-sm)',
              borderWidth: 1,
              borderStyle: 'solid',
              ...secondaryButtonVars,
            }}
          >
            <Plus size={12} />
            New
          </button>
        </div>
        {routes.length > 1 && (
          <p
            style={{
              margin: '-4px 12px 6px',
              fontSize: 'var(--text-2xs)',
              color: 'var(--text-muted)',
            }}
          >
            Matched top to bottom &middot; drag to reorder
          </p>
        )}
        <div
          role="list"
          aria-label="Routes in match order"
          onDragOver={(e) => {
            // The empty space below the last row counts as "drop at the end".
            if (dragFrom !== null && e.target === e.currentTarget) {
              e.preventDefault();
              setDropAt(routes.length);
            }
          }}
          onDrop={(e) => {
            e.preventDefault();
            if (dragFrom !== null && dropAt !== null) reorder(moveTo(routeNames, dragFrom, dropAt));
            endDrag();
          }}
          style={{ paddingBottom: 12 }}
        >
          {routes.map((route, index) => {
            const isSelected = selectedRoute === route.name;
            const isDragged = dragFrom === index;
            // The drop indicator sits on the top edge of the row after the
            // gap, or on the bottom edge of the last row for "drop at the end";
            // gaps that would not move the dragged row show nothing.
            const moves = (at: number) => dragFrom !== null && moveTo(routeNames, dragFrom, at) !== null;
            const lineAbove = dropAt === index && moves(index);
            const lineBelow = index === routes.length - 1 && dropAt === routes.length && moves(routes.length);
            return (
              <div
                key={route.name}
                role="listitem"
                data-testid={`route-row-${route.name}`}
                draggable
                onDragStart={(e) => {
                  e.dataTransfer.effectAllowed = 'move';
                  e.dataTransfer.setData('text/plain', route.name);
                  setDragFrom(index);
                }}
                onDragEnd={endDrag}
                onDragOver={(e) => {
                  if (dragFrom === null) return;
                  e.preventDefault();
                  e.dataTransfer.dropEffect = 'move';
                  const rect = e.currentTarget.getBoundingClientRect();
                  setDropAt(e.clientY < rect.top + rect.height / 2 ? index : index + 1);
                }}
                onClick={() => onSelectRoute(route.name)}
                className="rg-hover mx-2 mb-1 cursor-pointer flex items-center justify-between group"
                style={{
                  position: 'relative',
                  padding: '8px 6px 8px 2px',
                  borderRadius: 'var(--radius-sm)',
                  ...rowVars(isSelected),
                  opacity: isDragged ? 0.45 : 1,
                  transition: 'background-color var(--dur-fast) ease, opacity var(--dur-fast) ease',
                }}
              >
                {isSelected && <span aria-hidden style={selectionRailStyle} />}
                {(lineAbove || lineBelow) && (
                  <span
                    aria-hidden
                    style={{
                      position: 'absolute',
                      left: 4,
                      right: 4,
                      [lineAbove ? 'top' : 'bottom']: -3,
                      height: 2,
                      borderRadius: 1,
                      background: 'var(--accent)',
                      pointerEvents: 'none',
                    }}
                  />
                )}
                <div className="flex items-center min-w-0" style={{ gap: 4 }}>
                  <span
                    aria-hidden
                    title="Drag to change priority"
                    className="flex items-center opacity-40 group-hover:opacity-100 transition-opacity"
                    style={{ cursor: 'grab', color: 'var(--text-muted)', flexShrink: 0 }}
                  >
                    <GripVertical size={13} />
                  </span>
                  <span
                    title={`Priority ${index + 1}: matched ${index === 0 ? 'first' : `after ${index} other route${index === 1 ? '' : 's'}`}`}
                    style={{
                      fontFamily: 'var(--font-mono)',
                      fontSize: 'var(--text-2xs)',
                      color: 'var(--text-muted)',
                      minWidth: 14,
                      textAlign: 'right',
                      flexShrink: 0,
                    }}
                  >
                    {index + 1}
                  </span>
                  <div className="flex flex-col min-w-0" style={{ marginLeft: 4 }}>
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
                      {describeMatch(route.match ?? {})}
                    </span>
                  </div>
                </div>
                <div
                  className="flex items-center opacity-0 group-hover:opacity-100 group-focus-within:opacity-100 transition-opacity"
                  style={{ flexShrink: 0 }}
                >
                  {routes.length > 1 && (
                    <>
                      <button
                        onClick={(e) => {
                          e.stopPropagation();
                          reorder(moveBy(routeNames, index, -1));
                        }}
                        disabled={index === 0}
                        className="rg-hover flex items-center justify-center rounded disabled:opacity-30"
                        style={{ width: 20, height: 22, ...rowActionVars }}
                        aria-label={`Move route ${route.name} up`}
                        title="Higher priority"
                      >
                        <ChevronUp size={13} />
                      </button>
                      <button
                        onClick={(e) => {
                          e.stopPropagation();
                          reorder(moveBy(routeNames, index, 1));
                        }}
                        disabled={index === routes.length - 1}
                        className="rg-hover flex items-center justify-center rounded disabled:opacity-30"
                        style={{ width: 20, height: 22, ...rowActionVars }}
                        aria-label={`Move route ${route.name} down`}
                        title="Lower priority"
                      >
                        <ChevronDown size={13} />
                      </button>
                    </>
                  )}
                  <button
                    onClick={(e) => {
                      e.stopPropagation();
                      onEditRoute(route.name);
                    }}
                    className="rg-hover flex items-center justify-center rounded"
                    style={{ width: 22, height: 22, ...rowActionVars }}
                    aria-label={`Edit route ${route.name}`}
                    title="Edit match rule"
                  >
                    <Pencil size={12} />
                  </button>
                  <button
                    onClick={(e) => {
                      e.stopPropagation();
                      onDeleteRoute(route.name);
                    }}
                    className="rg-hover flex items-center justify-center rounded"
                    style={{ width: 22, height: 22, ...deleteActionVars }}
                    aria-label={`Delete route ${route.name}`}
                  >
                    <X size={13} />
                  </button>
                </div>
              </div>
            );
          })}
        </div>
      </div>

      )}

      {library === 'supernodes' && (
      <div className="flex-1 overflow-y-auto min-h-40" style={{ borderTop: '1px solid var(--border)' }}>
        <div className="p-3 flex items-center justify-between">
          <span className="eyebrow">Supernodes</span>
          <button
            onClick={onCreateSupernode}
            aria-label="New supernode"
            className="rg-press rg-hover flex items-center gap-1"
            style={{
              fontSize: 'var(--text-xs)',
              fontWeight: 'var(--weight-medium)' as never,
              padding: '3px 8px',
              borderRadius: 'var(--radius-sm)',
              borderWidth: 1,
              borderStyle: 'solid',
              ...secondaryButtonVars,
            }}
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
              onClick={() => pick(onSelectSupernode)(supernode.name)}
              className="rg-hover mx-2 mb-1 cursor-pointer flex items-center justify-between group"
              style={{
                position: 'relative',
                padding: '8px 10px',
                borderRadius: 'var(--radius-sm)',
                ...rowVars(isSelected),
                transition: 'background-color var(--dur-fast) ease',
              }}
            >
              {isSelected && <span aria-hidden style={selectionRailStyle} />}
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
                className="rg-hover opacity-0 group-hover:opacity-100 group-focus-within:opacity-100 focus-visible:opacity-100 flex items-center justify-center rounded transition-opacity"
                style={{ width: 22, height: 22, ...deleteActionVars }}
                aria-label={`Delete supernode ${supernode.name}`}
              >
                <X size={13} />
              </button>
            </div>
          );
        })}
      </div>

      )}

      {library === 'pluginConfigs' && (
      <div className="flex-1 overflow-y-auto min-h-40" style={{ borderTop: '1px solid var(--border)' }}>
        <div className="p-3 flex items-center justify-between">
          <span className="eyebrow">Plugin Configs</span>
          <button
            onClick={onCreatePluginConfig}
            aria-label="New plugin config"
            className="rg-press rg-hover flex items-center gap-1"
            style={{
              fontSize: 'var(--text-xs)',
              fontWeight: 'var(--weight-medium)' as never,
              padding: '3px 8px',
              borderRadius: 'var(--radius-sm)',
              borderWidth: 1,
              borderStyle: 'solid',
              ...secondaryButtonVars,
            }}
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
              onClick={() => pick(onSelectPluginConfig)(pc.name)}
              className="rg-hover mx-2 mb-1 cursor-pointer flex items-center justify-between group"
              style={{
                position: 'relative',
                padding: '8px 10px',
                borderRadius: 'var(--radius-sm)',
                ...rowVars(isSelected),
                transition: 'background-color var(--dur-fast) ease',
              }}
            >
              {isSelected && <span aria-hidden style={selectionRailStyle} />}
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
                className="rg-hover opacity-0 group-hover:opacity-100 group-focus-within:opacity-100 focus-visible:opacity-100 flex items-center justify-center rounded transition-opacity"
                style={{ width: 22, height: 22, ...deleteActionVars }}
                aria-label={`Delete plugin config ${pc.name}`}
              >
                <X size={13} />
              </button>
            </div>
          );
        })}
      </div>

      )}

      {library === 'stores' && (
      <div className="flex-1 overflow-y-auto min-h-40" style={{ borderTop: '1px solid var(--border)' }}>
        <div className="p-3 flex items-center justify-between">
          <span className="eyebrow">Stores</span>
          <button
            onClick={onCreateStore}
            aria-label="New store"
            className="rg-press rg-hover flex items-center gap-1"
            style={{
              fontSize: 'var(--text-xs)',
              fontWeight: 'var(--weight-medium)' as never,
              padding: '3px 8px',
              borderRadius: 'var(--radius-sm)',
              borderWidth: 1,
              borderStyle: 'solid',
              ...secondaryButtonVars,
            }}
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
              onClick={() => pick(onSelectStore)(s.name)}
              className="rg-hover mx-2 mb-1 cursor-pointer flex items-center justify-between group"
              style={{
                position: 'relative',
                padding: '8px 10px',
                borderRadius: 'var(--radius-sm)',
                ...rowVars(isSelected),
                transition: 'background-color var(--dur-fast) ease',
              }}
            >
              {isSelected && <span aria-hidden style={selectionRailStyle} />}
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
                className="rg-hover opacity-0 group-hover:opacity-100 group-focus-within:opacity-100 focus-visible:opacity-100 flex items-center justify-center rounded transition-opacity"
                style={{ width: 22, height: 22, ...deleteActionVars }}
                aria-label={`Delete store ${s.name}`}
              >
                <X size={13} />
              </button>
            </div>
          );
        })}
      </div>

      )}

      {/* Footer */}
      <div style={{ padding: 12, borderTop: '1px solid var(--border)' }}>
        {/* Two columns: with eight buttons here (Notifications through Reload
            Config), one-per-row would starve the route list of height above
            (it's the only flex-1 section — see the min-h-40 on it) at
            Playwright's default viewport. Grouping into a grid keeps every
            button's text/aria-label/title unchanged; only the layout moves. */}
        <div style={{ display: 'grid', gridTemplateColumns: '1fr 1fr', gap: 6 }}>
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
            className="rg-press rg-hover w-full flex items-center justify-center gap-1.5"
            style={{
              ...footerButtonStyle,
              ...(unreadNotifications > 0 ? cssVars({ '--rg-bd': 'var(--error)', '--rg-hover-bd': 'var(--error)' }) : null),
            }}
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
                  background: 'var(--error-solid)',
                  color: 'var(--text-on-accent)',
                }}
              >
                {unreadNotifications}
              </span>
            )}
          </button>
          <button
            onClick={onOpenAgent}
            aria-label="Agent"
            title={mcpEnabled ? 'Connect an AI agent over MCP; copy prompts' : 'MCP is off — set admin.mcp.enabled in system.yaml and restart'}
            className="rg-press rg-hover w-full flex items-center justify-center gap-1.5"
            style={{ ...footerButtonStyle, ...cssVars({ '--rg-fg': mcpEnabled ? 'var(--text-primary)' : 'var(--text-muted)' }) }}
          >
            <Bot size={12} />
            Agent
          </button>
          <button
            onClick={onOpenChat}
            aria-label="Chat"
            title="Chat with an AI agent about this gateway (your own OpenAI-compatible API key, stored in this browser)"
            className="rg-press rg-hover w-full flex items-center justify-center gap-1.5"
            style={footerButtonStyle}
          >
            <MessageSquare size={12} />
            Chat
          </button>
          <button
            onClick={onOpenCertificates}
            aria-label="Certificates"
            title="ACME-managed TLS certificates"
            className="rg-press rg-hover w-full flex items-center justify-center gap-1.5"
            style={footerButtonStyle}
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
            className="rg-press rg-hover w-full flex items-center justify-center gap-1.5"
            style={{ ...footerButtonStyle, ...cssVars({ '--rg-fg': stores.length === 0 ? 'var(--text-muted)' : 'var(--text-primary)' }) }}
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
            className="rg-press rg-hover w-full flex items-center justify-center gap-1.5"
            style={{ ...footerButtonStyle, ...cssVars({ '--rg-fg': debugEnabled ? 'var(--text-primary)' : 'var(--text-muted)' }) }}
          >
            <Bug size={12} />
            Debug
          </button>
          <button
            onClick={onViewYaml}
            className="rg-press rg-hover w-full flex items-center justify-center gap-1.5"
            style={footerButtonStyle}
          >
            <FileCode size={12} />
            View YAML
          </button>
          <button
            onClick={onReload}
            className="rg-press rg-hover w-full flex items-center justify-center gap-1.5"
            style={footerButtonStyle}
          >
            <RotateCw size={12} />
            Reload Config
          </button>
        </div>
        {status?.default_credentials && (
          <p
            role="status"
            className="flex items-start gap-1.5"
            style={{ margin: '10px 0 0', fontSize: 'var(--text-2xs)', color: 'var(--warning, var(--error))' }}
          >
            <TriangleAlert size={12} style={{ flexShrink: 0, marginTop: 1 }} />
            The admin API still uses the default admin/admin credentials. Set admin.username and admin.password.
          </p>
        )}
        <div
          className="flex items-center justify-between"
          style={{ marginTop: 10, fontSize: 'var(--text-2xs)', color: 'var(--text-muted)', gap: 8 }}
        >
          <span className="truncate" title={getUsername() ?? undefined}>
            {getUsername() ? <>Signed in as <strong style={{ color: 'var(--text-secondary)' }}>{getUsername()}</strong></> : 'Signed in by the browser'}
          </span>
          <button
            onClick={signOut}
            className="rg-hover flex items-center gap-1"
            style={{
              flexShrink: 0,
              padding: '2px 4px',
              borderRadius: 'var(--radius-xs)',
              ...cssVars({ '--rg-fg': 'var(--text-secondary)', '--rg-hover-fg': 'var(--text-primary)' }),
            }}
          >
            <LogOut size={12} />
            Sign out
          </button>
        </div>
      </div>
    </div>
  );
}
