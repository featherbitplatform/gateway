---
title: Web UI
description: The embedded node-graph policy editor — where it runs, the editing workflow, and headless operation.
---

import UiShot from '@site/src/components/UiShot';

featherbit ships a node-graph policy editor as a React SPA (React 19, TypeScript, Vite, React Flow) **embedded in the gateway binary** — no separate web server. Open the admin port in a browser:

```bash
open http://localhost:9090
```

<UiShot
  name="editor"
  alt="The featherbit editor: the route list on the left, the selected route's policy graph on the canvas."
  caption="The editor. Routes are listed on the left; selecting one opens its policy on the canvas. This is the same graph the gateway executes — not a diagram of it."
/>

## How it is served

The UI is compiled into the binary at build time (`ui/dist/`, embedded via `rust-embed`) and served by the admin server as the fallback for any path not matched by the admin API. Unknown paths fall back to `index.html` so client-side routes resolve within the SPA.

The static assets themselves are served **without** authentication; the SPA's own calls to the admin API carry HTTP Basic credentials (see [Admin API](./admin-api.md)).

## Editor workflow

1. **Select a route** from the sidebar to open its routing policy on the canvas.
2. **Add plugins** from the plugin drawer (**Add Node**). The palette is populated from `GET /api/plugins` and offers every node type the gateway can build.
3. **Wire nodes** by dragging from output ports to input ports — green ports are `success`, red ports are `error`. A complete pipeline runs from `listener.out` through the plugin chain to `client.in`.
4. **Configure a node** by clicking it: the inspector panel shows a schema-driven form for that plugin type's config keys. Types without a declared form get a raw-JSON config editor instead.
5. **Save Policy** to deploy: the UI writes the policy through the admin API, which validates, recompiles, and hot-swaps the route graphs — no restart.
6. **Toggle dark/light mode** with the theme button.

<UiShot
  name="plugin-drawer"
  alt="The Add Node drawer, listing the available plugin types with their icons and descriptions."
  caption="The plugin drawer lists every registered node type — proxying and transforms, auth and authz, traffic control, the loggers, tracing, and the serverless nodes."
/>

<UiShot
  name="node-inspector"
  alt="The node inspector showing the key-auth node's configuration form: node ID, header name, and valid keys."
  caption="Clicking a node opens its config form. These fields are the same keys the plugin's from_config accepts in YAML — the UI and the YAML are two views of one policy."
/>

Node positions on the canvas are stored in each node's `position` field in the policy; the graph engine ignores them, and they are omitted from serialized output when unset.

## Headless mode

The UI is optional. It is only a client of the admin API, and it edits exactly the same data that lives in `gateway.yaml` — a policy saved from the canvas and a policy written by hand in YAML are interchangeable. Everything the UI does can be done with the YAML files plus hot-reload, or with the [Admin API](./admin-api.md) directly.

Omitting the `admin` section from `system.yaml` disables the admin server entirely (no API, no UI); the data plane still runs from the YAML configuration.

## Building the UI

The gateway build embeds whatever is in `ui/dist/`, so build the frontend first:

```bash
cd ui && npm install && npm run build && cd ..
cargo build
```

:::caution
`cargo build` embeds `ui/dist/` as it finds it — it does **not** rebuild the frontend, and it will happily embed a stale bundle without a warning. After changing anything under `ui/src/`, re-run `npm run build` **before** `cargo build`, or the binary keeps serving the previous UI.
:::

For UI development, `npm run dev` starts a dev server with HMR that proxies `/api` to the gateway admin port.

## Screenshots in these docs

The UI screenshots on this site are captured from the real binary, not mocked up. `website/screenshots/capture.mjs` boots the gateway against a posed demo config, drives the editor with Playwright, and writes both light and dark variants to `static/img/ui/`:

```bash
cargo build --release
cd website && npx playwright install chromium && node screenshots/capture.mjs
```

Re-run it after a UI change so the docs images do not drift from the product.
