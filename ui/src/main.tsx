/**
 * Application entry point: resolves the color theme (dark-first, with a
 * `localStorage` override and an OS light-mode fallback) before first
 * paint, then mounts {@link App} into `#root` under React StrictMode.
 *
 * {@link EditorActionsProvider} wraps `App` here (rather than inside it)
 * because `App` itself calls `useEditorActions()` to bridge the command
 * palette to canvas-owned actions — the provider has to sit above every
 * consumer of that context.
 *
 * @module main
 */
import { StrictMode } from 'react';
import { createRoot } from 'react-dom/client';
import './index.css';
import App from './App';
import { EditorActionsProvider } from './components/EditorActionsProvider';

// Dark-first: dark is the default theme, light is opt-in
const savedTheme = localStorage.getItem('theme');
if (savedTheme === 'light' || (!savedTheme && window.matchMedia('(prefers-color-scheme: light)').matches)) {
  document.documentElement.setAttribute('data-theme', 'light');
}

createRoot(document.getElementById('root')!).render(
  <StrictMode>
    <EditorActionsProvider>
      <App />
    </EditorActionsProvider>
  </StrictMode>
);
