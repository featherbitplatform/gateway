/**
 * Dark/light theme switch for the admin UI. Themes are driven by the
 * `data-theme` attribute on documentElement (absent = dark, "light" = light)
 * and persisted in localStorage under the 'theme' key.
 *
 * @module components/ThemeToggle
 */
import { useState, useEffect } from 'react';
import { Moon, Sun } from 'lucide-react';
import { isDarkTheme, toggleTheme, THEME_CHANGE_EVENT } from '../theme';

/**
 * Icon button that toggles between dark and light mode.
 *
 * The flip itself (DOM attribute + localStorage) lives in `theme.ts`, shared
 * with the command palette's `toggle-theme` action, so the two can't drift.
 * This component only tracks the current value for its icon, seeded from
 * the DOM on mount and resynced whenever `theme.ts` reports a change —
 * including one triggered elsewhere, such as from the palette.
 *
 * @remarks main.tsx runs the same dark-first bootstrap logic before React
 * mounts, so the page paints in the persisted theme without a flash; keep
 * the two in sync.
 */
export function ThemeToggle() {
  const [dark, setDark] = useState(isDarkTheme);

  useEffect(() => {
    const onThemeChange = () => setDark(isDarkTheme());
    window.addEventListener(THEME_CHANGE_EVENT, onThemeChange);
    return () => window.removeEventListener(THEME_CHANGE_EVENT, onThemeChange);
  }, []);

  return (
    <button
      onClick={toggleTheme}
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
      title={dark ? 'Switch to light mode' : 'Switch to dark mode'}
      aria-label={dark ? 'Switch to light mode' : 'Switch to dark mode'}
    >
      {dark ? <Sun size={15} /> : <Moon size={15} />}
    </button>
  );
}
