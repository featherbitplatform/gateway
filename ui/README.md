# featherbit ui

The embedded admin UI for featherbit — a node-graph policy editor built with React 19,
TypeScript, Vite, and React Flow (`@xyflow/react`).

Styling follows the featherbit design system: dark-first ink surfaces, electric violet
accent (`#863bff`), IBM Plex Sans/Mono, and Lucide icons. Design tokens live in
`src/index.css`; per-plugin colors and icons in `src/pluginMeta.tsx`.

## Development

```bash
npm install
npm run dev        # dev server with HMR, proxies /api to the gateway admin port
npm run build      # emits dist/, embedded into the gateway binary by cargo build
npm run lint
```
