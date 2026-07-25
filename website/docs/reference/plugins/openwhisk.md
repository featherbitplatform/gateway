---
title: openwhisk
description: Invoke an Apache OpenWhisk action (blocking, result inlined) and return its reply as the gateway response.
---

<span className="plugin-chip" style={{'--chip-color': '#3c873a'}}>openwhisk</span>

Invokes an Apache OpenWhisk action and returns the action's reply as the gateway response. It **replaces the upstream**: place it where an `upstream` node would go and wire its `success` port straight to `client.in`.

## Configuration

| Key | Type | Default | Description |
|---|---|---|---|
| `api_host` | string | **required** | OpenWhisk API host, e.g. `https://ow.example.com`. |
| `service_token` | string | **required** | `user:pass` action token, sent base64-encoded as HTTP Basic auth. |
| `action` | string | **required** | Action name to invoke. |
| `namespace` | string | `_` | OpenWhisk namespace. |
| `package` | string | — | Package the action belongs to. |
| `result` | bool | `true` | Request `result=true` (inline the action result rather than the full activation record). |
| `ssl_verify` | bool | `true` | Verify TLS certificates on the callout. |
| `timeout` | integer (ms) | `3000` | Whole-call deadline, also passed as the action `timeout` query parameter. |

```yaml
type: openwhisk
config:
  api_host: https://ow.example.com
  service_token: ${OPENWHISK_TOKEN}
  namespace: guest
  action: hello
  result: true
  ssl_verify: true
  timeout: 3000
```

Config load fails if `api_host`, `service_token`, or `action` is missing/empty.

## Behavior

The plugin POSTs the client request body as the action parameters to `<api_host>/api/v1/namespaces/<namespace>/actions/<package/><action>?blocking=true&result=<result>&timeout=<ms>` with `Authorization: Basic <base64(service_token)>` and `Content-Type: application/json`. On success it populates `context.response` and exits through the `success` port — which should be wired to `client.in`, since this node stands in for the upstream.

### Response mapping

OpenWhisk returns a JSON envelope. An action may return just a body, or set `statusCode` and `headers` explicitly:

- `statusCode` (when present) becomes the response status; otherwise the transport status is used.
- `headers` (when present) are applied to the response.
- `body` (string or nested JSON) becomes the response body; when absent, the raw envelope is passed through.

A non-JSON, non-empty envelope fails the node with `503`.

A callout failure or an unparseable envelope returns the Context along with an error routed through the `error` port; the error is appended to `context.errors`:

| Code | Status | When |
|---|---|---|
| `OPENWHISK_CALLOUT_ERROR` | 504 | The callout exceeded `timeout`. |
| `OPENWHISK_CALLOUT_ERROR` | 503 | Connecting/exchanging failed, or the envelope was not valid JSON. |
| `OPENWHISK_CALLOUT_ERROR` | 502 | The request could not be built. |

The plugin does not read or write `context.message`.
