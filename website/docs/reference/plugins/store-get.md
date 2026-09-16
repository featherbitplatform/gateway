---
title: store-get
---

# store-get

Reads a key from a declared [store](../../guides/configuration.md) into `context.message`.

```yaml
- id: read-retries
  type: store-get
  config:
    store: sessions
    key: "retry:{{request.cookies.fb_sid}}"
    name: retry_count
    json: false
```

## Config

| Key | Type | Required | Default | Meaning |
|---|---|---|---|---|
| `store` | string | yes | — | Name of a declared `stores:` entry |
| `key` | string (templated) | yes | — | Key to read |
| `name` | string | yes | — | `context.message` key to write; readable as `$msg_<name>` |
| `json` | bool | no | `false` | Parse a JSON object and flatten its top-level fields |

## Ports

| Port | Kind | Meaning |
|---|---|---|
| `success` | success | The key existed; its value is in `context.message` |
| `miss` | outcome | The key does not exist; nothing was written |
| `error` | error | The store could not be reached, or the value was unusable |

`miss` and `error` are deliberately separate. A store outage must not look like
"nothing recorded" — otherwise a policy takes its happy path during exactly the
incident where that is most wrong.

## Keys are namespaced

Every key is stored as `<store key_prefix>:kv:<your key>`, so policy keys cannot
collide with the session, rate-limit and ACME keys that share the same store. A
key written by another system is not reachable.

## `json: true` flattens, it does not nest

`context.message` is a flat namespace — `{{message.a.b}}` resolves the literal
key `"a.b"` rather than traversing into an object. So a parsed object has its
top-level fields written as separate keys:

```yaml
# stored value: {"tier":"gold","seats":3}   with name: profile
# {{message.profile.tier}}  -> gold
# {{message.profile.seats}} -> 3
```

Nested objects and arrays are written as their own JSON value under one dotted
key, not recursed into. A JSON **scalar** is written under `name` unchanged, so
`$msg_<name>` keeps working.

Flattened keys are readable through `{{message.…}}` but **not** through legacy
`$msg_<name>`, where a dot ends the token.

A value that is not valid JSON exits `error` with `STORE_VALUE_INVALID` rather
than falling back to the raw string, which would make a malformed value
indistinguishable from a good one downstream.

## Errors

| Code | When |
|---|---|
| `STORE_ERROR` | The store could not be reached |
| `STORE_VALUE_INVALID` | `json: true` and the value is not valid JSON |
| `STORE_KEY_INVALID` | `key` rendered to an empty string (would put every request on one shared key) |

## See also

[store-set](./store-set.md) · [store-incr](./store-incr.md) · [store-delete](./store-delete.md)
