/**
 * Registry letting the canvas expose actions to the App-level command
 * palette without lifting its internal state (drawer visibility, save
 * handler) into App. GraphCanvas registers on mount and unregisters on
 * unmount, so `has()` doubles as "is the editor open?".
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
import { createContext, useContext, useCallback, useEffect, useMemo, useRef, type ReactNode } from 'react';

interface EditorActionsValue {
  invoke: (id: string) => void;
  has: (id: string) => boolean;
  register: (id: string, fn: () => void) => () => void;
}

const Ctx = createContext<EditorActionsValue | null>(null);

/** Wraps the app so canvas actions are reachable from the palette. */
export function EditorActionsProvider({ children }: { children: ReactNode }) {
  const actions = useRef(new Map<string, () => void>());

  const register = useCallback((id: string, fn: () => void) => {
    actions.current.set(id, fn);
    return () => {
      actions.current.delete(id);
    };
  }, []);

  const invoke = useCallback((id: string) => actions.current.get(id)?.(), []);
  const has = useCallback((id: string) => actions.current.has(id), []);

  // Memoized so the value's identity is stable across renders: invoke/has/
  // register never change, and without this the context value would be a
  // fresh object every render, which would make useRegisterEditorAction's
  // effect (keyed on `ctx`) re-run on every provider render.
  const value = useMemo(() => ({ invoke, has, register }), [invoke, has, register]);

  return <Ctx.Provider value={value}>{children}</Ctx.Provider>;
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
