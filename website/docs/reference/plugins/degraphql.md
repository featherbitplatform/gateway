---
title: degraphql
description: Expose a GraphQL upstream through a plain REST route by rewriting requests into GraphQL POST bodies.
---

<span className="plugin-chip" style={{'--chip-color': '#d946ef'}}>degraphql</span>

Translates a REST call into a GraphQL upstream call: the incoming request is rewritten into a canonical GraphQL POST — body `{"query": ..., "variables": {...}, "operationName": ...}`, method `POST`, `content-type: application/json` — with variables harvested from the client's query parameters and JSON body.

**Placement:** this node rewrites `context.request`, so it must sit **before** the `upstream` node that forwards to the GraphQL server.

## Configuration

| Key | Type | Default | Description |
|---|---|---|---|
| `query` | string, 1–1024 chars | required | The GraphQL document sent upstream. |
| `variables` | array of strings | — | Variable names to collect from the request. Must be non-empty when present; when absent, no `variables` key is sent upstream. |
| `operation_name` | string, 1–1024 chars | — | Sent as `operationName`, for multi-operation documents. |

```yaml
type: degraphql
config:
  query: |
    query ($name: String!) {
      persons(filter: { name: $name }) { id name }
    }
  variables: [name]
```

Rejected at config load: a missing/blank/oversized `query`, unbalanced curly braces or no selection set in `query`, an empty `variables` array, non-string variable names, and a blank `operation_name`.

## Behavior

1. Only `GET` and `POST` are accepted; any other method is rejected with **405** and error code `METHOD_NOT_ALLOWED`.
2. Each name in `variables` is resolved: **query parameters first** (first value, always a string), **then JSON body fields** (keeping their JSON types). Names found in neither source are omitted from `variables`. The body is parsed lazily — a non-empty, non-JSON body only fails (with **400**, code `INVALID_REQUEST_BODY`) when a variable actually needs it.
3. The request is rewritten: method forced to `POST`, body replaced with the GraphQL JSON document, `content-type` set to `application/json`, and the stale `content-length` / `content-encoding` headers removed (body-mutation convention).

Rejections write a JSON `{"error": ..., "message": ...}` response and exit through the `error` port. The plugin does not write to `context.message`.

## Behavior notes

- **`query` is checked structurally only** (non-blank, balanced braces, a selection set present) — it is not parsed with a full GraphQL parser, and a multi-operation document missing `operation_name` is not caught at config load; the GraphQL server will reject such documents itself.
- **GETs become POSTs.** featherbit always sends the canonical JSON POST, even for `GET` requests.
- **Variables merge both sources.** For every request, query parameters are checked first, falling back to JSON body fields — regardless of the request method.
