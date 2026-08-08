---
title: wolf-rbac
description: Authorize requests against a wolf-server by checking the caller's RBAC token per request path and method.
---

<span className="plugin-chip" style={{'--chip-color': '#f97316'}}>wolf-rbac</span>

Authorizes each request against a [wolf](https://github.com/iGeeky/wolf) RBAC server. The plugin extracts the caller's wolf RBAC token, parses it, and asks the wolf-server whether that token may perform the request's method on the request's path. On allow it copies the returned user identity into request headers and `context.message`; on deny it rejects.

## Configuration

| Key | Type | Default | Description |
|---|---|---|---|
| `server` | string | `http://127.0.0.1:12180` | wolf-server base URL; `/wolf/rbac/access_check` is called on it. |
| `appid` | string | `unset` | Application id sent as the `appID` argument when a token carries none. |
| `header_prefix` | string | `X-` | Prefix for the identity headers injected on an allowed request (`<prefix>UserId`, `<prefix>Username`, `<prefix>Nickname`). |
| `ssl_verify` | boolean | `false` | Verify wolf-server's TLS certificate on the callout. |
| `timeout_ms` | number | `10000` | Whole-call deadline for the wolf-server callout. |

```yaml
- id: rbac
  type: wolf-rbac
  config:
    server: http://wolf-server:12180
    appid: restful
    header_prefix: X-
    ssl_verify: false
```

## Behavior

1. The RBAC token is extracted in precedence order: the `rbac_token` **query argument**, then the `Authorization` **header**, then the `X-RBAC-Token` **header**, then the `x-rbac-token` **cookie**.
2. The token is parsed as `V1#<appid>#<wolf_token>`. A wrong version prefix or wrong segment count is rejected. A token that omits its appid falls back to the configured `appid`.
3. The plugin calls `GET <server>/wolf/rbac/access_check` with query arguments `appID`, `resName` (the request path), `action` (the request method), and `clientIP`, sending the `x-rbac-token` header.
4. The response body's `data.userInfo` (`id`, `username`, `nickname` — `nickname` falling back to `username`) is, when present, propagated onto the request as `<prefix>UserId` / `<prefix>Username` / `<prefix>Nickname` headers (nickname percent-encoded) and into `context.message["user"]` and `context.message["wolf_rbac.user_id"]`.

On a wolf-server **200** the context passes through the **success** port. On any other status, or a missing/unparseable token, the plugin rejects and exits through the **`denied`** port:

- `context.response.status_code` = `401`
- Body: `{"error": "forbidden", "message": "<reason>"}` (the wolf-server `reason` field when available) with `content-type: application/json`

A wolf-server callout that fails outright (network error, unreachable) is a genuine **infrastructure failure**, not a rejection — it stays on the **error** port instead, with `context.response.status_code = 500` and error code `WOLF_RBAC_UPSTREAM_ERROR`.

## Ports

`wolf-rbac` declares three output ports: `success`, `denied` (a deliberate rejection is prepared — missing/unparseable token, or a wolf-server deny decision), and `error` (the wolf-server callout itself failed). Like `success`, `denied` is a mandatory port: the policy compiler rejects any policy that leaves it unwired. Wire `wolf-rbac.denied` straight to `client` so the prepared `401` reaches the caller instead of continuing into `upstream`; wire `error` to an error-handler (or leave it unwired for the default 500):

```yaml
edges:
  - from: wolf-rbac.success
    to: upstream.in
  - from: wolf-rbac.denied
    to: client.in
```

## Limitations

- **Token-check only.** Only the per-request authorization path (`access_check`) is implemented. Interactive login endpoints — proxying credential exchange to wolf-server to mint or rotate RBAC tokens (login, change-password, user-info) — are a session/login concern and are **not** implemented. Obtain tokens directly from wolf-server (or a dedicated login route).
- **Config-driven server.** `server` / `ssl_verify` / `header_prefix` are read from the node config, not from a consumer's auth configuration; the token's appid is still used verbatim as the `appID` request argument. No consumer is required or attached.
- **No retry loop.** `access_check` is issued as a single call bounded by `timeout_ms`; a `5xx` response is not retried.
- **Identity headers are set on the upstream request only.** They are injected onto the proxied request (lowercased, per the gateway's header convention), not mirrored onto the client response.
