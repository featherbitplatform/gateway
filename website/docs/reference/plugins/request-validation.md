---
title: request-validation
description: Validate request headers and body against JSON Schemas and condition expressions, rejecting non-conforming requests before they reach the upstream.
---

<span className="plugin-chip" style={{'--chip-color': '#eab308'}}>request-validation</span>

Validates the request's headers and/or body against JSON Schemas, and/or a [condition expression](../conditions.md), rejecting non-conforming requests with a configurable status code. Place it before the `upstream` node.

## Configuration

At least one of `header_schema` / `body_schema` / `conditions` is required.

| Key | Type | Default | Description |
|---|---|---|---|
| `header_schema` | object | — | JSON Schema applied to the request headers, seen as a flat object `{name: first_value}` with lowercase header names. |
| `body_schema` | object | — | JSON Schema applied to the parsed request body. |
| `conditions` | array | — | A [condition expression](../conditions.md) (rules ANDed at the top level, with nested `AND`/`OR`/`NOT` groups and JSONPath body subjects). Evaluated **after** the schema checks. |
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
  conditions:
    - ["http_authorization", "present"]
    - ["$.user.email", "present"]
```

Schemas are compiled once at config load with the `jsonschema` crate — malformed schemas (and non-object schema values, missing schemas, out-of-range `rejected_code`) fail policy compilation, never a live request. `conditions` is parsed at the same time, with the same fail-fast guarantee.

## Behavior

1. **Headers** (when `header_schema` is set) — validated as a single-value object: the *first* value of each header, names lowercased.
2. **Body** (when `body_schema` is set):
   - An empty body is rejected.
   - `application/x-www-form-urlencoded` bodies are decoded into a flat object mirroring `ngx.decode_args`: `a=1&a=2` becomes `{"a": ["1","2"]}`, a bare `flag` (no `=`) becomes `{"flag": true}`, values are percent-decoded.
   - Any other content type is parsed as **JSON**; a body that fails to parse is rejected.
   - The parsed value is validated against `body_schema`.
3. **JSON normalization** — after a successful JSON-body validation the body is re-serialized from the parsed document and the stale `content-length` header is removed, so the JSON that was validated is exactly the JSON the upstream receives (guards against [JSON interoperability](https://bishopfox.com/blog/json-interoperability-vulnerabilities) smuggling). Urlencoded bodies are passed through unchanged.
4. **Conditions** (when `conditions` is set) — evaluated last, against the (possibly re-serialized) request. A false result rejects the request the same way a schema failure does, with message `"request conditions not satisfied"` (overridden by `rejected_msg`, like any other rejection).

On any rejection the plugin writes `rejected_code` plus the JSON body `{"error": "validation_failed", "message": <rejected_msg or validator/condition detail>}` onto `context.response` and exits through the `denied` port.

The plugin does not write to `context.message`.

## Limitations

- Headers validate the **first** value of multi-valued headers. Schemas that assert array-typed header values will not match.
- Secret-reference (`$secret://`) indirection for schemas is not supported — schemas are literal objects in the node config.

## Ports

`request-validation` declares three output ports: `success`, `denied` (a schema-validation or condition rejection is prepared), and `error` (never actually used — the plugin never fails; malformed schemas and malformed conditions fail at config load, not at request time). Like `success`, `denied` is a mandatory port: the policy compiler rejects any policy that leaves it unwired. Wire `request-validation.denied` straight to `client` so the prepared rejection reaches the caller instead of continuing into `upstream`:

```yaml
edges:
  - from: request-validation.success
    to: upstream.in
  - from: request-validation.denied
    to: client.in
```
