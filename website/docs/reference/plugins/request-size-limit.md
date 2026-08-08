---
title: request-size-limit
description: Reject requests whose body exceeds a configured byte limit with a 413 error.
---

<span className="plugin-chip" style={{'--chip-color': '#84cc16'}}>request-size-limit</span>

Enforces a maximum request body size: requests whose body exceeds `max_bytes` are rejected with a 413 through the node's `denied` port. Place it early in the pipeline, before nodes that process the body or forward it upstream.

## Configuration

The single key is optional; the constructor never fails.

| Key | Type | Default | Description |
|---|---|---|---|
| `max_bytes` | integer | `1048576` (1 MiB) | Maximum allowed request body size in bytes. |

```yaml
type: request-size-limit
config:
  max_bytes: 262144
```

## Behavior

The request body is already fully buffered by the time a policy graph runs, so the plugin simply compares `context.request.body` length against `max_bytes`:

- **Within the limit** — the request passes through the `success` port with the Context untouched.
- **Over the limit** — the plugin writes a rejection onto `context.response` (status `413`, JSON body `{"error": "payload_too_large", "message": "Request body exceeds size limit"}`, `content-type: application/json`) and exits through the `denied` port.

The plugin does not write to `context.message`.

**UI editor note:** the node inspector form also shows a `reject_status` field, but the plugin does not read that key — the rejection status is always `413`.

## Ports

`request-size-limit` declares three output ports: `success`, `denied` (a rejection is prepared), and `error` (never actually used — the plugin never fails). Like `success`, `denied` is a mandatory port: the policy compiler rejects any policy that leaves it unwired. Wire `request-size-limit.denied` straight to `client` so the prepared `413` reaches the caller instead of continuing into `upstream`:

```yaml
edges:
  - from: request-size-limit.success
    to: upstream.in
  - from: request-size-limit.denied
    to: client.in
```
