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
3. **Wire nodes** by dragging from output ports to input ports — green ports are `success`, red ports are `error`. A complete pipeline runs from `listener.out` through the plugin chain to `client.in`. Each output port carries exactly one edge — dragging from a port that is already wired **rewires** it to the new target (the engine [rejects fan-out at compile time](../concepts/policies-and-graphs.md#compilation-rules), so the canvas never lets a graph get into that shape). Inputs accept any number of edges — several paths may converge on the same node (fan-in). The one connection the canvas refuses outright is an edge that would close a **loop** (including a node wired to itself): policies must be acyclic, and the compiler rejects a cyclic graph on save.
4. **Configure a node** by clicking it: the inspector panel shows a schema-driven form for that plugin type's config keys. Types without a declared form get a raw-JSON config editor instead. Typing `{{` in a text field opens a suggestion popover for `{{namespace.path}}` templates ([reference](../reference/templates.md)) — full request/response/message/client context suggestions with live-value preview, plus environment-variable names, on fields the plugin actually renders through the template engine; everywhere else the same `{{`-triggered popover offers environment-variable names only (never values), inserted as `${NAME}` rather than `{{env.NAME}}` since those fields never get parsed as a template — only the older, universal `${NAME}` env substitution actually resolves there (see [Templates → exclusions](../reference/templates.md#exclusions)). The handful of fields that still support the legacy `$var` syntax additionally trigger a `$`/`${` popover. A "Context vars" button in the inspector header opens the full legend — every namespace, the environment names, and the legacy `$var` → `{{path}}` mapping — with live values inlined when [debug mode](./debugging.md) has a trace for the selected node.
5. **Preview a supernode in place**: a [supernode](../concepts/supernodes.md) instance carries an expand chevron in its header — click it to grow the node into a zoomed-out, read-only rendering of the definition's inner graph, floating above the neighboring nodes with the instance's edges still attached. Click again to fold it back. The preview is a glance, not an editor (editing a definition happens in the Supernodes library), and expansion is never saved into the policy. Several instances can be expanded at once.
6. **Save Policy** to deploy: the UI writes the policy through the admin API, which validates, recompiles, and hot-swaps the route graphs — no restart.
7. **Toggle dark/light mode** with the theme button.

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

## Building supernode definitions

Opening a [supernode](../concepts/supernodes.md) from the Supernodes section of the sidebar puts the canvas into definition mode: the plugin drawer hides `listener`, `client`, and `supernode` (a definition can't nest any of those) and adds an **Output port** entry instead. Dropping it prompts for a port name in a small dialog, validated against the [reserved boundary ids](../concepts/supernodes.md#named-output-ports) and the definition's existing node ids — each drop adds one more `type: output` boundary, so a definition can carry as many named exits as it needs.

An output boundary's Node ID field is editable — click it and the inspector shows a **Rename** button (`input` and `error` stay fixed, as the sole boundaries of their kind). Renaming an output boundary renames the port itself, since the node's id *is* the port name; the canvas rewrites every edge that referenced the old id to match.

Back on a policy canvas, a supernode instance node renders one port row per port its definition exposes — `success` (only if the definition has an `output`-id boundary), then any named output ports in the order they appear in the definition, then `error` — instead of the fixed success/error pair from before named output ports existed.

## Extracting a selection into a supernode

Instead of building a definition from scratch, a group of existing policy nodes can be lifted straight into a new supernode. Multi-select two or more nodes on a policy canvas (shift-click or box-select), then trigger extraction one of three ways: the toolbar's **Extract Supernode** button, **Extract selection as supernode…** on the right-click context menu, or the same entry in the Ctrl+K command palette. All three are disabled until the selection is eligible.

**Eligibility:**
- No `listener`, `client`, or `supernode` node in the selection (a definition can't nest any of those).
- Exactly one **entry** node — every edge coming in from outside the selection must land on the same node.
- At least one non-error exit edge leaving the selection.
- Every error exit edge leaving the selection must target the same outer node (an instance has one `error` port).
- No node in the selection has the id `input`, `output`, or `error` — those ids are reserved for supernode boundary nodes, and extraction refuses a selection that has one: "Rename node 'output' before extracting — that id is reserved for supernode boundary nodes".

A selection that fails any of these shows an error toast naming the problem instead of extracting.

**What gets derived:** the entry node is wired from a new `input` boundary; each non-error exit edge becomes its own `output`-type boundary named after its source port (`success`/`out` exits map to the `output`-id boundary, i.e. the instance's `success` port), with duplicate names deduped by a numeric suffix; every error exit collapses into the single `error` boundary. Internal edges and node configs carry over unchanged.

One dialog asks for the new definition's name. On confirm, the definition is created immediately through the Admin API and the selected nodes on the canvas are replaced by a single wired instance — the policy itself is not saved automatically, same as any other canvas edit, so **Save Policy** is still required to deploy it. If creating the definition fails, nothing on the canvas changes.

## Headless mode

The UI is optional. It is only a client of the admin API, and it edits exactly the same data that lives in `gateway.yaml` — a policy saved from the canvas and a policy written by hand in YAML are interchangeable. Everything the UI does can be done with the YAML files plus hot-reload, or with the [Admin API](./admin-api.md) directly.

Omitting the `admin` section from `system.yaml` disables the admin server entirely (no API, no UI); the data plane still runs from the YAML configuration.

There are two mechanisms to run without the UI:

1. **Runtime**: Set `admin.ui_enabled: false` in `system.yaml` keeps the admin REST API available but returns 404 for UI paths. A restart is required for this setting to take effect.
2. **Compile time**: The `-headless` Docker image tags (`latest-headless`, `edge-headless`, `X.Y.Z-headless`) omit the UI entirely at build time, reducing binary size and attack surface.

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
