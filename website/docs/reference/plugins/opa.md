---
title: opa
description: Delegates the access decision to an external Open Policy Agent server, sending a request input document and enforcing the returned allow/deny decision.
---

<span className="plugin-chip" style={{'--chip-color': '#7c3aed'}}>opa</span>

Delegates the authorization decision for each request to an external [Open Policy Agent](https://www.openpolicyagent.org/) server. The plugin builds an OPA *input document* describing the request (and, optionally, the matched consumer), POSTs it to `<host>/v1/data/<policy>`, and enforces the decision under `result`. Place it early in the request pipeline, before the upstream node.

## Configuration

| Key | Type | Default | Description |
|---|---|---|---|
| `host` | string | — | OPA base URL (e.g. `http://opa:8181`). **Required**; a trailing `/` is trimmed. |
| `policy` | string | — | Decision path appended as `/v1/data/<policy>`. **Required**; a leading `/` is trimmed. |
| `ssl_verify` | boolean | `true` | Verify TLS certificates for `https` callouts. |
| `timeout` | integer (ms) | `3000` | Whole-call callout deadline (connect + request + response). |
| `with_consumer` | boolean | `false` | Include a `consumer` object (built from `context.message`'s `consumer.*` keys) in the input document. |
| `with_route` | boolean | `false` | Accepted for config compatibility but a **no-op** — featherbit has no route object. |
| `with_service` | boolean | `false` | Accepted for config compatibility but a **no-op** — featherbit has no service object. |
| `send_headers_upstream` | array of strings | — | OPA-response header names copied onto the request forwarded upstream on allow. A configured name absent from the OPA response removes any client-supplied value. |

```yaml
- id: authz
  type: opa
  config:
    host: http://opa:8181
    policy: example/allow
    with_consumer: true
    send_headers_upstream: [x-user-id]
    ssl_verify: true
    timeout: 3000
```

## Behavior

The plugin POSTs the following input document to `<host>/v1/data/<policy>`:

```json
{
  "input": {
    "type": "http",
    "request": {
      "scheme": "http", "method": "GET", "host": "example.com", "port": 8080,
      "path": "/api/users", "headers": { "...": "..." }, "query": { "...": "..." }
    },
    "var": { "remote_addr": "10.0.0.7", "remote_port": 5555, "timestamp": 1710000000 },
    "consumer": { "name": "alice", "auth_type": "key-auth" }
  }
}
```

Headers and query parameters collapse to a string when single-valued and to an array when repeated. The `consumer` object is present only when `with_consumer: true` and a consumer is attached. The `var` block omits `server_addr` / `server_port`, which featherbit does not track.

The OPA reply's `result` object determines routing:

- **`allow: true`** → the request passes through the **success** port. If `send_headers_upstream` is set, each named header from `result.headers` is copied onto the request forwarded upstream; a named header absent from the OPA response is removed.
- **`allow: false` or missing** → the request is denied, exiting through the **`denied`** port. OPA-supplied `result.status_code` (or `result.status`) sets the response status (default `403`), `result.headers` are copied onto the response, and `result.reason` becomes the response body (objects are JSON-encoded).
- **Callout failure** (timeout / transport error) → a genuine infrastructure failure: exits through the **error** port with status `403` and error `OPA_ERROR` (block-by-default).
- **Unparseable response** (not JSON, or missing `result`) → also a genuine infrastructure failure: exits through the **error** port with status `503` and error `OPA_ERROR`.

## Ports

`opa` declares three output ports: `success`, `denied` (an `allow: false`/missing decision is prepared), and `error` (the callout failed, or its response could not be parsed). Like `success`, `denied` is a mandatory port: the policy compiler rejects any policy that leaves it unwired. Wire `opa.denied` straight to `client` so the prepared response reaches the caller instead of continuing into `upstream`:

```yaml
edges:
  - from: opa.success
    to: upstream.in
  - from: opa.denied
    to: client.in
```
