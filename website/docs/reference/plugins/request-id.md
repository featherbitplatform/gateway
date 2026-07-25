---
title: request-id
description: Ensure every request carries a unique id header and optionally echo it on the response.
---

<span className="plugin-chip" style={{'--chip-color': '#0ea5e9'}}>request-id</span>

Guarantees a request-id header on the request: when the header is absent or empty, a fresh UUID v4 is generated and set on `context.request.headers`; an id supplied by the client is kept as-is. With `include_in_response` the same id is also set on the response. It is typically placed right after the `listener` node so every downstream node (logging, upstream) sees the id.

## Configuration

All keys are optional.

| Key | Type | Default | Description |
|---|---|---|---|
| `header_name` | string | `X-Request-Id` | Header carrying the id. Lowercased internally, per featherbit's header convention. |
| `include_in_response` | bool | `true` | Also set the id on `context.response.headers`, unless the response already carries the header. |
| `algorithm` | string | `uuid` | Id generation algorithm. Only `uuid` (UUID v4) is supported; any other value is rejected at config load. |

```yaml
type: request-id
config:
  header_name: X-Request-Id
  include_in_response: true
  algorithm: uuid
```

## Behavior

This plugin never fails at execution time — it always exits through the `success` port, and the `error` port is never taken.

On each request:

1. If the request header is absent or empty, a UUID v4 is generated and set. A non-empty client-supplied id is trusted and reused unchanged.
2. With `include_in_response: true`, the id is set on the response **only if** the response does not already carry the header (an upstream-provided id wins).

**Placement note:** the `upstream` node replaces `context.response.headers` wholesale with the upstream's response headers, so a response echo written *before* `upstream` is lost unless the upstream itself echoes the header. To guarantee the id on the response, add a second `request-id` node after `upstream`: it finds the request header already set, reuses the same id, and stamps it on the response.

**Limitations:** only the `uuid` algorithm is implemented — `nanoid`, `range_id`, `ksuid`, and `uuidv7` are rejected at config load with an error naming the supported set.
