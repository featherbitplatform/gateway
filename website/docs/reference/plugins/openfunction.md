---
title: openfunction
description: Forward the request to an OpenFunction endpoint with optional Basic auth, and return its reply as the gateway response.
---

<span className="plugin-chip" style={{'--chip-color': '#00b4a0'}}>openfunction</span>

Forwards the request to an OpenFunction endpoint and returns the function's status, headers, and body as the gateway response. It **replaces the upstream**: place it where an `upstream` node would go and wire its `success` port straight to `client.in`.

## Configuration

| Key | Type | Default | Description |
|---|---|---|---|
| `function_uri` | string | **required** | OpenFunction URL to invoke. |
| `authorization` | object | — | Optional service-token credential (see below). |
| `authorization.service_token` | string | — | Sent as `Authorization: Basic <base64(service_token)>`. |
| `ssl_verify` | bool | `true` | Verify TLS certificates on the callout. |
| `timeout` | integer (ms) | `3000` | Whole-call deadline (connect + request + response body). |

```yaml
type: openfunction
config:
  function_uri: http://openfunction.svc/default/hello
  authorization:
    service_token: ${OPENFUNCTION_TOKEN}
  ssl_verify: true
  timeout: 3000
```

Config load fails if `function_uri` is missing or empty.

## Behavior

The plugin forwards the client's method, headers, query string, and body to `function_uri`, overriding `Host` with the endpoint's authority. When `authorization.service_token` is set, an `Authorization: Basic <base64(service_token)>` header is added (replacing any client-supplied `Authorization`). On success it populates `context.response` with the function's status, headers, and body and exits through the `success` port — which should be wired to `client.in`, since this node stands in for the upstream. The function's status is passed through as-is.

A callout failure returns the Context along with an error so the graph engine routes through the `error` port; the error is appended to `context.errors`:

| Code | Status | When |
|---|---|---|
| `OPENFUNCTION_CALLOUT_ERROR` | 504 | The callout exceeded `timeout`. |
| `OPENFUNCTION_CALLOUT_ERROR` | 503 | Connecting to or exchanging with the endpoint failed. |
| `OPENFUNCTION_CALLOUT_ERROR` | 502 | The request could not be built (e.g. invalid `function_uri`). |

The plugin does not read or write `context.message`.
