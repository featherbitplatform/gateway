---
title: dingtalk-auth
description: Validates a DingTalk authorization code against DingTalk's OAuth API and attaches the resolved user identity.
---

<span className="plugin-chip" style={{'--chip-color': '#1493ff'}}>dingtalk-auth</span>

Validates a DingTalk authorization **code** by exchanging it — through DingTalk's OAuth API — for the calling user's identity, then attaches that identity to the request for downstream nodes. A request whose code cannot be resolved to a DingTalk user is rejected with `401`.

The code is read from a request header (default `X-DingTalk-Code`), falling back to a query parameter (default `code`). The plugin fetches an app-level access token (cached in-process, ~7000s TTL) and calls DingTalk's `getuserinfo` endpoint with the code.

:::note Limitations — token validation only
featherbit is **stateless with no session store**, so this plugin implements a **token-validation** flow: every request must carry a code, which is validated against DingTalk on each request. There is no session cookie caching the resolved userinfo between requests, and no 302 redirect when the code is absent — a missing code is simply rejected. Config keys that would only serve a session flow (`secret`, `secret_fallbacks`, `redirect_uri`, `cookie_expires_in`) are not supported. The app-level access token *is* cached in-process, so only the userinfo call happens per request.
:::

## Configuration

| Key | Type | Default | Description |
|---|---|---|---|
| `app_key` | string | — | **Required.** DingTalk application key. |
| `app_secret` | string | — | **Required.** DingTalk application secret. |
| `code_header` | string | `X-DingTalk-Code` | Header the authorization code is read from first (matched case-insensitively). |
| `code_query` | string | `code` | Query parameter the code falls back to when the header is absent. |
| `access_token_url` | string | `https://api.dingtalk.com/v1.0/oauth2/accessToken` | DingTalk access-token endpoint. |
| `userinfo_url` | string | `https://oapi.dingtalk.com/topapi/v2/user/getuserinfo` | DingTalk userinfo endpoint. |
| `set_userinfo_header` | boolean | `true` | Base64-encode the resolved userinfo into the `X-Userinfo` request header for the upstream. |
| `timeout` | integer (ms) | `6000` | Per-callout timeout. |
| `ssl_verify` | boolean | `true` | Verify DingTalk's TLS certificate. |

```yaml
- id: auth
  type: dingtalk-auth
  config:
    app_key: ${DINGTALK_APP_KEY}
    app_secret: ${DINGTALK_APP_SECRET}
    code_header: X-DingTalk-Code
```

## Behavior

The code is read from `code_header`, then `code_query`. With no code, the request is rejected. Otherwise the plugin obtains an access token (from cache or by calling `access_token_url`) and POSTs the code to `userinfo_url`; DingTalk's `errcode: 0` with a `result` object indicates success.

Any client-supplied `X-Userinfo` header is stripped before authentication.

On success the context passes through the **success** port:

- `context.message["dingtalk_userinfo"]` = the resolved DingTalk `result` object
- `context.message["user_id"]` = `userid` (or `unionid`), when present
- `X-Userinfo` request header = base64-encoded userinfo JSON (when `set_userinfo_header`)

On a missing code, a rejected code (`errcode != 0`), or a callout failure, the plugin rejects and routes through the **error** port:

- `context.response.status_code` = `401`
- Body: `{"error": "unauthorized", "message": "<reason>"}` with `content-type: application/json`
- Error code appended to `context.errors`: `DINGTALK_AUTH_FAILED`
