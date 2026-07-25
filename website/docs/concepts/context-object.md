---
title: The Context Object
description: The per-request Context that flows through every node — request, response, message, and errors.
---

The **Context** holds all state for one request as it travels through a policy graph. It is created when a route matches, passed to every node in turn, and whatever its `response` field contains when execution finishes is what the client receives. Each plugin may read and mutate any part of it.

```
Context
├── request     # the inbound request (plugins may rewrite it before proxying)
├── response    # the response under construction
├── message     # free-form key/value scratch space for inter-node data
└── errors      # errors accumulated during graph execution
```

## `context.request`

A protocol-agnostic snapshot of the inbound request, populated by the listener:

| Field | Type | Notes |
|---|---|---|
| `method` | string | `GET`, `POST`, ... |
| `path` | string | Request path without the query string |
| `host` | string | Value of the `Host` header; empty string when absent |
| `scheme` | string | Defaults to `http` when the URI carries none |
| `headers` | map of string → list of string | Multi-valued; headers may repeat |
| `query_params` | map of string → list of string | Multi-valued; parameters may repeat |
| `body` | bytes | Fully buffered |
| `remote_addr` | string | Client socket address as `ip:port` |
| `protocol` | enum | `http1` or `http2` (see note below) |

Transform plugins such as `proxy-rewrite` modify this before the `upstream` plugin forwards it — for example stripping a path prefix or removing headers.

## `context.response`

Initially empty. Populated by the `upstream` plugin (with the backend's reply) or by any plugin that short-circuits, such as an auth plugin returning 401 or an `error-handler` rendering a custom body. Downstream nodes may inspect or modify it.

| Field | Type | Notes |
|---|---|---|
| `status_code` | u16 | `0` means **unset** — no node has written a status yet |
| `headers` | map of string → list of string | |
| `body` | bytes | |

When graph execution finishes, a `status_code` of `0` is treated as `200` by the server before the response is sent to the client.

## `context.message`

A free-form key/value map (string → JSON value) that the gateway imposes **no schema** on. Plugins use it to pass data to downstream nodes:

- the `jwt-auth` plugin validates the token and extracts its claims into `context.message` for downstream nodes (for example a logging plugin) to read;
- a Lua script node can set arbitrary keys (`ctx.message.processed_by = "lua-plugin"`) and read keys written by earlier nodes.

Because there is no schema, the convention around key names is defined by your policy — which nodes write what, and which nodes read it.

## `context.errors`

An append-only list of errors accumulated during graph execution. When a node fails, the engine tags the error with the failing node's `id`, appends it here, and routes the context through the error path — see [Error handling](error-handling.md). Each entry records:

| Field | Type | Notes |
|---|---|---|
| `node_id` | string | Id of the node that produced the error |
| `code` | string | Machine-readable code, e.g. `unauthorized`, `rate_limited` |
| `message` | string | Human-readable description |
| `metadata` | map of string → JSON value | Optional structured details |

Errors do not abort the pipeline by themselves; they redirect where execution goes next.

## Serialization

The Context is serializable so it can be marshalled to and from Lua scripts (and rendered as JSON). Request and response **bodies are encoded as base64 strings** during serialization, keeping binary payloads intact through JSON/Lua round-trips. Inside a Lua script the context arrives as a native table mirroring the structure above.

:::note Planned
The `protocol` enum also declares `websocket`, `tcp`, and `udp` variants, but only `http1` and `http2` are currently produced; the rest are reserved for planned WebSocket/TCP/UDP proxying. A Python scripting runtime that would marshal the same Context into Python dicts is also planned but not implemented.
:::
