/**
 * Registry letting the canvas expose actions to the App-level command
 * palette without lifting its internal state (drawer visibility, save
 * handler) into App. GraphCanvas registers on mount and unregisters on
 * unmount, so `has()` doubles as "is the editor open?".
 *
 * @module editorActions
 */
import { createContext, useContext, useCallback, useEffect, useMemo, useRef, useState, type ReactNode } from 'react';

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

  // Memoized so the value's identity is stable across bump-triggered
  // re-renders: invoke/has/register never change, and without this the
  // context value would be a fresh object every render, which would make
  // useRegisterEditorAction's effect (keyed on `ctx`) re-run on every
  // provider render, re-bumping forever.
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
