/**
 * Dark/light theme switch for the admin UI. Themes are driven by the
 * `data-theme` attribute on documentElement (absent = dark, "light" = light)
 * and persisted in localStorage under the 'theme' key.
 *
 * @module components/ThemeToggle
 */
import { useState, useEffect } from 'react';
import { Moon, Sun } from 'lucide-react';

/**
 * Icon button that toggles between dark and light mode.
 *
 * Initial state comes from localStorage 'theme' if set, otherwise from the OS
 * preference (dark-first: dark unless the OS explicitly prefers light). Each
 * change writes 'theme' back to localStorage and updates `data-theme` on the
 * root element: the attribute is removed for dark (the stylesheet's default)
 * and set to "light" for light mode.
 *
 * @remarks The same bootstrap logic runs in main.tsx before React mounts, so
 * the page paints in the persisted theme without a flash; keep the two in sync.
 */
export function ThemeToggle() {
  const [dark, setDark] = useState(() => {
    const saved = localStorage.getItem('theme');
    if (saved) return saved === 'dark';
    // Dark-first: default to dark unless the OS prefers light
    return !window.matchMedia('(prefers-color-scheme: light)').matches;
  });

  useEffect(() => {
    if (dark) {
      document.documentElement.removeAttribute('data-theme');
    } else {
      document.documentElement.setAttribute('data-theme', 'light');
    }
    localStorage.setItem('theme', dark ? 'dark' : 'light');
  }, [dark]);

  return (
    <button
      onClick={() => setDark(!dark)}
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
