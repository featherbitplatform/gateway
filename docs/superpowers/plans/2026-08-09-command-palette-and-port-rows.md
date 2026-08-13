# Command Palette & Port Row Labels Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make port names readable on the canvas (labeled port rows inside each node, default on) and give the editor a searchable command palette that doubles as the keyboard-shortcut catalog.

**Architecture:** A `showPortNames` preference (localStorage, same pattern as `ThemeToggle`) switches `PluginNode` between labeled port rows and today's compact handles. A declarative command registry (`commands.ts`) feeds both a `Ctrl+K` palette modal and a global key handler mounted in `App`. Two actions belong to the canvas rather than App (add plugin, save graph), so `GraphCanvas` registers them into a small React-context action bridge that the palette invokes; `when()` predicates hide unavailable actions and make their keys inert.

**Tech Stack:** React 18 + TypeScript, @xyflow/react, lucide-react icons, Vite build embedded via rust-embed; Playwright for behavioral coverage (the UI has no unit-test runner — e2e specs are the tests).

**Specs:** `docs/superpowers/specs/2026-08-08-command-palette-design.md` and `docs/superpowers/specs/2026-08-08-port-row-labels-design.md` (read both before starting).

## Global Constraints

- Branch: `feature/named-output-ports` (already checked out; this work refines unmerged PR #13). Conventional Commits, NO Co-Authored-By trailer.
- The UI has **no unit test framework**. TDD here means: write the Playwright scenario first, run it, watch it fail, then implement. Every task's gate is `cd ui && npm run build` (typecheck) → `cargo build --release` (embeds the bundle) → `cd e2e && npm test`.
- Full e2e suite must be green at every task boundary (currently 104/104). Scenario IDs continue each spec file's numbering — read `e2e/E2E_TESTBOOK.md` for the next free id per prefix, and add a testbook row in the same task that adds the scenario.
- Port semantics, catalog API, engine, and edge format do NOT change. Handle ids stay equal to port names; `title` tooltips stay; kind colors stay (success `--success`, outcome `--accent`, error `--error`).
- Use existing design tokens (`--text-2xs`, `--text-muted`, `--surface-raised`, `--radius-sm`, `--font-mono`) — no hardcoded colors or px fonts.
- `showPortNames` default is **visible (true)**.
- Single-letter shortcuts must be inert while an `input`, `textarea`, or contenteditable has focus.
- Run `graphify update .` before committing (if unavailable, note it and continue).

---

### Task 1: Port rows in PluginNode, behind a persisted preference

**Files:**
- Create: `ui/src/usePortNames.ts`
- Modify: `ui/src/components/PluginNode.tsx` (outputs section ~lines 176-221; `showLabels` const ~line 105)
- Modify: `ui/src/components/GraphCanvas.tsx` (node-data construction — grep for `pluginType:` inside the policy→nodes conversion and the three `handleAdd*` functions; also the `PluginNodeData` fan-out)
- Modify: `ui/src/components/PluginNode.tsx` interface `PluginNodeData` (add the flag)
- Test: `e2e/tests/editor-roundtrip.spec.ts` (new scenario appended, following that file's existing harness idioms)
- Modify: `e2e/E2E_TESTBOOK.md`

**Interfaces:**
- Produces (later tasks rely on these exact names):
  - `usePortNames(): [boolean, () => void]` from `ui/src/usePortNames.ts` — `[showPortNames, togglePortNames]`, persisted under localStorage key `portNames` (`'true'` / `'false'`), default `true`.
  - `PluginNodeData.showPortNames?: boolean` — read by `PluginNode`; `undefined` is treated as `true`.

- [ ] **Step 1: Write the failing e2e scenario**

Append to `e2e/tests/editor-roundtrip.spec.ts`, reusing that file's existing setup (its `test.describe`, its admin-UI navigation helper, and its policy fixtures — copy the idiom from the neighbouring `E2E-UI-13` test rather than inventing helpers):

```ts
  /**
   * E2E-UI-16: port names are rendered as labeled rows inside the node by
   * default (previously discoverable only via hover tooltip).
   */
  test('E2E-UI-16: node shows labeled port rows by default', async ({ page }) => {
    // Navigate to the echo policy in the editor (same idiom as E2E-UI-13).
    const corsNode = page.locator('.react-flow__node', { hasText: 'cors' }).first();
    await expect(corsNode).toBeVisible();

    // Three declared outputs, each rendered as a visible text label.
    await expect(corsNode.getByText('success', { exact: true })).toBeVisible();
    await expect(corsNode.getByText('preflight', { exact: true })).toBeVisible();
    await expect(corsNode.getByText('error', { exact: true })).toBeVisible();
    // The input port is labeled too.
    await expect(corsNode.getByText('in', { exact: true })).toBeVisible();

    // Handles still carry their ids and tooltips (unchanged contract).
    await expect(corsNode.locator('[data-handleid="preflight"]')).toHaveCount(1);
    await expect(corsNode.locator('[data-handleid="preflight"]')).toHaveAttribute(
      'title',
      /preflight —/
    );
  });
```

- [ ] **Step 2: Run it to verify it fails**

```bash
cargo build --release && cd e2e && npx playwright test editor-roundtrip -g "E2E-UI-16"
```
Expected: FAIL — the port names render only as hover tooltips today (and only as floating text for nodes with >2 outputs), so `getByText('preflight')` finds nothing visible.

- [ ] **Step 3: Create the preference hook**

`ui/src/usePortNames.ts`:

```ts
/**
 * Persisted "show port names" preference for the node editor.
 *
 * Mirrors the ThemeToggle pattern: state seeded from localStorage, written
 * back on every change. Default is visible — port names were previously
 * discoverable only by hovering a handle.
 *
 * @module usePortNames
 */
import { useState, useEffect } from 'react';

/** localStorage key holding `'true'` | `'false'`. */
const STORAGE_KEY = 'portNames';

/**
 * Returns the current preference and a toggle function.
 *
 * @returns `[showPortNames, togglePortNames]`
 */
export function usePortNames(): [boolean, () => void] {
  const [show, setShow] = useState(() => localStorage.getItem(STORAGE_KEY) !== 'false');

  useEffect(() => {
    localStorage.setItem(STORAGE_KEY, show ? 'true' : 'false');
  }, [show]);

  return [show, () => setShow((v) => !v)];
}
```

- [ ] **Step 4: Render port rows in PluginNode**

In `ui/src/components/PluginNode.tsx`: add `showPortNames?: boolean;` to the `PluginNodeData` interface (doc comment: "When false, ports render as bare handles with hover tooltips; default true renders labeled rows."). Delete the `showLabels` const (line ~105) and replace the whole outputs/input JSX block (~lines 176-221) with:

```tsx
      {/* Ports. With names shown, each port is a labeled row and its handle
          sits at the row's vertical centre — @xyflow anchors edges off DOM
          layout, so rows and handles stay aligned however tall the header
          and body grow. With names hidden, handles keep the previous
          evenly-spaced absolute placement. */}
      {showNames ? (
        <div style={{ borderTop: '1px solid var(--border)', padding: '4px 0' }}>
          {!isEntry && (
            <div style={portRowStyle('left')}>
              <Handle
                type="target"
                position={Position.Left}
                id="in"
                title={nodeData.ports?.input ?? undefined}
                style={{ ...handleStyle('var(--accent)'), top: '50%' }}
              />
              in
            </div>
          )}
          {outputs.map((p) => (
            <div key={p.name} style={portRowStyle('right')}>
              {p.name}
              <Handle
                type="source"
                position={Position.Right}
                id={p.name}
                title={`${p.name} — ${p.description}`}
                style={{ ...handleStyle(PORT_COLOR[p.kind]), top: '50%' }}
              />
            </div>
          ))}
        </div>
      ) : (
        <>
          {!isEntry && (
            <Handle
              type="target"
              position={Position.Left}
              id="in"
              title={nodeData.ports?.input ?? undefined}
              style={handleStyle('var(--accent)')}
            />
          )}
          {outputs.map((p, i) => (
            <Handle
              key={p.name}
              type="source"
              position={Position.Right}
              id={p.name}
              title={`${p.name} — ${p.description}`}
              style={{
                ...handleStyle(PORT_COLOR[p.kind]),
                top: `${outputs.length === 1 ? 50 : 25 + (i * 50) / (outputs.length - 1)}%`,
              }}
            />
          ))}
        </>
      )}
```

with these additions near the top of the module (beside `handleStyle`):

```tsx
/** One port row: relative so its Handle anchors to the row, not the node. */
const portRowStyle = (align: 'left' | 'right'): React.CSSProperties => ({
  position: 'relative',
  height: 18,
  lineHeight: '18px',
  padding: '0 10px',
  textAlign: align,
  fontFamily: 'var(--font-mono)',
  fontSize: 'var(--text-2xs)',
  color: 'var(--text-muted)',
  whiteSpace: 'nowrap',
});
```

and inside the component, next to the existing `outputs` const:

```tsx
  const showNames = nodeData.showPortNames !== false;
```

- [ ] **Step 5: Thread the preference through GraphCanvas**

In `ui/src/components/GraphCanvas.tsx`: call `const [showPortNames] = usePortNames();` in the component body (import from `../usePortNames`), and set `showPortNames` in every place that builds `PluginNodeData` — the policy→nodes conversion plus `handleAddPlugin` / `handleAddScript` / `handleAddSupernode` (grep `pluginType:` to find all four). Because node `data` is captured at conversion time, also add a `useEffect` that rewrites existing nodes when the preference flips:

```tsx
  useEffect(() => {
    setNodes((nds) =>
      nds.map((n) => ({ ...n, data: { ...n.data, showPortNames } }))
    );
  }, [showPortNames, setNodes]);
```

- [ ] **Step 6: Check auto-layout spacing for taller nodes**

Nodes now grow ~18px per port (a 4-output `workflow` node gains ~90px). Read the auto-layout constants in `GraphCanvas.tsx` (grep for the vertical spacing used when a policy has no saved positions) and raise the row spacing if a 4-port node would overlap its neighbour. Load a policy with no positions and confirm visually via the e2e run in Step 7 (no overlapping-node failures).

- [ ] **Step 7: Run the full gate**

```bash
cd ui && npm run build && cd .. && cargo build --release && cd e2e && npm test
```
Expected: 105/105 — the new E2E-UI-16 passes and every pre-existing scenario stays green. If `E2E-UI-15`'s edge-click (geometry-derived midpoint, already documented as layout-dependent) fails because taller nodes moved the path, adjust that click to the new geometry and say so in your report — do not weaken its assertions.

- [ ] **Step 8: Catalog the scenario**

Add the `E2E-UI-16` row to the UI table in `e2e/E2E_TESTBOOK.md` ("Node shows labeled port rows by default → three labeled output rows + labeled input, handle ids/tooltips unchanged").

- [ ] **Step 9: Commit**

```bash
git add ui/ e2e/
git commit -m "feat(ui): render ports as labeled rows inside nodes, behind a persisted preference"
```

---

### Task 2: Command registry, palette modal, and App-level shortcuts

**Files:**
- Create: `ui/src/commands.ts`
- Create: `ui/src/components/CommandPalette.tsx`
- Modify: `ui/src/App.tsx` (mount palette + global keydown; pass handlers)
- Test: `e2e/tests/command-palette.spec.ts` (new file)
- Modify: `e2e/E2E_TESTBOOK.md`

**Interfaces:**
- Consumes: `usePortNames()` from Task 1.
- Produces (Task 3 extends these):
  - `ui/src/commands.ts`: `interface Command { id: string; title: string; shortcut?: string; when?: (ctx: CommandContext) => boolean; run: (ctx: CommandContext) => void }`, `interface CommandContext { editorOpen: boolean; hasSelection: boolean; togglePortNames: () => void; createRoute: () => void; createSupernode: () => void; createPluginConfig: () => void; viewYaml: () => void; reloadConfig: () => void; toggleTheme: () => void; invokeEditorAction: (id: string) => void; hasEditorAction: (id: string) => boolean }`, and `buildCommands(): Command[]`.
  - `ui/src/components/CommandPalette.tsx`: `export function CommandPalette({ open, onClose, ctx }: { open: boolean; onClose: () => void; ctx: CommandContext }): JSX.Element | null`.
  - `matchesShortcut(e: KeyboardEvent, shortcut: string): boolean` exported from `commands.ts`.

- [ ] **Step 1: Write the failing e2e scenarios**

Create `e2e/tests/command-palette.spec.ts`, copying the harness idiom (gateway boot, admin UI navigation, auth) from `e2e/tests/editor-roundtrip.spec.ts`:

```ts
  /** E2E-UI-17: Ctrl+K opens the palette, which lists actions with shortcuts. */
  test('E2E-UI-17: command palette opens and lists actions with shortcuts', async ({ page }) => {
    await page.keyboard.press('Control+k');
    const palette = page.getByRole('dialog', { name: 'Command palette' });
    await expect(palette).toBeVisible();
    await expect(palette.getByText('Toggle port names')).toBeVisible();
    await expect(palette.getByText('New route')).toBeVisible();
    // Shortcut chips are rendered beside the titles.
    await expect(palette.locator('kbd', { hasText: 'P' }).first()).toBeVisible();
    // Filtering narrows the list.
    await page.keyboard.type('route');
    await expect(palette.getByText('New route')).toBeVisible();
    await expect(palette.getByText('Toggle port names')).toHaveCount(0);
    await page.keyboard.press('Escape');
    await expect(palette).toHaveCount(0);
  });

  /** E2E-UI-18: toggling port names hides the rows and survives a reload. */
  test('E2E-UI-18: port-name toggle persists across reload', async ({ page }) => {
    const corsNode = page.locator('.react-flow__node', { hasText: 'cors' }).first();
    await expect(corsNode.getByText('preflight', { exact: true })).toBeVisible();

    await page.keyboard.press('Control+k');
    await page.getByRole('dialog', { name: 'Command palette' }).getByText('Toggle port names').click();
    await expect(corsNode.getByText('preflight', { exact: true })).toHaveCount(0);
    // The handle itself survives — only the label is hidden.
    await expect(corsNode.locator('[data-handleid="preflight"]')).toHaveCount(1);

    await page.reload();
    const afterReload = page.locator('.react-flow__node', { hasText: 'cors' }).first();
    await expect(afterReload).toBeVisible();
    await expect(afterReload.getByText('preflight', { exact: true })).toHaveCount(0);
  });

  /** E2E-UI-19: a bare shortcut runs its action; typing in a field does not. */
  test('E2E-UI-19: bare shortcut opens the new-route dialog, inputs are exempt', async ({ page }) => {
    await page.keyboard.press('r');
    await expect(page.getByRole('dialog', { name: 'New route' })).toBeVisible();
    // Typing "r" inside the dialog's field must not re-trigger the shortcut.
    await page.getByLabel('Route name').fill('shortcut-probe');
    await expect(page.getByRole('dialog', { name: 'New route' })).toHaveCount(1);
    await page.keyboard.press('Escape');
  });
```

- [ ] **Step 2: Run to verify they fail**

```bash
cargo build --release && cd e2e && npx playwright test command-palette
```
Expected: all three FAIL — no palette exists, `Control+k` does nothing.

- [ ] **Step 3: Write the command registry**

`ui/src/commands.ts`:

```ts
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
      when: (c) => c.hasEditorAction('add-plugin'),
      run: (c) => c.invokeEditorAction('add-plugin'),
    },
    {
      id: 'save-graph',
      title: 'Save policy',
      shortcut: 'Ctrl+S',
      when: (c) => c.hasEditorAction('save-graph'),
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
```

- [ ] **Step 4: Write the palette modal**

`ui/src/components/CommandPalette.tsx`:

```tsx
/**
 * Searchable, executable catalog of editor actions (Ctrl+K).
 *
 * Renders the registry from commands.ts: typing filters by title, arrows
 * move the selection, Enter runs the action and closes. Each row shows the
 * action's shortcut, so the palette is also the shortcut reference.
 *
 * @module components/CommandPalette
 */
import { useEffect, useMemo, useState } from 'react';
import { buildCommands, type CommandContext } from '../commands';

/** Props for {@link CommandPalette}. */
interface CommandPaletteProps {
  open: boolean;
  onClose: () => void;
  ctx: CommandContext;
}

/** Modal palette; renders nothing when closed. */
export function CommandPalette({ open, onClose, ctx }: CommandPaletteProps) {
  const [query, setQuery] = useState('');
  const [active, setActive] = useState(0);

  // Only available actions are listed; `when` false also makes keys inert.
  const matches = useMemo(() => {
    const q = query.trim().toLowerCase();
    return buildCommands()
      .filter((c) => (c.when ? c.when(ctx) : true))
      .filter((c) => c.title.toLowerCase().includes(q));
  }, [query, ctx]);

  // Reset per opening, and keep the cursor inside the filtered list.
  useEffect(() => {
    if (open) {
      setQuery('');
      setActive(0);
    }
  }, [open]);
  useEffect(() => setActive(0), [query]);

  if (!open) return null;

  const run = (index: number) => {
    const cmd = matches[index];
    if (!cmd) return;
    onClose();
    cmd.run(ctx);
  };

  return (
    <div
      onClick={onClose}
      style={{
        position: 'fixed',
        inset: 0,
        background: 'rgba(0,0,0,0.45)',
        display: 'flex',
        justifyContent: 'center',
        alignItems: 'flex-start',
        paddingTop: '12vh',
        zIndex: 100,
      }}
    >
      <div
        role="dialog"
        aria-label="Command palette"
        onClick={(e) => e.stopPropagation()}
        style={{
          width: 460,
          maxWidth: '90vw',
          background: 'var(--surface-raised)',
          border: '1px solid var(--border)',
          borderRadius: 'var(--radius-md)',
          boxShadow: 'var(--shadow-md)',
          overflow: 'hidden',
        }}
      >
        <input
          autoFocus
          value={query}
          placeholder="Type a command…"
          onChange={(e) => setQuery(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === 'Escape') {
              e.preventDefault();
              onClose();
            } else if (e.key === 'ArrowDown') {
              e.preventDefault();
              setActive((i) => Math.min(i + 1, matches.length - 1));
            } else if (e.key === 'ArrowUp') {
              e.preventDefault();
              setActive((i) => Math.max(i - 1, 0));
            } else if (e.key === 'Enter') {
              e.preventDefault();
              run(active);
            }
          }}
          style={{
            width: '100%',
            padding: '10px 12px',
            border: 'none',
            borderBottom: '1px solid var(--border)',
            background: 'transparent',
            color: 'var(--text-primary)',
            fontFamily: 'var(--font-sans)',
            fontSize: 'var(--text-sm)',
            outline: 'none',
          }}
        />
        <div style={{ maxHeight: 320, overflowY: 'auto' }}>
          {matches.length === 0 && (
            <div style={{ padding: '10px 12px', color: 'var(--text-muted)', fontSize: 'var(--text-xs)' }}>
              No matching command
            </div>
          )}
          {matches.map((cmd, i) => (
            <div
              key={cmd.id}
              onClick={() => run(i)}
              onMouseEnter={() => setActive(i)}
              className="flex items-center justify-between cursor-pointer"
              style={{
                padding: '8px 12px',
                background: i === active ? 'var(--surface-hover)' : 'transparent',
                color: 'var(--text-primary)',
                fontSize: 'var(--text-sm)',
              }}
            >
              <span>{cmd.title}</span>
              {cmd.shortcut && (
                <kbd
                  style={{
                    fontFamily: 'var(--font-mono)',
                    fontSize: 'var(--text-2xs)',
                    color: 'var(--text-muted)',
                    border: '1px solid var(--border)',
                    borderRadius: 'var(--radius-sm)',
                    padding: '1px 5px',
                  }}
                >
                  {cmd.shortcut}
                </kbd>
              )}
            </div>
          ))}
        </div>
      </div>
    </div>
  );
}
```

If `--font-sans` or `--shadow-md` are not the tokens this codebase uses, grep `ui/src/index.css` for the actual names and substitute — do not hardcode values.

- [ ] **Step 5: Mount it in App with the global key handler**

In `ui/src/App.tsx`: add `const [paletteOpen, setPaletteOpen] = useState(false);`, `const [showPortNames, togglePortNames] = usePortNames();`, build `const commandCtx: CommandContext = { ... }` from the existing handlers (`handleCreateRoute`, `handleCreateSupernode`, `handleCreatePluginConfig`, `handleViewYaml`, `handleReload`), and render `<CommandPalette open={paletteOpen} onClose={() => setPaletteOpen(false)} ctx={commandCtx} />`. Add the listener:

```tsx
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if ((e.ctrlKey || e.metaKey) && e.key.toLowerCase() === 'k') {
        e.preventDefault();
        setPaletteOpen((v) => !v);
        return;
      }
      if (paletteOpen) return; // the palette owns keys while it is open
      const t = e.target as HTMLElement | null;
      if (t && (t.tagName === 'INPUT' || t.tagName === 'TEXTAREA' || t.isContentEditable)) return;
      for (const cmd of buildCommands()) {
        if (!cmd.shortcut || !matchesShortcut(e, cmd.shortcut)) continue;
        if (cmd.when && !cmd.when(commandCtx)) continue;
        e.preventDefault();
        cmd.run(commandCtx);
        return;
      }
    };
    window.addEventListener('keydown', onKey);
    return () => window.removeEventListener('keydown', onKey);
  }, [paletteOpen, commandCtx]);
```

Theme toggling currently lives inside `ThemeToggle`'s local state; for `toggleTheme` in the context, flip `data-theme` + the `theme` localStorage key with the same logic the component uses (extract a tiny `toggleTheme()` helper into `ui/src/theme.ts` and have `ThemeToggle` call it too, so the two cannot drift).

**Note:** `add-plugin` and `save-graph` return `false` from `hasEditorAction` until Task 3 lands the bridge — they are correctly hidden until then.

- [ ] **Step 6: Run the gate**

```bash
cd ui && npm run build && cd .. && cargo build --release && cd e2e && npm test
```
Expected: 108/108 — E2E-UI-17/18/19 pass, everything else stays green.

- [ ] **Step 7: Catalog the scenarios**

Add rows for E2E-UI-17, E2E-UI-18, E2E-UI-19 to `e2e/E2E_TESTBOOK.md`.

- [ ] **Step 8: Commit**

```bash
git add ui/ e2e/
git commit -m "feat(ui): command palette with searchable action list and keyboard shortcuts"
```

---

### Task 3: Editor-action bridge for canvas-owned commands

**Files:**
- Create: `ui/src/editorActions.tsx`
- Modify: `ui/src/App.tsx` (wrap in provider; wire `invokeEditorAction`/`hasEditorAction`)
- Modify: `ui/src/components/GraphCanvas.tsx` (register both actions; add the palette toolbar button next to `ThemeToggle` ~line 691)
- Test: `e2e/tests/command-palette.spec.ts` (append), `e2e/E2E_TESTBOOK.md`

**Interfaces:**
- Consumes: `CommandContext` from Task 2.
- Produces: `ui/src/editorActions.tsx` exporting `EditorActionsProvider({ children })`, `useRegisterEditorAction(id: string, fn: () => void): void`, and `useEditorActions(): { invoke: (id: string) => void; has: (id: string) => boolean }`.

- [ ] **Step 1: Write the failing e2e scenarios**

Append to `e2e/tests/command-palette.spec.ts`:

```ts
  /** E2E-UI-20: canvas-owned actions appear only with the editor open. */
  test('E2E-UI-20: "A" opens the plugin drawer from the canvas', async ({ page }) => {
    // With a policy open in the editor:
    await page.keyboard.press('a');
    await expect(page.getByPlaceholder('Search plugins')).toBeVisible();
    await page.keyboard.press('Escape');

    await page.keyboard.press('Control+k');
    const palette = page.getByRole('dialog', { name: 'Command palette' });
    await expect(palette.getByText('Add plugin to canvas')).toBeVisible();
    await expect(palette.getByText('Save policy')).toBeVisible();
  });
```

(Use the actual drawer search placeholder — grep `PluginDrawer.tsx` for its `placeholder` and match it exactly.)

- [ ] **Step 2: Run to verify it fails**

```bash
cargo build --release && cd e2e && npx playwright test command-palette -g "E2E-UI-20"
```
Expected: FAIL — `a` does nothing and both actions are hidden (`hasEditorAction` is stubbed `false`).

- [ ] **Step 3: Write the bridge**

`ui/src/editorActions.tsx`:

```tsx
/**
 * Registry letting the canvas expose actions to the App-level command
 * palette without lifting its internal state (drawer visibility, save
 * handler) into App. GraphCanvas registers on mount and unregisters on
 * unmount, so `has()` doubles as "is the editor open?".
 *
 * @module editorActions
 */
import { createContext, useContext, useCallback, useEffect, useRef, useState, type ReactNode } from 'react';

interface EditorActionsValue {
  invoke: (id: string) => void;
  has: (id: string) => boolean;
  register: (id: string, fn: () => void) => () => void;
}

const Ctx = createContext<EditorActionsValue | null>(null);

/** Wraps the app so canvas actions are reachable from the palette. */
export function EditorActionsProvider({ children }: { children: ReactNode }) {
  const actions = useRef(new Map<string, () => void>());
  // Bumped on register/unregister so consumers re-evaluate `has()`.
  const [, bump] = useState(0);

  const register = useCallback((id: string, fn: () => void) => {
    actions.current.set(id, fn);
    bump((n) => n + 1);
    return () => {
      actions.current.delete(id);
      bump((n) => n + 1);
    };
  }, []);

  const invoke = useCallback((id: string) => actions.current.get(id)?.(), []);
  const has = useCallback((id: string) => actions.current.has(id), []);

  return <Ctx.Provider value={{ invoke, has, register }}>{children}</Ctx.Provider>;
}

/** Registers one action for as long as the calling component is mounted. */
export function useRegisterEditorAction(id: string, fn: () => void): void {
  const ctx = useContext(Ctx);
  useEffect(() => ctx?.register(id, fn), [ctx, id, fn]);
}

/** Palette-side accessor. */
export function useEditorActions(): { invoke: (id: string) => void; has: (id: string) => boolean } {
  const ctx = useContext(Ctx);
  return { invoke: (id) => ctx?.invoke(id), has: (id) => ctx?.has(id) ?? false };
}
```

- [ ] **Step 4: Register the two canvas actions**

In `GraphCanvas.tsx`, after the existing handlers:

```tsx
  useRegisterEditorAction('add-plugin', useCallback(() => {
    setSelectedNodeId(null);
    setDrawerOpen(true);
  }, []));
  useRegisterEditorAction('save-graph', handleSave); // the existing save handler — grep for the Save Policy button's onClick
```

Wrap `handleSave` in `useCallback` if it is not already stable, so the registration effect does not re-run every render.

- [ ] **Step 5: Wire App and add the toolbar button**

In `App.tsx`: wrap the rendered tree in `<EditorActionsProvider>` (in `main.tsx` if App itself needs the hook — put the provider above whatever calls `useEditorActions`), then replace the stubs: `const editorActions = useEditorActions();` and set `invokeEditorAction: editorActions.invoke, hasEditorAction: editorActions.has` in `commandCtx`.

In `GraphCanvas.tsx` beside `<ThemeToggle />` (~line 691), add a palette button. Do **not** simulate a keypress — add an `onOpenPalette?: () => void` prop to `GraphCanvasProps`, pass `() => setPaletteOpen(true)` from App, and call it on click:

```tsx
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
```

(`Command` from `lucide-react`, added to that file's existing icon import.)

- [ ] **Step 6: Run the gate**

```bash
cd ui && npm run build && cd .. && cargo build --release && cd e2e && npm test
```
Expected: 109/109.

- [ ] **Step 7: Catalog and commit**

Add the E2E-UI-20 row to `e2e/E2E_TESTBOOK.md`, then:

```bash
git add ui/ e2e/
git commit -m "feat(ui): expose canvas actions to the command palette"
```

---

### Task 4: Refresh screenshots and document the palette

**Files:**
- Modify: `website/screenshots/*.png` (regenerated), `website/docs/concepts/error-handling.md:12-13`, `website/docs/concepts/policies-and-graphs.md:12-13`
- Modify: `website/docs/concepts/policies-and-graphs.md` (editor section — document the palette + port-name toggle)
- Modify: `CLAUDE.md` (UI feature bullet)

**Interfaces:**
- Consumes: the shipped UI from Tasks 1-3.

- [ ] **Step 1: Regenerate the screenshots**

```bash
cargo build --release
cd website/screenshots && node capture.mjs
```
(Read `capture.mjs`'s header for any prerequisites — it boots the gateway and drives a browser.) The new captures show labeled port rows, including the `denied`/`limited` edges the previous PNGs predate.

- [ ] **Step 2: Retire the stale-capture disclaimers**

In `website/docs/concepts/error-handling.md:12-13` and `website/docs/concepts/policies-and-graphs.md:12-13`, delete the "(This capture predates the outcome ports, so those two edges are not drawn.)" sentences and rewrite each alt text + caption to describe what the fresh capture actually shows (verify against the new PNG rather than assuming).

- [ ] **Step 3: Document the palette**

Add a short "Command palette" subsection to the editor part of `website/docs/concepts/policies-and-graphs.md`: `Ctrl+K` opens it, it lists every action with its shortcut, and port names can be toggled from it (default on, persisted per browser). Include the v1 shortcut table (P, R, S, C, A, Ctrl+S, Y).

- [ ] **Step 4: Update CLAUDE.md**

In the UI/feature bullets, note that the node editor has a command palette (`Ctrl+K`) and toggleable port-name rows.

- [ ] **Step 5: Build the site**

```bash
cd website && npm run build
```
Expected: PASS (catches broken links/anchors).

- [ ] **Step 6: Commit**

```bash
git add website/ CLAUDE.md
git commit -m "docs: refresh editor screenshots and document the command palette"
```

---

## Execution order & checkpoints

Tasks are ordered 1 → 4. Tasks 1-3 each end with the full e2e suite green (105 → 108 → 109 scenarios). Task 2's `add-plugin`/`save-graph` commands are intentionally hidden until Task 3 supplies the bridge — a reviewer seeing them absent mid-plan is looking at correct behavior. After Task 4, re-read both specs against the shipped UI as a final acceptance pass.
