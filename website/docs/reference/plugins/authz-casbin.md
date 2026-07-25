---
title: authz-casbin
description: Embedded Casbin ABAC/RBAC authorization — evaluates each request against a local model + policy, no network calls.
---

<span className="plugin-chip" style={{'--chip-color': '#7c3aed'}}>authz-casbin</span>

Authorizes requests with an embedded [Casbin](https://casbin.org/) enforcer. The model and policy are loaded once at config load — from files on the gateway host or from inline strings — and every request is evaluated locally with no network calls. Place it after authentication (so a consumer identity is available) and before the upstream node.

For each request the plugin builds a Casbin request tuple `(subject, object, action)`:

- **subject** — the authenticated consumer (`consumer.name` in `context.message`) if present, else the value of a configured header, else `"anonymous"`.
- **object** — the request path.
- **action** — the request method.

`enforce((subject, object, action))` decides the outcome.

## Configuration

One of the two source pairs is required.

| Key | Type | Default | Description |
|---|---|---|---|
| `model_path` | string | — | Path to a Casbin model file on the gateway host. Requires `policy_path`. |
| `policy_path` | string | — | Path to a Casbin policy (CSV) file on the gateway host. Requires `model_path`. |
| `model` | string | — | Inline Casbin model config text. Requires `policy`. |
| `policy` | string | — | Inline Casbin policy (CSV) text. Requires `model`. |
| `username_header` | string | `x-user` | Header the subject is read from when no consumer identity is attached; compared case-insensitively. |

```yaml
# inline model + policy
- id: authz
  type: authz-casbin
  config:
    username_header: x-user
    model: |
      [request_definition]
      r = sub, obj, act
      [policy_definition]
      p = sub, obj, act
      [role_definition]
      g = _, _
      [policy_effect]
      e = some(where (p.eft == allow))
      [matchers]
      m = g(r.sub, p.sub) && r.obj == p.obj && r.act == p.act
    policy: |
      p, admin, /data, GET
      g, alice, admin
```

```yaml
# model + policy files on the gateway host
- id: authz
  type: authz-casbin
  config:
    model_path: /etc/featherbit/model.conf
    policy_path: /etc/featherbit/policy.csv
    username_header: x-user
```

## Behavior

The enforcer is compiled eagerly at config load; a malformed model or policy fails fast (config is rejected) rather than at request time.

On success the context passes through the **success** port unchanged. On a denied decision the plugin routes through the **error** port:

- `context.response.status_code` = `403`
- Body: `{"message":"Access Denied"}` with `content-type: application/json`
- Error code appended to `context.errors`: `AUTHZ_CASBIN_DENIED`

## Behavior notes

- **Subject resolution.** The subject is resolved in order: the attached consumer identity (`consumer.name`) when present, then the header named by `username_header` (default `x-user`), then `"anonymous"` — integrating with featherbit's consumer model.
- **Per-node model/policy.** There is no global plugin-metadata layer; supply the model/policy per node via the file paths or inline strings.
- **Enforcer construction.** Casbin's enforcer is async to build; featherbit constructs it at load on a dedicated short-lived thread with its own Tokio runtime, then shares it read-only (`Arc<Enforcer>`) across requests.
