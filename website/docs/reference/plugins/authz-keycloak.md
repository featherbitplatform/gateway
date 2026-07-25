---
title: authz-keycloak
description: Keycloak UMA 2.0 permission check — validates the caller's bearer token against configured permissions via the token endpoint.
---

<span className="plugin-chip" style={{'--chip-color': '#0ea5e9'}}>authz-keycloak</span>

Authorizes requests against a [Keycloak](https://www.keycloak.org/) authorization server using the **UMA 2.0 permission check**. For each request the plugin takes the caller's bearer access token and asks Keycloak whether it grants the configured permissions, using the `urn:ietf:params:oauth:grant-type:uma-ticket` grant with `response_mode=decision`. A `200` decision allows the request; anything else denies it. Place it after any token-issuing step and before the upstream node.

## Configuration

| Key | Type | Default | Description |
|---|---|---|---|
| `token_endpoint` | string | — | **Required.** Keycloak token endpoint URL (`.../protocol/openid-connect/token`). |
| `client_id` | string | — | **Required.** OAuth client id, sent as the UMA `audience`. |
| `permissions` | array of strings | `[]` | Requested permissions, each `resource` or `resource#scope`. |
| `policy_enforcement_mode` | string | `ENFORCING` | `ENFORCING` denies when `permissions` is empty; `PERMISSIVE` allows without a callout. |
| `http_method_as_scope` | boolean | `false` | Append the request method as the scope of each permission. |
| `ssl_verify` | boolean | `true` | Verify the endpoint's TLS certificate. |
| `timeout` | integer (ms) | `3000` | Callout timeout. |

```yaml
- id: authz
  type: authz-keycloak
  config:
    token_endpoint: https://kc.example.com/realms/myrealm/protocol/openid-connect/token
    client_id: my-api
    permissions: ["Default Resource#read"]
    policy_enforcement_mode: ENFORCING
    ssl_verify: true
    timeout: 3000
```

## Behavior

The bearer token is read from the `Authorization` header (a missing `Bearer ` prefix is added). The plugin POSTs `application/x-www-form-urlencoded` body `grant_type=urn:ietf:params:oauth:grant-type:uma-ticket&audience=<client_id>&response_mode=decision&permission=<...>` to `token_endpoint`, forwarding the caller's token as the `Authorization` header.

- `200` from Keycloak → **success** port, request continues.
- Any other status, a missing token, or a callout error → **error** port:
  - `context.response.status_code` = `403`
  - Body: `{"error":"access_denied","error_description":"not_authorized"}`
  - Error code appended to `context.errors`: `AUTHZ_KEYCLOAK_DENIED`

With an empty `permissions` list, `ENFORCING` denies and `PERMISSIVE` allows (no callout).

## Limitations

The plugin covers the static-permission UMA check. The following features are intentionally **not** supported:

- **Discovery.** Endpoints are not resolved from a discovery URL; `token_endpoint` must be configured directly.
- **`lazy_load_paths` / resource resolution.** No dynamic URI→resource lookups against the resource-registration endpoint, and no service-account (`client_credentials`) token acquisition. Only statically configured `permissions` are checked.
- **Password grant.** `password_grant_token_generation_incoming_uri` token minting is not supported.
- **Caching & redirects.** No discovery/token caching and no `access_denied_redirect_uri` (307) redirect — denials are always `403`.
- **Error status normalization.** Every non-`200` outcome — including Keycloak's own `401`/`403`/`4xx` responses — maps to `403` with code `AUTHZ_KEYCLOAK_DENIED`.
