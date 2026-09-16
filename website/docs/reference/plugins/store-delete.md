---
title: store-delete
---

# store-delete

Removes a key from a declared [store](../../guides/configuration.md).

```yaml
- id: clear-retries
  type: store-delete
  config:
    store: sessions
    key: "retry:{{request.cookies.fb_sid}}"
```

## Config

| Key | Type | Required | Default | Meaning |
|---|---|---|---|---|
| `store` | string | yes | — | Name of a declared `stores:` entry |
| `key` | string (templated) | yes | — | Key to delete |

## Ports

| Port | Kind | Meaning |
|---|---|---|
| `success` | success | The key was removed, or it did not exist |
| `error` | error | The store could not be reached, or the key rendered empty |

## Deleting an absent key is a success

There is no `miss` port here, unlike [store-get](./store-get.md). Deleting a
key that was never set, or has already expired, is not a failure — it is the
state the caller wanted. A `miss` port would be mandatory-wired in every
policy that clears state, to signal something almost no caller acts on
differently from ordinary success.

## Keys are namespaced

Every key is stored as `<store key_prefix>:kv:<your key>`, so policy keys cannot
collide with the session, rate-limit and ACME keys that share the same store.

## Errors

| Code | When |
|---|---|
| `STORE_ERROR` | The store could not be reached |
| `STORE_KEY_INVALID` | `key` rendered to an empty string (would put every request on one shared key) |

## See also

[store-get](./store-get.md) · [store-set](./store-set.md) · [store-incr](./store-incr.md)
