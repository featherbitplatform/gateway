---
title: oas-validator
description: Validate incoming requests against an inline OpenAPI 3 specification, rejecting non-conforming requests before they reach the upstream.
---

<span className="plugin-chip" style={{'--chip-color': '#10b981'}}>oas-validator</span>

Validates the incoming request against an OpenAPI 3 (OAS 3) specification and rejects non-conforming requests with a configurable status code. Place it before the `upstream` node. The spec is supplied inline as a JSON object; every operation's request-body schema is compiled once at config load.

## Configuration

| Key | Type | Default | Description |
|---|---|---|---|
| `spec` | object | — (**required**) | The OpenAPI 3 document as an inline JSON object. Its `paths` are walked at config load. |
| `rejected_code` | integer 400–599 | `400` | Response status for rejected requests. |
| `rejected_msg` | string | — | Fixed message returned instead of the per-violation detail. |

```yaml
type: oas-validator
config:
  rejected_code: 400
  spec:
    openapi: 3.0.0
    info: { title: demo, version: "1.0" }
    components:
      schemas:
        User:
          type: object
          required: [name]
          properties:
            name: { type: string, minLength: 1 }
    paths:
      /users/{id}:
        get:
          parameters:
            - { name: verbose, in: query, required: true, schema: { type: string } }
            - { name: x-trace, in: header, required: true, schema: { type: string } }
        post:
          requestBody:
            required: true
            content:
              application/json:
                schema: { $ref: '#/components/schemas/User' }
```

At config load the plugin walks `paths`; for every operation it compiles the `application/json` `requestBody` schema (with the `jsonschema` crate, embedding the spec's `components` so local `$ref`s resolve) and indexes the `required` query/header parameters. A non-object `spec`, a missing `paths`, a malformed path item/operation, or an uncompilable schema fails policy compilation, never a live request. An out-of-range `rejected_code` is also rejected here.

## Behavior

1. **Operation match.** The request's method + path is matched against the spec, with OpenAPI path templating: `/users/{id}` matches `/users/123`. Segment counts must be equal; a `{param}` segment matches any single non-empty segment; the matching operation with the most literal segments wins (so an exact path beats a templated one).
2. **No match → pass through.** If no operation matches (unknown path, or a method not declared on a matched path), the request passes through untouched — it is not the validator's job to 404.
3. **Required parameters.** For a matched operation, every `required: true` query and header parameter must be present (header names compared case-insensitively). A missing one is rejected. Path parameters are inherently present once the path matches; cookie parameters are out of scope.
4. **Request body.** If the operation's `requestBody` is `required: true` and the body is empty, the request is rejected. When an `application/json` schema exists and the body is non-empty and JSON (content-type `application/json`, or absent), the body is parsed and validated against the schema; a non-JSON body or a schema violation is rejected.

On any rejection the plugin writes `rejected_code` plus the JSON body `{"error": "oas_validation_failed", "message": <rejected_msg or violation detail>}` onto `context.response` and fails with error code `OAS_VALIDATION_FAILED`, routing the Context through the `error` port. On success the Context passes through unchanged; the plugin does not write to `context.message`.

## Limitations

- **Inline-JSON spec only.** `spec` is an inline OpenAPI JSON object. A JSON-*string* form, a remote `spec_url` fetch, a YAML/file loader, and secret-reference indirection are **out of scope**.
- **Validation scope.** Validated: presence of `required` query/header parameters and the `application/json` `requestBody` schema (with local `#/components/...` `$ref` resolution). Not covered: response validation, parameter *type/format* schema validation and coercion, `oneOf`/`anyOf` operation selection, cookie parameters, and external-document `$ref`s.
- `skip_*` toggles, `verbose_errors`, and `reject_if_not_match` are not modelled: a matched operation is always validated and violations are always rejected with `rejected_code`. A request that matches no operation is never rejected.
