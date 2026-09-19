---
title: store-set
---

# store-set

Writes a key into a declared [store](../../guides/configuration.md), with an
optional TTL.

```yaml
- id: record-seen
  type: store-set
  config:
    store: sessions
    key: "seen:{{request.headers.x-session}}"
    value: "1"
    ttl_seconds: 300
```

## Config

| Key | Type | Required | Default | Meaning |
|---|---|---|---|---|
| `store` | string | yes | — | Name of a declared `stores:` entry |
| `key` | string (templated) | yes | — | Key to write |
| `value` | string (templated) | yes | — | Value to write |
| `ttl_seconds` | integer | no | — (no expiry) | Expiry in seconds; `0` is rejected as a config error |

## Ports

| Port | Kind | Meaning |
|---|---|---|
| `success` | success | The key was written |
| `error` | error | The store could not be reached, the key rendered empty, or `ttl_seconds` was invalid |

## Keys are namespaced

Every key is stored as `<store key_prefix>:kv:<your key>`, so policy keys cannot
collide with the session, rate-limit and ACME keys that share the same store.

## No TTL means the key persists indefinitely

Omitting `ttl_seconds` is not "pick a sensible default" — it means the key is
written with no expiry at all. A policy that writes a per-request key (one
keyed by a session id, a request id, or anything else that is effectively
unique) without a TTL will grow the store without bound, one key per request,
forever. Set `ttl_seconds` on any key whose value should eventually go away.

`ttl_seconds: 0` is rejected at compile time rather than treated as "no
expiry" — the two readings are too easy to confuse, and omitting the field is
the unambiguous way to say "no expiry".

## Errors

| Code | When |
|---|---|
| `STORE_ERROR` | The store could not be reached |
| `STORE_KEY_INVALID` | `key` rendered to an empty string (would put every request on one shared key) |

## See also

[store-get](./store-get.md) · [store-incr](./store-incr.md) · [store-delete](./store-delete.md)
