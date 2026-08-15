/**
 * Persisted "show port names" preference for the node editor.
 *
 * Mirrors the ThemeToggle pattern: state seeded from localStorage, written
 * back on every change. Default is visible — port names were previously
 * discoverable only by hovering a handle.
 *
 * @module usePortNames
 */
import { useState, useEffect, useCallback } from 'react';

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

  // Stable identity: this toggle is a dependency of App's memoized
  // CommandContext, which in turn keys the global keydown effect. A fresh
  // arrow every render would resubscribe that listener on every render.
  const toggle = useCallback(() => setShow((v) => !v), []);

  return [show, toggle];
}
