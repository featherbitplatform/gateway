/**
 * Registry letting the canvas expose actions to the App-level command
 * palette without lifting its internal state (drawer visibility, save
 * handler) into App. GraphCanvas registers on mount and unregisters on
 * unmount.
 *
 * `has()` is *not* "is the editor open?": the registration hooks sit above
 * GraphCanvas's `if (!policy)` early return (hook order must not vary across
 * renders), so a canvas mounted with `policy={null}` — App's state whenever
 * nothing is selected — still registers. Commands that require a real graph
 * pair `has()` with `CommandContext.editorOpen` (see commands.ts).
 *
 * This registry is intentionally *not* reactive: `register`/its returned
 * cleanup are side-effectful mutations of a plain `Map` ref, and `invoke`/
 * `has` are live reads of that ref — none of them trigger a re-render, and
 * none of them need to. `has(id)` reflects whatever is registered at the
 * moment some *other* render calls it (e.g. CommandPalette re-evaluating
 * `when()` on every keystroke, or App re-rendering for any of its own
 * reasons); nothing here re-renders App or the palette just because a
 * registration changed. An earlier version bumped a `useState` counter on
 * every register/unregister specifically to force such a re-render, but
 * paired with a memoized context value (needed to stop the registration
 * effect in `useRegisterEditorAction` from re-firing on every provider
 * render — see git history) that counter became a state update with no
 * observer: the memoized value and `children` are both referentially
 * stable, so React bails out of re-rendering anything below the Provider
 * on a bump. It was dead weight, not a bug fix, so it's gone.
 *
 * @module editorActions
 */
import { createContext, useContext, useEffect, useMemo } from 'react';

export interface EditorActionsValue {
  invoke: (id: string) => void;
  has: (id: string) => boolean;
  register: (id: string, fn: () => void) => () => void;
}

/** Shared with the provider component (components/EditorActionsProvider.tsx),
 *  which lives in its own file so this one exports no components and stays
 *  fast-refresh clean. */
export const Ctx = createContext<EditorActionsValue | null>(null);

/** Registers one action for as long as the calling component is mounted. */
export function useRegisterEditorAction(id: string, fn: () => void): void {
  const ctx = useContext(Ctx);
  useEffect(() => ctx?.register(id, fn), [ctx, id, fn]);
}

/**
 * Palette-side accessor.
 *
 * Memoized on `ctx` (itself stable, see the Provider) so the returned object
 * and its two functions keep their identity across renders — App feeds them
 * into a memoized CommandContext that keys the global keydown effect.
 */
export function useEditorActions(): { invoke: (id: string) => void; has: (id: string) => boolean } {
  const ctx = useContext(Ctx);
  return useMemo(
    () => ({ invoke: (id: string) => ctx?.invoke(id), has: (id: string) => ctx?.has(id) ?? false }),
    [ctx]
  );
}
