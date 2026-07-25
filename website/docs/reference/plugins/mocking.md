---
title: mocking
description: Respond with a configured mock instead of proxying upstream — status, headers, templated body, and optional delay.
---

<span className="plugin-chip" style={{'--chip-color': '#f59e0b'}}>mocking</span>

Responds with a configured mock instead of proxying upstream. Useful for stubbing APIs during development and testing, or for serving canned responses on routes with no backend yet.

## Configuration

Validated at config load; `response_example` is required.

| Key | Type | Default | Description |
|---|---|---|---|
| `response_example` | string | **required** | Response body; supports `$var` interpolation (e.g. `$uri`, `$arg_name`, `$http_<header>`). |
| `response_status` | integer 100–599 | `200` | Mock status code. |
| `content_type` | string | `application/json;charset=utf8` | Base type (before `;`) must be one of `application/json`, `application/xml`, `text/plain`, `text/html`, `text/xml`. |
| `response_headers` | map `{name: value}` or array `[{name, value}]` | — | Extra response headers; string values support `$var` interpolation. Names are lowercased. |
| `with_mock_header` | boolean | `true` | Adds `x-mock-by: featherbit-mocking` to the response. |
| `delay` | number (seconds, may be fractional) | `0` | Async sleep before responding. |
| `response_schema` | — | **rejected** | Not implemented (see limitations); configs that set it fail at load. |

```yaml
type: mocking
config:
  response_status: 200
  content_type: application/json;charset=utf8
  response_example: '{"user": "$arg_name", "path": "$uri"}'
  response_headers:
    x-mock-env: staging
  delay: 0.2
```

## Behavior

The plugin sleeps for `delay` (if set), then writes the full mock onto `context.response`: the status, the `content-type` header, `x-mock-by: featherbit-mocking` (unless disabled), each configured header, and the interpolated body. It never fails at execution time; the `error` port is never taken.

### Terminal wiring

A mocking node always answers the request itself, so it is **terminal by intent**: it returns `Ok` with the response prepared, and its **`success` port must be wired straight to `client.in`** — never to an `upstream` (which would overwrite the mock with a real backend response).

```yaml
nodes:
  - { id: mock, type: mocking, config: { response_example: '{"ok": true}' } }
edges:
  - { from: listener.out, to: mock.in }
  - { from: mock.success, to: client.in }
```

To mock only some requests, put a conditional node (e.g. `workflow` or `fault-injection`) in front and route only the chosen slice into the mocking node.

## Limitations

- `response_schema` (random body generation from a JSON schema) is not implemented; configs that set it are rejected at load, making `response_example` required.
- The mock header value is `featherbit-mocking`.
- `delay` accepts fractional seconds.
