/**
 * Add-node palette for the policy editor: a right-hand drawer listing the
 * gateway's native plugin types (GET /api/plugins) and discovered script
 * files (GET /api/scripts) that can be added as nodes to the graph.
 *
 * Native plugins are grouped into the same collapsible categories the docs
 * sidebar uses (see {@link module:pluginCategories}) and filterable with a
 * quick-search box that matches on type name and description.
 *
 * @module components/PluginDrawer
 */
import { useMemo, useState } from 'react';
import { ChevronRight, FileCode2, Moon, Search, X } from 'lucide-react';
import type { PluginType, ScriptFile } from '../types';
import { getPluginMeta } from '../pluginMeta';
import { groupPluginsByCategory } from '../pluginCategories';

/** Props for {@link PluginDrawer}. */
interface PluginDrawerProps {
  /** Native plugin types from GET /api/plugins; `listener` and `script` are filtered out of the list. */
  plugins: PluginType[];
  /** Script files from GET /api/scripts; the section is hidden when empty. */
  scripts: ScriptFile[];
  /** Fires with the plugin type when a native plugin entry is clicked. */
  onAddPlugin: (type: string) => void;
  /** Fires with the script file when a script entry is clicked. */
  onAddScript: (script: ScriptFile) => void;
  /** Whether the drawer is visible; when false the component renders nothing. */
  isOpen: boolean;
  /** Fires when the close (X) button is clicked. */
  onClose: () => void;
}

/**
 * Small rounded square that tints an icon with a plugin's accent color.
 *
 * @param color - Accent color; used for the icon and, at 18% opacity, the tile background.
 * @param children - Icon element to render inside the tile.
 */
function IconTile({ color, children }: { color: string; children: React.ReactNode }) {
  return (
    <span
      className="flex items-center justify-center shrink-0"
      style={{
        width: 28,
        height: 28,
        borderRadius: 'var(--radius-sm)',
        background: `color-mix(in srgb, ${color} 18%, transparent)`,
        color,
      }}
    >
      {children}
    </span>
  );
}

/** Row button shared by native-plugin and script entries. */
function NodeRow({
  onClick,
  color,
  icon,
  title,
  subtitle,
  subtitleMono = false,
}: {
  onClick: () => void;
  color: string;
  icon: React.ReactNode;
  title: string;
  subtitle: string;
  subtitleMono?: boolean;
}) {
  return (
    <button
      onClick={onClick}
      className="w-full text-left flex items-center transition-colors"
      style={{
        gap: 10,
        padding: '8px 10px',
        borderRadius: 'var(--radius-sm)',
        background: 'var(--surface-raised)',
        border: '1px solid var(--border-subtle)',
        color: 'var(--text-primary)',
      }}
      onMouseEnter={(e) => (e.currentTarget.style.borderColor = 'var(--accent-ring)')}
      onMouseLeave={(e) => (e.currentTarget.style.borderColor = 'var(--border-subtle)')}
    >
      <IconTile color={color}>{icon}</IconTile>
      <div className="flex flex-col min-w-0">
        <span
          style={{
            fontFamily: 'var(--font-mono)',
            fontSize: 'var(--text-sm)',
            fontWeight: 'var(--weight-medium)' as never,
          }}
        >
          {title}
        </span>
        <span
          className="truncate"
          style={{
            fontFamily: subtitleMono ? 'var(--font-mono)' : undefined,
            fontSize: 'var(--text-xs)',
            color: 'var(--text-secondary)',
          }}
        >
          {subtitle}
        </span>
      </div>
    </button>
  );
}

/**
 * Drawer panel for adding nodes to the policy graph.
 *
 * Native plugins (every fetched type except `listener`, the fixed entry point,
 * and `script`, added via concrete files) are grouped into the docs
 * categories under collapsible headings, and a search box filters them by type
 * name and description. Script files appear in their own "Script Plugins"
 * section, also honoring the search. Clicking an entry delegates node creation
 * to `onAddPlugin`/`onAddScript`; the drawer itself does not mutate the graph.
 *
 * Categories start collapsed (like the docs sidebar); an active search
 * force-expands every group so all matches are visible at once.
 *
 * @remarks
 * Native plugin types correspond to the Rust implementations registered in
 * src/plugins/mod.rs (create_plugin); script entries become `script` nodes
 * executed by src/plugins/script/lua_runtime.rs.
 */
export function PluginDrawer({ plugins, scripts, onAddPlugin, onAddScript, isOpen, onClose }: PluginDrawerProps) {
  const [query, setQuery] = useState('');
  // Labels of categories the user has expanded. Empty = all collapsed.
  const [expanded, setExpanded] = useState<Set<string>>(new Set());

  const q = query.trim().toLowerCase();
  const searching = q.length > 0;

  const groups = useMemo(() => {
    const draggable = plugins.filter((p) => p.type !== 'listener' && p.type !== 'script');
    const matched = q
      ? draggable.filter(
          (p) => p.type.toLowerCase().includes(q) || p.description?.toLowerCase().includes(q),
        )
      : draggable;
    return groupPluginsByCategory(matched);
  }, [plugins, q]);

  const matchedScripts = useMemo(
    () =>
      q
        ? scripts.filter(
            (s) => s.name.toLowerCase().includes(q) || s.file.toLowerCase().includes(q),
          )
        : scripts,
    [scripts, q],
  );

  if (!isOpen) return null;

  const toggle = (label: string) =>
    setExpanded((prev) => {
      const next = new Set(prev);
      if (next.has(label)) next.delete(label);
      else next.add(label);
      return next;
    });

  const nothingMatches = searching && groups.length === 0 && matchedScripts.length === 0;

  return (
    <div
      className="absolute right-0 top-0 h-full z-50 flex flex-col"
      style={{
        width: 'var(--rail-drawer)',
        background: 'var(--surface)',
        borderLeft: '1px solid var(--border)',
        boxShadow: 'var(--shadow-panel)',
      }}
    >
      <div
        className="flex items-center justify-between"
        style={{ padding: '14px 16px', borderBottom: '1px solid var(--border)' }}
      >
        <span
          style={{
            fontSize: 'var(--text-base)',
            fontWeight: 'var(--weight-semibold)' as never,
            color: 'var(--text-primary)',
          }}
        >
          Add Node
        </span>
        <button
          onClick={onClose}
          className="flex items-center justify-center rounded transition-colors"
          style={{ width: 26, height: 26, color: 'var(--text-secondary)' }}
          onMouseEnter={(e) => (e.currentTarget.style.background = 'var(--surface-hover)')}
          onMouseLeave={(e) => (e.currentTarget.style.background = 'transparent')}
          aria-label="Close"
        >
          <X size={15} />
        </button>
      </div>

      {/* Search */}
      <div style={{ padding: '10px 12px', borderBottom: '1px solid var(--border)' }}>
        <div className="flex items-center" style={{ position: 'relative' }}>
          <Search
            size={14}
            style={{ position: 'absolute', left: 10, color: 'var(--text-muted)', pointerEvents: 'none' }}
          />
          <input
            type="text"
            value={query}
            autoFocus
            placeholder="Search plugins…"
            onChange={(e) => setQuery(e.target.value)}
            onKeyDown={(e) => e.key === 'Escape' && (query ? setQuery('') : onClose())}
            className="w-full"
            style={{
              padding: '7px 28px 7px 30px',
              borderRadius: 'var(--radius-sm)',
              fontSize: 'var(--text-sm)',
              background: 'var(--surface-input)',
              color: 'var(--text-primary)',
              border: '1px solid var(--border)',
            }}
          />
          {query && (
            <button
              onClick={() => setQuery('')}
              className="flex items-center justify-center"
              style={{ position: 'absolute', right: 6, width: 20, height: 20, color: 'var(--text-muted)' }}
              aria-label="Clear search"
            >
              <X size={13} />
            </button>
          )}
        </div>
      </div>

      <div className="flex-1 overflow-y-auto p-3 space-y-1.5">
        {nothingMatches && (
          <p
            style={{
              fontSize: 'var(--text-sm)',
              color: 'var(--text-muted)',
              textAlign: 'center',
              padding: '24px 8px',
            }}
          >
            No plugins match “{query}”.
          </p>
        )}

        {/* Native plugins, grouped into collapsible categories */}
        {groups.map((group) => {
          const isOpenGroup = searching || expanded.has(group.label);
          return (
            <div key={group.label}>
              <button
                onClick={() => toggle(group.label)}
                className="w-full flex items-center transition-colors"
                style={{
                  gap: 6,
                  padding: '6px 6px',
                  borderRadius: 'var(--radius-sm)',
                  background: 'transparent',
                  color: 'var(--text-secondary)',
                }}
                onMouseEnter={(e) => (e.currentTarget.style.background = 'var(--surface-hover)')}
                onMouseLeave={(e) => (e.currentTarget.style.background = 'transparent')}
                aria-expanded={isOpenGroup}
              >
                <ChevronRight
                  size={13}
                  style={{
                    transition: 'transform var(--dur-fast) var(--ease-out)',
                    transform: isOpenGroup ? 'rotate(90deg)' : 'none',
                    color: 'var(--text-muted)',
                  }}
                />
                <span
                  className="eyebrow"
                  style={{ flex: 1, textAlign: 'left', padding: 0 }}
                >
                  {group.label}
                </span>
                <span
                  style={{
                    fontFamily: 'var(--font-mono)',
                    fontSize: 'var(--text-2xs)',
                    color: 'var(--text-muted)',
                  }}
                >
                  {group.items.length}
                </span>
              </button>
              {isOpenGroup && (
                <div className="space-y-1.5" style={{ marginTop: 4, marginBottom: 4 }}>
                  {group.items.map((plugin) => {
                    const meta = getPluginMeta(plugin.type);
                    const Icon = meta.icon;
                    return (
                      <NodeRow
                        key={plugin.type}
                        onClick={() => onAddPlugin(plugin.type)}
                        color={meta.color}
                        icon={<Icon size={15} strokeWidth={1.75} />}
                        title={plugin.type}
                        subtitle={plugin.description}
                      />
                    );
                  })}
                </div>
              )}
            </div>
          );
        })}

        {/* Script plugins */}
        {matchedScripts.length > 0 && (
          <>
            <div className="eyebrow px-1 pt-4 pb-2">Script Plugins</div>
            {matchedScripts.map((script) => (
              <NodeRow
                key={script.file}
                onClick={() => onAddScript(script)}
                color="var(--plugin-script)"
                icon={
                  script.runtime === 'lua' ? (
                    <Moon size={15} strokeWidth={1.75} />
                  ) : (
                    <FileCode2 size={15} strokeWidth={1.75} />
                  )
                }
                title={script.name}
                subtitle={`${script.runtime} · ${script.file.split('/').pop()}`}
                subtitleMono
              />
            ))}
          </>
        )}
      </div>
    </div>
  );
}
