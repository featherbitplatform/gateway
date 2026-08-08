---
title: ua-restriction
description: Allow or deny clients by matching the User-Agent header against lists of regular expressions.
---

<span className="plugin-chip" style={{'--chip-color': '#d946ef'}}>ua-restriction</span>

Restricts access based on the `User-Agent` request header, matching it against a configured allowlist or denylist of regular expressions. Rejected clients receive a configurable status (default 403) through the node's `denied` port. Place it at the front of the pipeline, before auth and upstream nodes.

## Configuration

Exactly **one** of `allowlist` / `denylist` must be non-empty — configuring both (or neither) is a config error. Regexes are compiled at config load, so an invalid pattern fails fast.

| Key | Type | Default | Description |
|---|---|---|---|
| `allowlist` | array of regex strings | — | Only User-Agents matching at least one rule pass; everything else is rejected. |
| `denylist` | array of regex strings | — | User-Agents matching any rule are rejected. |
| `bypass_missing` | bool | `false` | Pass requests that carry no `User-Agent` header instead of rejecting them. |
| `rejected_code` | integer (200–599) | `403` | HTTP status for rejections. |
| `rejected_msg` | string | `"Not allowed"` | Rejection message, returned as `{"message": ...}`. |

```yaml
type: ua-restriction
config:
  denylist: ["curl/.*", "(?i)spider"]
  bypass_missing: false
  rejected_msg: Not allowed
```

## Matching

Each `User-Agent` value (requests may carry several) is trimmed of surrounding whitespace and tested with Rust `regex` syntax. Matching is unanchored and case-sensitive; use `(?i)` inside a rule for case-insensitive matching. In allowlist mode the request passes when **any** value matches **any** rule; in denylist mode it is rejected when **any** value matches **any** rule.

## Behavior

1. **Missing User-Agent** — passed when `bypass_missing` is `true`, otherwise rejected.
2. **Allowlist mode** — non-matching requests are rejected.
3. **Denylist mode** — matching requests are rejected.

A rejection writes `rejected_code` with a JSON body `{"message": rejected_msg}` (`content-type: application/json`) onto `context.response` and exits through the `denied` port. Permitted requests pass through the `success` port untouched; the plugin does not write to `context.message`.

:::note Behavior notes
The rejection status is configurable via `rejected_code` and the message via `rejected_msg`, for consistency with `uri-blocker`. Patterns use Rust `regex` syntax, not PCRE (no backreferences or lookarounds).
:::

## Ports

`ua-restriction` declares three output ports: `success`, `denied` (a rejection is prepared), and `error` (never actually used — the plugin never fails). Like `success`, `denied` is a mandatory port: the policy compiler rejects any policy that leaves it unwired. Wire `ua-restriction.denied` straight to `client` so the prepared rejection reaches the caller instead of continuing into `upstream`:

```yaml
edges:
  - from: ua-restriction.success
    to: upstream.in
  - from: ua-restriction.denied
    to: client.in
```
