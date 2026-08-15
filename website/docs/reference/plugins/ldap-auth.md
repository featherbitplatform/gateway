---
title: ldap-auth
description: Authenticate HTTP Basic credentials against an LDAP server via a simple bind.
---

<span className="plugin-chip" style={{'--chip-color': '#0ea5e9'}}>ldap-auth</span>

Authenticates a request's HTTP Basic credentials against an LDAP server. The bind DN is assembled from the presented username and the configured directory location, then a **simple bind** is attempted with that DN and the presented password: a successful bind authenticates the request. Place it early in the request pipeline, before the upstream node.

## Configuration

| Key | Type | Default | Description |
|---|---|---|---|
| `base_dn` | string | — (**required**) | Base DN the bind DN is built under, e.g. `ou=users,dc=example,dc=org`. |
| `ldap_uri` | string | — (**required**) | LDAP server URI, e.g. `ldap://ldap.example.org:389` (or `ldaps://…:636`). |
| `uid` | string | `cn` | RDN attribute that prefixes the username in the bind DN. |
| `use_tls` | boolean | `false` | Negotiate StartTLS on the connection after connecting. |
| `tls_verify` | boolean | `false` | Verify the LDAP server's TLS certificate. |
| `realm` | string | `ldap` | Realm advertised in the `WWW-Authenticate: Basic` challenge. |
| `timeout_ms` | number | `10000` | Whole-operation deadline for the connect + bind. |

```yaml
- id: auth
  type: ldap-auth
  config:
    base_dn: ou=users,dc=example,dc=org
    ldap_uri: ldap://ldap.example.org:389
    uid: cn
    use_tls: false
    tls_verify: false
```

`base_dn` and `ldap_uri` are required; a missing or blank value is rejected at config load.

## Behavior

1. The `Authorization: Basic <base64(user:pass)>` header is parsed. The scheme match is case-insensitive; the payload is standard-base64 decoded, split on the first `:`, and **all whitespace is stripped from both the username and the password**.
2. The bind DN is assembled as `<uid>=<username>,<base_dn>` (e.g. `cn=alice,ou=users,dc=example,dc=org`).
3. The plugin connects to `ldap_uri` and attempts a simple bind with the DN and password, under the `timeout_ms` deadline.

On a successful bind the context passes through the **success** port, with the authenticated username exposed to downstream nodes as `context.message["user"]`.

On a missing header, malformed credentials, empty username/password, or a bind rejection, the plugin rejects and exits through the **`denied`** port:

- `context.response.status_code` = `401`
- `WWW-Authenticate: Basic realm="<realm>"` challenge header
- Body: `{"error": "unauthorized", "message": "<reason>"}` with `content-type: application/json`

A connection error or a connect+bind timeout is a genuine **infrastructure failure**, not a credential decision — it stays on the **error** port instead, with error code `LDAP_AUTH_FAILED`. The prepared response mirrors the `denied` shape exactly (same `401`, same `WWW-Authenticate` challenge, same JSON body and `content-type`), so what the client sees is unchanged if the error edge leads to `client`.

## Ports

`ldap-auth` declares three output ports: `success`, `denied` (a deliberate credential rejection is prepared), and `error` (the LDAP server was unreachable, or the connect+bind deadline elapsed — a genuine infra failure, not a rejection). Like `success`, `denied` is a mandatory port: the policy compiler rejects any policy that leaves it unwired. Wire `ldap-auth.denied` straight to `client` so the prepared `401` reaches the caller instead of continuing into `upstream`; wire `error` to an error-handler (or leave it unwired for the default 500) since that path represents the node failing to do its job, not a policy decision:

```yaml
edges:
  - from: ldap-auth.success
    to: upstream.in
  - from: ldap-auth.denied
    to: client.in
```

## Behavior notes

- **Bind-auth only, not search-then-bind.** The bind DN is built directly from `uid` + `base_dn`; the plugin never performs a directory search to locate the user's entry — `uid`/`base_dn` fully determine the DN.
- **No consumer resolution.** The plugin performs pure bind authentication: on a successful bind the request continues and the username is written to `context.message["user"]`; no consumer identity is attached and no consumer is required.
- **Empty passwords are rejected up front.** A blank password would otherwise trigger an *unauthenticated* (anonymous) bind that many directories accept, silently authenticating anyone. featherbit rejects empty username/password before contacting the server.
- **`use_tls` negotiates StartTLS** on the given URI. For implicit TLS, use an `ldaps://` URI directly.
