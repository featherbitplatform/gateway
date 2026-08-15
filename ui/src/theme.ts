/**
 * Single source of truth for the dark/light theme flip: the `data-theme`
 * attribute on `documentElement` and its `theme` localStorage key.
 *
 * ThemeToggle and the command palette's `toggle-theme` action both need to
 * flip the theme; if each kept its own copy of "is it dark" state, one could
 * flip the DOM/localStorage while the other's cached copy went stale (the
 * same class of bug `usePortNames` guards against for port-name visibility).
 * Centralizing the flip here, plus a change event mounted components can
 * listen for, keeps every caller in sync without lifting the state into App.
 *
 * @module theme
 */

/** Dispatched on `window` after every {@link toggleTheme} call. */
export const THEME_CHANGE_EVENT = 'featherbit:theme-change';

/**
 * True when the current theme is dark — the default; light mode sets
 * `data-theme="light"` on `documentElement` (see main.tsx's pre-mount
 * bootstrap, which resolves the initial value from localStorage/OS
 * preference before this module is ever consulted).
 */
export function isDarkTheme(): boolean {
  return document.documentElement.getAttribute('data-theme') !== 'light';
}

/**
 * Flips the theme: updates `data-theme`, persists the choice to
 * localStorage under `theme`, and fires {@link THEME_CHANGE_EVENT} so any
 * mounted UI (e.g. ThemeToggle's icon) can resync.
 */
export function toggleTheme(): void {
  const nextDark = !isDarkTheme();
  if (nextDark) {
    document.documentElement.removeAttribute('data-theme');
  } else {
    document.documentElement.setAttribute('data-theme', 'light');
  }
  localStorage.setItem('theme', nextDark ? 'dark' : 'light');
  window.dispatchEvent(new Event(THEME_CHANGE_EVENT));
}
