---
title: response-rewrite
description: Rewrite the response status code, body, and headers, with regex body filters and a vars gate.
---

<span className="plugin-chip" style={{'--chip-color': '#6366f1'}}>response-rewrite</span>

Rewrites `context.response` before it reaches the client: forces a status code, replaces the body, applies regex filters to the body, and adds/sets/removes headers with `$var` interpolation. It is a response-phase node — place it after `upstream`, before `client`. featherbit responses are fully buffered, so status, headers, and body are all rewritten in a single node execution.

## Configuration

All keys are optional.

:::caution Not the same header keys as `proxy-rewrite`
`response-rewrite` takes a single `headers` object with `add` / `set` / `remove` sub-keys. The sibling [`proxy-rewrite`](./proxy-rewrite.md) instead uses top-level `add_headers` / `remove_headers`. Passing `add_headers` here (or `headers` there) is now **rejected at load** with a message pointing to the right shape, rather than being silently ignored.
:::

| Key | Type | Default | Description |
|---|---|---|---|
| `status_code` | integer | — | New response status code. Must be within 200-598. |
| `body` | string | — | New response body. Mutually exclusive with `filters` (configuring both fails at load). |
| `body_base64` | bool | `false` | When true, `body` is base64-decoded at config load — for binary or pre-encoded content. Invalid or empty base64 fails at load. |
| `headers.add` | array of strings | `[]` | Headers appended alongside existing values, each entry as `"Name: value"`. The value must be non-empty and colon-free. |
| `headers.set` | map | `{}` | Headers whose value is replaced. Values may be strings or numbers. |
| `headers.remove` | array of strings | `[]` | Header names to delete. |
| `headers` (flat map) | map | — | Deprecated shape, accepted for config compatibility: a plain `{name: value}` map is treated as `set`. |
| `filters` | array | — | Regex substitutions applied to the response body (see below). Mutually exclusive with `body`. |
| `vars` | array | — | Triple-array condition expression, e.g. `[["status", "==", "500"]]` (rules are ANDed; operators: `==`, `~=`, `>`, `>=`, `<`, `<=`, `~~`, `~*`, `in`, `has`, `ipmatch`). When present and false at execution time, the node is a pure passthrough. |

Each `filters` entry:

| Key | Type | Default | Description |
|---|---|---|---|
| `regex` | string | required | Pattern matched against the body. Compiled at config load — invalid patterns fail there. |
| `replace` | string | required | Replacement text; `$1`-style capture references are supported. |
| `scope` | string | `once` | `once` replaces the first match, `global` replaces all. |
| `options` | string | `""` | Only `"i"` (case-insensitive) is supported; any other value is rejected. |

`headers.add` and `headers.set` values support `$var` / `${var}` interpolation at execution time (`$status`, `$remote_addr`, `$http_<name>`, `$msg_<key>`, ...). Unknown variables resolve to the empty string.

```yaml
type: response-rewrite
config:
  status_code: 200
  headers:
    set:
      x-server-id: "3"
    add:
      - "x-trace: $http_x_request_id"
    remove: [x-powered-by]
  filters:
    - regex: "X-Amzn-"
      scope: global
      replace: ""
  vars:
    - ["status", "==", "200"]
```

## Behavior

This plugin never fails at execution time — it always exits through the `success` port.

1. **Gate** — when `vars` is configured and evaluates to false, the context passes through completely unchanged.
2. **Status** — `status_code` is applied.
3. **Body** — `body` (decoded from base64 when `body_base64`) replaces the response body. Otherwise `filters` run, in order: if the upstream body is content-encoded (`gzip`, `deflate`, `br`), it is decoded first and left decoded afterward. Whenever the body changes, the stale `content-length`, `content-encoding`, `last-modified`, and `etag` headers are removed (the server layer recomputes the length).
4. **Headers** — `add`, then `set`, then `remove`, with interpolated values. Header names are lowercased.

Filters are skipped with a warning — leaving body **and** headers untouched — when the body carries an unsupported `content-encoding` (e.g. `zstd`), fails to decode, or is not valid UTF-8 — in those cases the response passes through intact.

Filter `options` only accepts `"i"` — the PCRE `j`/`o` flags are JIT/compile-cache hints with no meaning here and are rejected at config load.

The plugin does not read or write `context.message` or `context.errors`.
