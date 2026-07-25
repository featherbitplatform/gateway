---
title: body-transformer
description: Rewrite the request or response body from a template with JSON body fields and context variables.
---

<span className="plugin-chip" style={{'--chip-color': '#22c55e'}}>body-transformer</span>

Rewrites the request and/or response body by rendering a template against the parsed JSON body and context variables. The template language is a **deliberate subset** — read the limitations below before writing templates.

## Limitations (important)

featherbit implements:

- **`input_format: json` only.** `xml`, `encoded`, `args`, `plain`, and `multipart` are rejected at config load. Omitting `input_format` defaults to `json` (there is no content-type sniffing).
- **Placeholder templates only.** Templates are plain strings with two placeholder forms — no expressions, loops, or escaping helpers:
  - `{{body.x.y}}` — dotted path into the parsed JSON body (numeric segments index arrays, e.g. `{{body.items.0.id}}`; `{{body}}` is the whole document).
  - `{{$var}}` — a context variable (`$uri`, `$http_x_request_id`, `$arg_page`, `$status`, ... — the same names the `vars` system resolves).
  Text outside `{{ }}` also gets plain `$var` interpolation.
- **`template_is_base64` is not supported** and is rejected at config load — store templates literally in YAML.

## Configuration

At least one of `request` / `response` is required.

| Key | Type | Default | Description |
|---|---|---|---|
| `request` | object | — | Transform applied to `context.request.body`. |
| `request.template` | string | required | Output body template. |
| `request.input_format` | string | `json` | Only `json` is accepted. |
| `response` | object | — | Same shape, applied to `context.response.body`. |

```yaml
type: body-transformer
config:
  request:
    input_format: json
    template: '{"name":"{{body.user.name}}","trace":"{{$http_x_request_id}}"}'
```

Rejected at config load: missing/empty templates, an unclosed `{{` placeholder, non-`json` `input_format`, and `template_is_base64: true`.

## Behavior

**Placement:** a node with a `request` transform must sit **before** `upstream`; a node with a `response` transform must sit **after** it (the response body is empty until the upstream runs). Use two nodes when transforming both sides.

Placeholder resolution: strings from the body are inserted **raw** (unquoted — add quotes in the template when building JSON), numbers and booleans verbatim, `null` and missing paths as the empty string, and objects/arrays as compact JSON. Unknown placeholders render empty. Values substituted from the body are never re-interpolated.

- **Request transform** — an empty body renders the template with all `{{body...}}` placeholders empty; a non-empty body that is not valid JSON fails with a **400**. After rendering, the body is replaced and the stale `content-length` / `content-encoding` headers are removed.
- **Response transform** — a `content-encoding`d upstream body (gzip, deflate, br) is decoded first; unsupported encodings, undecodable data, or a non-JSON body fail with a **502**. After rendering, the body is replaced *decoded* and `content-length` / `content-encoding` are removed.

Failures exit through the `error` port with error code `BODY_DECODE_FAILED` and a JSON response body `{"error": "body_decode_failed", "message": ...}`. The plugin does not write to `context.message`.
