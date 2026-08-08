---
title: multi-auth
description: Chains several auth plugins and accepts the request if any one succeeds; rejects with 401 only when all fail.
---

<span className="plugin-chip" style={{'--chip-color': '#0ea5e9'}}>multi-auth</span>

Runs a list of authentication sub-plugins in order and accepts the request as soon as **any** of them authenticates it (first success wins). The request is rejected with a `401` only when **every** sub-plugin fails. This lets a single route accept more than one credential scheme — for example an API key *or* HTTP Basic — without branching the policy graph. Place it early in the request pipeline, before the upstream node.

## Configuration

| Key | Type | Default | Description |
|---|---|---|---|
| `auth_plugins` | array | — (required) | Ordered list of auth sub-plugins to try. Each element is a **single-key map** `{plugin-type: {that plugin's config}}`. Every entry is instantiated at config load, so an unknown plugin type or an invalid sub-config fails fast. Must be non-empty. |

Each entry's inner object is the exact config you would give that plugin as a standalone node.

```yaml
- id: auth
  type: multi-auth
  config:
    auth_plugins:
      - key-auth:
          use_consumers: true
          header_name: x-api-key
      - basic-auth:
          use_consumers: true
```

## Behavior

The sub-plugins run in the listed order, threading the context from one attempt to the next:

- The **first** sub-plugin that returns a clean **success** (no named exit port) ends the chain; its output is returned verbatim, so any consumer identity it attached (`consumer.*` message keys, `X-Consumer-*` headers) flows downstream unchanged. `multi-auth` itself then passes through its own **success** port.
- Anything else a sub-plugin returns — a raw failure, or an alternate outcome port such as a credential-auth sub-plugin's own `denied` — is treated as a **failed attempt**, not a match: `multi-auth` has no way to fan a single request out to more than one downstream route, so only a plain success can end the chain early.
- If **all** sub-plugins fail, the request is rejected and exits through `multi-auth`'s own **`denied`** port:
  - `context.response.status_code` = `401`
  - Body: `{"error": "unauthorized", "message": "Authorization Failed"}` with `content-type: application/json`

A later sub-plugin sees the context as left by prior *failed* attempts. Auth plugins generally mutate the context only on success (leaving `request`/`message` untouched when they reject), so ordering is safe. As an extra safeguard, the `response` is reset between attempts, so a losing sub-plugin's rejection body never leaks onto the request when a later attempt succeeds.

:::note Any plugin type is accepted
Only auth-type plugins are meaningful here, but the set is not hard-restricted — any registered node type may be listed. A non-auth plugin simply runs as an ordinary node, and its success ends the chain.
:::

## Ports

`multi-auth` declares three output ports: `success`, `denied` (every sub-plugin failed; a rejection is prepared), and `error` (never actually used — the plugin never fails itself; a sub-plugin's own genuine infra failure is absorbed as a failed attempt, not propagated). Like `success`, `denied` is a mandatory port: the policy compiler rejects any policy that leaves it unwired. Wire `multi-auth.denied` straight to `client` so the prepared `401` reaches the caller instead of continuing into `upstream`:

```yaml
edges:
  - from: multi-auth.success
    to: upstream.in
  - from: multi-auth.denied
    to: client.in
```
