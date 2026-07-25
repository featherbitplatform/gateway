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

On a wolf-server **200** the context passes through the **success** port. On any other status — or a missing/unparseable token, or a callout failure — the plugin rejects through the **error** port:

- `context.response.status_code` = `401` (callout transport failures set `500`)
- Body: `{"error": "forbidden", "message": "<reason>"}` (the wolf-server `reason` field when available) with `content-type: application/json`
- Error code appended to `context.errors`: `WOLF_RBAC_DENIED`

## Limitations

- **Token-check only.** Only the per-request authorization path (`access_check`) is implemented. Interactive login endpoints — proxying credential exchange to wolf-server to mint or rotate RBAC tokens (login, change-password, user-info) — are a session/login concern and are **not** implemented. Obtain tokens directly from wolf-server (or a dedicated login route).
- **Config-driven server.** `server` / `ssl_verify` / `header_prefix` are read from the node config, not from a consumer's auth configuration; the token's appid is still used verbatim as the `appID` request argument. No consumer is required or attached.
- **No retry loop.** `access_check` is issued as a single call bounded by `timeout_ms`; a `5xx` response is not retried.
- **Identity headers are set on the upstream request only.** They are injected onto the proxied request (lowercased, per the gateway's header convention), not mirrored onto the client response.
