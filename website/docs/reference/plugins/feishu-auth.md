---
title: feishu-auth
description: Validates a Feishu/Lark authorization code via Feishu's OAuth API and attaches the resolved user identity.
---

<span className="plugin-chip" style={{'--chip-color': '#00c2a8'}}>feishu-auth</span>

Validates a Feishu / Lark authorization **code** by exchanging it — through Feishu's OAuth v2 token endpoint — for a user access token, then calls Feishu's userinfo endpoint to resolve the calling user's identity and attaches it to the request. A code that cannot be resolved is rejected with `401`.

The code is read from a request header (default `X-Feishu-Code`), falling back to a query parameter (default `code`).

:::note Limitations — token validation only
featherbit is **stateless with no session store**, so this plugin implements **token validation only**: every request must carry a code, which is exchanged and validated on each request. There is no session cookie caching the exchanged token/userinfo, and no interactive 302 redirect when a code is absent — the keys that would only serve such a flow (`secret`, `secret_fallbacks`, `redirect_uri`, and `cookie_expires_in`) are not supported. `auth_redirect_uri` **is** supported because it is part of the `authorization_code` token-exchange body, not an interactive redirect.
:::

## Configuration

| Key | Type | Default | Description |
|---|---|---|---|
| `app_id` | string | — | **Required.** Feishu application id. |
| `app_secret` | string | — | **Required.** Feishu application secret. |
| `auth_redirect_uri` | string | — | **Required.** The `redirect_uri` registered with Feishu; sent in the `authorization_code` token-exchange body and must match the one used to obtain the code. |
| `code_header` | string | `X-Feishu-Code` | Header the code is read from first (matched case-insensitively). |
| `code_query` | string | `code` | Query parameter the code falls back to. |
| `access_token_url` | string | `https://open.feishu.cn/open-apis/authen/v2/oauth/token` | Feishu token endpoint. |
| `userinfo_url` | string | `https://open.feishu.cn/open-apis/authen/v1/user_info` | Feishu userinfo endpoint. |
| `set_userinfo_header` | boolean | `true` | Base64-encode the resolved userinfo into the `X-Userinfo` request header for the upstream. |
| `timeout` | integer (ms) | `6000` | Per-callout timeout. |
| `ssl_verify` | boolean | `true` | Verify Feishu's TLS certificate. |

```yaml
- id: auth
  type: feishu-auth
  config:
    app_id: ${FEISHU_APP_ID}
    app_secret: ${FEISHU_APP_SECRET}
    auth_redirect_uri: https://app.example.com/callback
```

## Behavior

The code is read from `code_header`, then `code_query`. With no code, the request is rejected. Otherwise the plugin POSTs an `authorization_code` grant to `access_token_url` to obtain a user access token, then GETs `userinfo_url` with `Authorization: Bearer <token>`; Feishu's `code: 0` with a `data` object indicates success.

Any client-supplied `X-Userinfo` header is stripped before authentication.

On success the context passes through the **success** port:

- `context.message["feishu_userinfo"]` = the resolved Feishu `data` object
- `context.message["user_id"]` = `user_id` (or `open_id` / `union_id`), when present
- `X-Userinfo` request header = base64-encoded userinfo JSON (when `set_userinfo_header`)

On a missing code, a rejected code / token (non-zero `code`), or a callout failure, the plugin rejects and routes through the **error** port:

- `context.response.status_code` = `401`
- Body: `{"error": "unauthorized", "message": "<reason>"}` with `content-type: application/json`
- Error code appended to `context.errors`: `FEISHU_AUTH_FAILED`
