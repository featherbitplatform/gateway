/**
 * Application entry point: resolves the color theme (dark-first, with a
 * `localStorage` override and an OS light-mode fallback) before first
 * paint, then mounts {@link App} into `#root` under React StrictMode.
 *
 * @module main
 */
import { StrictMode } from 'react';
import { createRoot } from 'react-dom/client';
import './index.css';
import App from './App';

// Dark-first: dark is the default theme, light is opt-in
const savedTheme = localStorage.getItem('theme');
if (savedTheme === 'light' || (!savedTheme && window.matchMedia('(prefers-color-scheme: light)').matches)) {
  document.documentElement.setAttribute('data-theme', 'light');
}

createRoot(document.getElementById('root')!).render(
  <StrictMode>
    <App />
  </StrictMode>
);
