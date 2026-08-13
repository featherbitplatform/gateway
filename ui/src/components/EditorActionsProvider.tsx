/**
 * Provider half of the editor-action registry — see editorActions.ts for the
 * registry's design notes (why it is a non-reactive Map ref, why the context
 * value is memoized). Kept in its own file so editorActions.ts exports only
 * hooks and the app stays fast-refresh clean.
 *
 * @module components/EditorActionsProvider
 */
import { useCallback, useMemo, useRef, type ReactNode } from 'react';
import { Ctx } from '../editorActions';

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
