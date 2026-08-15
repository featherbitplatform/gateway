# Command Palette & Keyboard Shortcuts — Design

**Date:** 2026-08-08
**Status:** Approved (Francesco, 2026-08-08)
**Motivation:** The editor has no keyboard shortcuts and no single place to
discover actions. A command palette provides both: a searchable, executable
catalog of every action, each showing its shortcut.

## Decision

A **command palette** (VS Code style): `Ctrl+K` opens a modal with a search
input; typing filters actions; `↑`/`↓` moves the selection, `Enter` runs it,
`Esc` closes. Each row shows the action title and its shortcut as a `kbd`
chip — the palette IS the shortcuts catalog. A small toolbar button also
opens it for discoverability.

## Components

- **`ui/src/commands.ts`** — the action registry:
  ```ts
  interface Command {
    id: string;                 // kebab-case, e.g. "toggle-port-names"
    title: string;              // "Toggle port names"
    shortcut?: string;          // display + binding, e.g. "P", "Ctrl+S"
    when?: (ctx: CommandContext) => boolean;  // hidden + inert when false
    run: (ctx: CommandContext) => void;
  }
  ```
  `CommandContext` carries the current view, whether a policy editor is
  open, and the existing App handlers — the registry never reaches into
  component internals.
- **`ui/src/components/CommandPalette.tsx`** — the modal; follows
  `PluginDrawer`'s search/list/Escape idioms and the app's design tokens.
- **Global key handling** — one `keydown` listener registered in `App`:
  `Ctrl+K` toggles the palette; single-letter shortcuts are suppressed
  while any `input`/`textarea`/contenteditable has focus; `Ctrl+S` calls
  `preventDefault()`. Actions whose `when()` is false are hidden from the
  palette and their shortcuts inert.

## V1 actions

| Action | Shortcut | Availability | Wires to |
| --- | --- | --- | --- |
| Toggle port names | `P` | always | `showPortNames` pref (see amendment in [[2026-08-08-port-row-labels-design]]) |
| New route | `R` | always | `handleCreateRoute` |
| New supernode | `S` | always | `handleCreateSupernode` |
| New shared plugin config | `C` | always | `handleCreatePluginConfig` |
| Add plugin to canvas | `A` | policy editor open | opens `PluginDrawer` |
| Save policy/graph | `Ctrl+S` | policy editor open | `handleSaveGraph` |
| View YAML | `Y` | selection with YAML view | `handleViewYaml` |
| Reload gateway config | — (palette only) | always | `handleReload` |
| Toggle theme | — (palette only) | always | theme toggle |

Adding future actions = one registry entry; the palette and shortcut
handling pick them up automatically.

## Port-names toggle

`showPortNames` persisted in localStorage (key alongside the theme pref),
**default `true`**. ON renders the port rows per
`2026-08-08-port-row-labels-design.md`; OFF renders today's compact handles
(hover tooltips remain). The value threads from App into `PluginNodeData`
(or a React context) so `PluginNode` switches rendering; toggling re-renders
live without reload.

## Testing

- `cd ui && npm run build`; `cargo build` embeds the fresh bundle.
- New e2e scenarios (+ testbook rows): `Ctrl+K` opens the palette and lists
  actions with `kbd` chips; executing "Toggle port names" hides the port
  rows (assert row-label absence) and the preference survives a page
  reload; `R` from the palette (and as a bare key) creates a route.
- Full e2e suite green; screenshot regeneration
  (`website/screenshots/capture.mjs`) with default-visible port rows, and
  removal of the "(This capture predates the outcome ports…)" caption
  disclaimers in the two concepts pages.

## Non-goals

- User-customizable keybindings.
- Fuzzy-ranking beyond simple substring/prefix filtering.
- Shortcuts inside modal dialogs other than the palette's own navigation.
- A shortcuts cheat-sheet separate from the palette.
