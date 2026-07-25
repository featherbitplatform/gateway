---
title: data-mask
description: Mask sensitive request data — query parameters, headers, and JSON body fields — before it reaches loggers or the upstream.
---

<span className="plugin-chip" style={{'--chip-color': '#64748b'}}>data-mask</span>

Masks sensitive fields in the request: query parameters, headers, and JSON body fields can be **removed**, **replaced** with a fixed value, or partially rewritten with a **regex** substitution. Place it before the `logging` and/or `upstream` nodes that must not see the raw values.

## Configuration

| Key | Type | Default | Description |
|---|---|---|---|
| `request` | array of rules | `[]` | Masking rules, applied in order. |
| `max_body_size` | integer > 0 | `1048576` (1 MiB) | Bodies larger than this skip body rules (query/header rules still apply). |

Each rule object:

| Key | Type | Required | Description |
|---|---|---|---|
| `type` | string | yes | `query`, `header`, or `body`. |
| `name` | string | yes | Query parameter / header name (case-insensitive for headers), or a dotted path into the JSON body — `user.cards.0.number`. Numeric segments index arrays; a leading `$.` is tolerated. |
| `action` | string | yes | `remove`, `replace`, or `regex`. |
| `value` | string | for `replace`/`regex` | Replacement value. For `regex` it may use `$1`-style capture references. |
| `regex` | string | for `regex` | Pattern (Rust `regex` syntax), compiled at config load. |
| `body_format` | string | for `body` rules | Only `json` is accepted. |

```yaml
type: data-mask
config:
  request:
    - { type: header, name: authorization, action: replace, value: "***" }
    - { type: query, name: token, action: remove }
    - type: body
      body_format: json
      name: user.card_number
      action: regex
      regex: "^(\\d{4})\\d+(\\d{4})$"
      value: "$1********$2"
```

Rejected at config load: unknown `type`/`action` values, `regex` rules without `regex`+`value`, `replace` rules without `value`, invalid regex patterns, body rules without `body_format`, `body_format` values other than `json`, and dotted paths with empty segments.

## Behavior

Masking is best-effort and **never fails at execution time** — the node always exits through the `success` port and emits no error codes.

- **Query rules** operate on `context.request.query_params`: `remove` deletes the parameter, `replace` collapses it to the single configured value, `regex` rewrites the **first** match in every value of the parameter (only the first occurrence is substituted).
- **Header rules** operate the same way on `context.request.headers` (names lowercased).
- **Body rules** share one lazily parsed JSON document. Rules whose dotted path does not resolve (missing key, out-of-range index, type mismatch) are skipped, as are `regex` actions on non-string values. `remove` on an array index deletes the element (shifting the rest left). If any body rule changed the document it is re-serialized into `context.request.body` and the stale `content-length` header is removed.

Bodies that are absent, larger than `max_body_size`, or not valid JSON silently skip all body rules — the body passes through untouched.

The plugin only touches `context.request`; it never writes `context.message` or `context.errors`.

## Behavior notes

- **Dotted paths, not JSONPath** — body field names are exact dotted paths only; there is no JSONPath engine, no recursive descent, no wildcards, and each rule addresses one location.
- **`body_format: urlencoded` is not supported** and is rejected at config load; only JSON bodies can be masked.
- **Phase** — masking applies at the node's position in the graph, so it also masks what the upstream receives if placed before `upstream`, not only what downstream loggers see.
