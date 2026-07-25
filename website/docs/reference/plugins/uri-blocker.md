---
title: uri-blocker
description: Block requests whose URI (path plus query string) matches any of a list of regular expressions.
---

<span className="plugin-chip" style={{'--chip-color': '#dc2626'}}>uri-blocker</span>

Blocks requests whose request URI matches any configured `block_rules` regex. Matching requests receive a configurable status (default 403) through the node's `error` port. Place it at the front of the pipeline, before auth and upstream nodes.

## Configuration

`block_rules` is required and must be non-empty; regexes are compiled at config load, so an invalid pattern fails fast.

| Key | Type | Default | Description |
|---|---|---|---|
| `block_rules` | array of regex strings | **required** | Rules tested against the request URI. A request matching any rule is rejected. |
| `rejected_code` | integer (200–599) | `403` | HTTP status for rejections. |
| `rejected_msg` | string | — | When set, rejections carry a JSON body `{"error_msg": ...}`; when unset the body is empty. |
| `case_insensitive` | bool | `false` | Match rules case-insensitively (each rule is compiled with `(?i)`). |

```yaml
type: uri-blocker
config:
  block_rules: ["root.exe", "root.m+", "^/admin/"]
  rejected_code: 404
  case_insensitive: true
```

## Matching

The subject is the request path plus `?query` when query parameters exist, so a rule can hit either the path (`^/admin/`) or a query value (`root.exe` against `/download?file=root.exe`). Matching is unanchored Rust `regex` syntax; anchor with `^`/`$` where needed.

## Behavior

A blocked request writes `rejected_code` onto `context.response` — with a JSON body `{"error_msg": rejected_msg}` (`content-type: application/json`) when `rejected_msg` is set, otherwise an empty body — and routes the Context through the `error` port with error code `URI_BLOCKED`. Non-matching requests pass through the `success` port untouched; the plugin does not write to `context.message`.

:::note Behavior notes
featherbit compiles each rule separately and tests them in order, giving clear per-rule config errors. The query string is rebuilt from parsed parameters (sorted `key=value` pairs) rather than the raw wire bytes, and patterns use Rust `regex` syntax, not PCRE.
:::
