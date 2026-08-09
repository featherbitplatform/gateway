/**
 * Declarative registry of editor actions. The command palette renders this
 * list and the global key handler binds it, so adding an action here makes
 * it both discoverable and bindable — no other file changes.
 *
 * @module commands
 */

/** Everything an action may need; supplied by App. */
export interface CommandContext {
  /** True when the node-graph editor is mounted (a policy/supernode is selected). */
  editorOpen: boolean;
  /** True when any route/supernode/config is selected. */
  hasSelection: boolean;
  togglePortNames: () => void;
  createRoute: () => void;
  createSupernode: () => void;
  createPluginConfig: () => void;
  viewYaml: () => void;
  reloadConfig: () => void;
  toggleTheme: () => void;
  /** Runs an action owned by the canvas (see editorActions.tsx). */
  invokeEditorAction: (id: string) => void;
  /** True when the canvas has registered that action. */
  hasEditorAction: (id: string) => boolean;
}

/** One palette entry. */
export interface Command {
  /** Stable kebab-case id. */
  id: string;
  /** Title shown in the palette. */
  title: string;
  /** Display + binding, e.g. "P" or "Ctrl+S". Omit for palette-only actions. */
  shortcut?: string;
  /** When false the action is hidden and its shortcut inert. */
  when?: (ctx: CommandContext) => boolean;
  run: (ctx: CommandContext) => void;
}

/** The v1 action list, in palette order. */
export function buildCommands(): Command[] {
  return [
    { id: 'toggle-port-names', title: 'Toggle port names', shortcut: 'P', run: (c) => c.togglePortNames() },
    { id: 'new-route', title: 'New route', shortcut: 'R', run: (c) => c.createRoute() },
    { id: 'new-supernode', title: 'New supernode', shortcut: 'S', run: (c) => c.createSupernode() },
    { id: 'new-plugin-config', title: 'New shared plugin config', shortcut: 'C', run: (c) => c.createPluginConfig() },
    {
      id: 'add-plugin',
      title: 'Add plugin to canvas',
      shortcut: 'A',
      // `editorOpen` is the authority on "is a graph being edited?".
      // GraphCanvas registers its actions above its own `if (!policy)` early
      // return (moving the hook calls below it would change hook ordering
      // across renders), so `hasEditorAction` alone is true even when the
      // canvas is mounted with `policy={null}` and renders its empty state.
      when: (c) => c.editorOpen && c.hasEditorAction('add-plugin'),
      run: (c) => c.invokeEditorAction('add-plugin'),
    },
    {
      id: 'save-graph',
      title: 'Save policy',
      shortcut: 'Ctrl+S',
      // Same guard as add-plugin above.
      when: (c) => c.editorOpen && c.hasEditorAction('save-graph'),
      run: (c) => c.invokeEditorAction('save-graph'),
    },
    { id: 'view-yaml', title: 'View YAML', shortcut: 'Y', when: (c) => c.hasSelection, run: (c) => c.viewYaml() },
    { id: 'reload-config', title: 'Reload gateway config', run: (c) => c.reloadConfig() },
    { id: 'toggle-theme', title: 'Toggle theme', run: (c) => c.toggleTheme() },
  ];
}

/**
 * True when a keyboard event matches a shortcut string.
 *
 * Bare single letters require no modifiers; "Ctrl+X" requires ctrl or meta.
 */
export function matchesShortcut(e: KeyboardEvent, shortcut: string): boolean {
  const ctrl = shortcut.startsWith('Ctrl+');
  const key = (ctrl ? shortcut.slice(5) : shortcut).toLowerCase();
  if (e.key.toLowerCase() !== key) return false;
  return ctrl ? e.ctrlKey || e.metaKey : !e.ctrlKey && !e.metaKey && !e.altKey;
}
