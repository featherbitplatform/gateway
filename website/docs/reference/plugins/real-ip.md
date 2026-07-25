---
title: real-ip
description: Rewrite the client address seen by the pipeline from a header such as X-Forwarded-For or X-Real-IP.
---

<span className="plugin-chip" style={{'--chip-color': '#d946ef'}}>real-ip</span>

Replaces `context.request.remote_addr` with the address carried by a configured variable — typically `http_x_forwarded_for` or `http_x_real_ip` — so downstream nodes (`ip-restriction`, `rate-limit` with client-address keys, `logging`) see the real client instead of the last proxy hop. Place it early in the policy, before any node that reads the client address.

The rewrite is guarded: when `trusted_addresses` is configured, it only happens if the **direct** peer address matches the list, so a client cannot spoof its IP unless the request came through a trusted proxy.

## Configuration

| Key | Type | Default | Description |
|---|---|---|---|
| `source` | string | — (**required**) | Variable holding the real address, e.g. `http_x_real_ip`. `http_x_forwarded_for` gets special comma-list handling (see below); any other name goes through the standard variable resolver (`http_<header>`, `arg_<name>`, ...). |
| `trusted_addresses` | array of IPs/CIDRs | — | Only rewrite when the direct peer matches one of these. When omitted, the rewrite always applies. An empty array or an invalid IP/CIDR is a config error. |
| `recursive` | bool | `false` | For `http_x_forwarded_for` with `trusted_addresses`: walk the list from the rightmost entry, skip trusted hops, and take the first untrusted address (falling back to the leftmost entry when every hop is trusted). When `false`, the last (rightmost) entry is used. |

```yaml
type: real-ip
config:
  source: http_x_forwarded_for
  trusted_addresses: ["127.0.0.0/24", "10.0.0.0/8"]
  recursive: true
```

## Behavior

This plugin never fails at execution time — every non-applicable case (untrusted peer, missing header, unparsable address, port out of range) is a silent passthrough that leaves `remote_addr` unchanged, and the `error` port is never taken.

On each request:

1. **Trust check** — with `trusted_addresses` set, the direct peer IP (from `context.request.remote_addr`) must match one of the networks; otherwise the request passes through unchanged.
2. **Address extraction** — for `source: http_x_forwarded_for`, the last `X-Forwarded-For` header value is read and split on commas: non-recursive mode takes the rightmost entry; recursive mode walks right-to-left skipping trusted hops (an unparsable entry counts as untrusted). For any other `source`, the variable is resolved directly.
3. **Rewrite** — the extracted value may be `ip`, `ip:port`, or `[v6]:port`. When the source carries no port, the original peer port is kept. The new value is written back to `context.request.remote_addr`.

**Behavior notes:** featherbit rewrites `context.request.remote_addr` directly, so there is no runtime dependency and no error path. Variables resolve through featherbit's resolver (see the `vars` reference).
