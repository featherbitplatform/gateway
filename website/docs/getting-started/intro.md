---
title: Introduction
description: What featherbit is, how node-graph routing policies work, and when to use it.
---

featherbit is a lightweight, high-performance API gateway delivered as a **single Rust binary**. There is no external runtime to install: the data-plane server, the admin REST API, and the node-graph web editor are all served by the same executable.

## The graph is the pipeline

Most gateways configure request processing as an ordered list of middleware. featherbit instead models each route's processing logic as a **directed node graph**, called a *routing policy*:

- Each **node** is a plugin instance (proxy-rewrite, upstream, jwt-auth, rate-limit, a Lua script, ...).
- Each **edge** connects a node's output port to another node's input port. Every plugin node has a **success** port and an **error** port, so happy paths and failure paths are wired explicitly in the same graph.
- A [Context object](../concepts/context-object.md) — carrying the request, the response under construction, a free-form message map, and accumulated errors — flows through every node.

Execution starts at the [listener node](../concepts/listener-and-client.md), follows success or error edges node by node, and stops when the context reaches a terminal client node. Whatever is in `context.response` at that point is sent back to the caller.

## YAML and the visual editor are two views of the same data

A routing policy can be designed in the embedded web UI (a node-graph editor with a canvas, plugin drawer, and inspector) or written directly in `gateway.yaml`. Both read and write the **same serialized graph format** — nodes, edges, and per-node config. You can:

- design a policy visually, save it, and commit the resulting YAML to version control;
- edit the YAML by hand (or in CI) and see the same graph rendered in the editor;
- run fully headless with no UI at all — the YAML is the source of truth either way.

## Feature summary

| Area | What you get |
|---|---|
| Routing | Match rules on path, method, headers, and host; each route dispatches to a policy graph |
| Plugins | 80+ built-in node types (proxying, transforms, auth, authz, rate limiting, traffic control, logging, tracing, serverless) plus custom plugins in Lua |
| Protocols | HTTP/1.1 and HTTP/2 (ALPN over TLS, h2c on plaintext); WebSocket proxying, including RFC 8441 over HTTP/2; L4 TCP/UDP stream proxying with SNI routing — see [TLS](../guides/tls.md) and [Stream](../guides/stream.md) |
| TLS | Termination with hot-reloading certs, per-hostname SNI certificates, and mTLS — the verified client identity (fingerprint, subject CN, SAN DNS) is exposed to the graph |
| Error handling | Per-node error edges, a policy-level catch-all handler, and a generic 500 fallback — see [Error handling](../concepts/error-handling.md) |
| Operations | Admin REST API with Basic Auth, `/healthz` and `/readyz` probes, Prometheus metrics per route and per node; graceful shutdown drains in-flight requests on SIGTERM |
| Configuration | Two YAML files (`system.yaml`, `gateway.yaml`) with `${ENV_VAR:-default}` interpolation; hot-reload on file change or via the API |
| Deployment | Single binary; Docker Compose for local development; optional etcd-backed clustering for HA — see [Deployment](../guides/deployment.md) |

## When to use featherbit

featherbit is a good fit when you want:

- a gateway you can deploy as one binary or one small container, configured entirely through YAML and environment variables;
- request pipelines that are **explicit and inspectable** — the graph shows exactly which nodes run, in what order, and where errors go;
- custom logic without recompiling, via Lua script nodes;
- configuration changes that take effect without restarts (file watcher or `POST /api/config/reload`).

:::note Planned
Two capabilities from the project specification are not implemented yet and should not be relied on: the **Python scripting runtime** (Lua is the supported scripting runtime today) and the **`unpack` node**. See the [roadmap](../reference/roadmap.md).
:::

## Next steps

- [Quick start](quick-start.md) — run the gateway locally or with Docker Compose and send a first request.
- [Your first route](first-route.md) — add a new route and policy to `gateway.yaml` step by step.
- [Architecture](../concepts/architecture.md) — how a request travels through the gateway internally.
