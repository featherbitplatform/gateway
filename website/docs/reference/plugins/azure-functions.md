---
title: azure-functions
description: Forward the request to an Azure Function endpoint with the function key headers, and return its reply as the gateway response.
---

<span className="plugin-chip" style={{'--chip-color': '#0062ad'}}>azure-functions</span>

Forwards the request to an Azure Function endpoint and returns the function's status, headers, and body as the gateway response. It **replaces the upstream**: place it where an `upstream` node would go and wire its `success` port straight to `client.in`.

## Configuration

| Key | Type | Default | Description |
|---|---|---|---|
| `function_uri` | string | **required** | Azure Function URL to invoke. |
| `authorization` | object | — | Function-key credentials (see below). |
| `authorization.apikey` | string | — | Sent as the `x-functions-key` header. |
| `authorization.clientid` | string | — | Sent as the `x-functions-clientid` header. |
| `ssl_verify` | bool | `true` | Verify TLS certificates on the callout. |
| `timeout` | integer (ms) | `3000` | Whole-call deadline (connect + request + response body). |

```yaml
type: azure-functions
config:
  function_uri: https://app.azurewebsites.net/api/HttpTrigger
  authorization:
    apikey: ${AZURE_FUNCTION_KEY}
  ssl_verify: true
  timeout: 3000
```

Config load fails if `function_uri` is missing or empty.

## Behavior

The plugin forwards the client's method, headers, query string, and body to `function_uri`, overriding `Host` with the endpoint's authority. Unless the client already supplied `x-functions-key`/`x-functions-clientid`, the configured `apikey`/`clientid` are added as those headers. On success it populates `context.response` with the function's status, headers, and body and exits through the `success` port — which should be wired to `client.in`, since this node stands in for the upstream. The function's status is passed through as-is.

A callout failure returns the Context along with an error so the graph engine routes through the `error` port; the error is appended to `context.errors`:

| Code | Status | When |
|---|---|---|
| `AZURE_FUNCTIONS_CALLOUT_ERROR` | 504 | The callout exceeded `timeout`. |
| `AZURE_FUNCTIONS_CALLOUT_ERROR` | 503 | Connecting to or exchanging with the endpoint failed. |
| `AZURE_FUNCTIONS_CALLOUT_ERROR` | 502 | The request could not be built (e.g. invalid `function_uri`). |

The plugin does not read or write `context.message`.
