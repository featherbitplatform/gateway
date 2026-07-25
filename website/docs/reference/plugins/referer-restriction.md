---
title: referer-restriction
description: Allow or deny requests by the host of the Referer header, with exact hosts and *.wildcard patterns.
---

<span className="plugin-chip" style={{'--chip-color': '#f43f5e'}}>referer-restriction</span>

Restricts access based on the host parsed out of the `Referer` request header, matching it against a configured whitelist or blacklist of host patterns. Rejected clients receive a 403 through the node's `error` port. Place it at the front of the pipeline, before auth and upstream nodes.

## Configuration

Exactly **one** of `whitelist` / `blacklist` must be non-empty — configuring both (or neither) is a config error.

| Key | Type | Default | Description |
|---|---|---|---|
| `whitelist` | array of host patterns | — | Only these Referer hosts pass; everything else is rejected. |
| `blacklist` | array of host patterns | — | These Referer hosts are rejected. |
| `bypass_missing` | bool | `false` | Pass requests whose Referer is missing or not a parseable `http(s)` URL. |
| `message` | string | `"Your referer host is not allowed"` | Rejection message, returned as `{"message": ...}`. |

```yaml
type: referer-restriction
config:
  whitelist: ["example.com", "*.example.org"]
  bypass_missing: true
```

## Matching

The Referer value must be an `http://` or `https://` URL; the host part is extracted (port, path, and query are ignored) and compared case-insensitively. Anything else — including a bare host without a scheme — counts as a *missing* Referer. Patterns are either exact hosts (`example.com`) or leading-`*` wildcards: `*.example.com` matches any subdomain (`api.example.com`) but **not** the bare apex `example.com`.

## Behavior

1. **Missing or malformed Referer** — passed when `bypass_missing` is `true`, otherwise rejected (in both list modes).
2. **Whitelist mode** — hosts not matching any pattern are rejected.
3. **Blacklist mode** — hosts matching any pattern are rejected.

A rejection writes a 403 JSON response (`{"message": ...}`, `content-type: application/json`) onto `context.response` and routes the Context through the `error` port with error code `REFERER_RESTRICTED`. Permitted requests pass through the `success` port untouched; the plugin does not write to `context.message`.

:::note Behavior notes
The Referer URL parser is a small built-in (scheme + host extraction); it does not accept IPv6 literal hosts.
:::
