---
title: ip-restriction
description: Filter clients by remote address against allow and deny lists of exact IPs or CIDR blocks.
---

<span className="plugin-chip" style={{'--chip-color': '#ec4899'}}>ip-restriction</span>

Restricts access based on `context.request.remote_addr`, matching it against configured allow and deny lists of exact IPs or CIDR blocks. Denied clients receive a 403 through the node's `error` port. Place it at the front of the pipeline, before auth and upstream nodes.

## Configuration

Both keys are optional; the constructor never fails. With both lists empty, all traffic is permitted.

| Key | Type | Default | Description |
|---|---|---|---|
| `allow` | array of strings | `[]` | IPs or CIDR blocks (e.g. `"10.0.0.0/8"`) that may pass. When non-empty, the list acts as a whitelist: everything else is rejected. |
| `deny` | array of strings | `[]` | IPs or CIDR blocks that are always rejected. Takes precedence over `allow`. |

```yaml
type: ip-restriction
config:
  allow: ["10.0.0.0/8", "192.168.1.5"]
  deny: ["10.1.2.3"]
```

## Matching

The client address is parsed as an IP; a trailing `:port` suffix is stripped if present. Each pattern is either an exact IP or a `net/bits` CIDR block — both IPv4 and IPv6 are supported, and an IPv4 pattern never matches an IPv6 client (or vice versa). An unparseable remote address never matches any pattern: it passes the deny check, but is rejected whenever the allow list is non-empty.

## Behavior

Checks run in order:

1. **Deny list first** — if `deny` is non-empty and the client matches, the request is rejected with error code `IP_DENIED`.
2. **Allow list** — if `allow` is non-empty and the client does not match, the request is rejected with error code `IP_NOT_ALLOWED`.

Either rejection writes a 403 JSON response onto `context.response` (`{"error": "forbidden", ...}` with `content-type: application/json`) and routes the Context through the `error` port. Permitted requests pass through the `success` port with the Context untouched. The plugin does not write to `context.message`.

:::note Legacy configs
Older UI builds saved the keys `mode` and `rules`, which the plugin ignores - nodes saved with them apply no restriction. Re-save the node (the editor now uses `allow` and `deny` lists) or update the YAML.
:::
