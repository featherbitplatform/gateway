---
title: request-validation
description: Validate request headers and body against JSON Schemas, rejecting non-conforming requests before they reach the upstream.
---

<span className="plugin-chip" style={{'--chip-color': '#eab308'}}>request-validation</span>

Validates the request's headers and/or body against JSON Schemas and rejects non-conforming requests with a configurable status code. Place it before the `upstream` node.

## Configuration

At least one of `header_schema` / `body_schema` is required.

| Key | Type | Default | Description |
|---|---|---|---|
| `header_schema` | object | — | JSON Schema applied to the request headers, seen as a flat object `{name: first_value}` with lowercase header names. |
| `body_schema` | object | — | JSON Schema applied to the parsed request body. |
| `rejected_code` | integer 200–599 | `400` | Response status for rejected requests. |
| `rejected_msg` | string | — | Fixed message returned instead of the validator's error description. |

```yaml
type: request-validation
config:
  rejected_code: 422
  body_schema:
    type: object
    required: [name]
    properties:
      name: { type: string, minLength: 1 }
  header_schema:
    type: object
    required: [x-api-version]
```

Schemas are compiled once at config load with the `jsonschema` crate — malformed schemas (and non-object schema values, missing schemas, out-of-range `rejected_code`) fail policy compilation, never a live request.

## Behavior

1. **Headers** (when `header_schema` is set) — validated as a single-value object: the *first* value of each header, names lowercased.
2. **Body** (when `body_schema` is set):
   - An empty body is rejected.
   - `application/x-www-form-urlencoded` bodies are decoded into a flat object mirroring `ngx.decode_args`: `a=1&a=2` becomes `{"a": ["1","2"]}`, a bare `flag` (no `=`) becomes `{"flag": true}`, values are percent-decoded.
   - Any other content type is parsed as **JSON**; a body that fails to parse is rejected.
   - The parsed value is validated against `body_schema`.
3. **JSON normalization** — after a successful JSON-body validation the body is re-serialized from the parsed document and the stale `content-length` header is removed, so the JSON that was validated is exactly the JSON the upstream receives (guards against [JSON interoperability](https://bishopfox.com/blog/json-interoperability-vulnerabilities) smuggling). Urlencoded bodies are passed through unchanged.

On any rejection the plugin writes `rejected_code` plus the JSON body `{"error": "validation_failed", "message": <rejected_msg or validator detail>}` onto `context.response` and fails with error code `VALIDATION_FAILED`, routing the Context through the `error` port.

The plugin does not write to `context.message`.

## Limitations

- Headers validate the **first** value of multi-valued headers. Schemas that assert array-typed header values will not match.
- Secret-reference (`$secret://`) indirection for schemas is not supported — schemas are literal objects in the node config.
