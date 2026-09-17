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
| `extend_ttl_seconds` | integer | no | — (do not extend) | Reading the key re-arms its expiry to this many seconds; `0` is rejected as a config error |

:::note[`extend_ttl_seconds` turns the store into a cache]
Without it, a read is a plain `GET` and the key's expiry keeps counting down
regardless of use. With it, the read becomes `GETEX key EX n`: the entry stays
alive while it is being used and disappears a fixed time after the **last
access**. One round trip, so there is no read-then-write race between gateway
instances.

A miss stays a miss. `GETEX` on a key that does not exist returns nothing and
creates nothing, so a keep-alive read never manufactures the entries it is
meant to keep warm.

**It will also give an expiry to a key that had none.** A value written by
`store-set` *without* `ttl_seconds` persists forever; the first extending read
makes it ephemeral. That is the point of the option rather than a surprise —
but if a key is meant to be durable, do not read it through a node configured
this way.

Requires redis 6.2 or newer (`GETEX`). Both `redis:7` and `valkey:8` are
covered by CI.
:::

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

Flattened keys are readable through `{{message.…}}`, and through the legacy
`${msg_<name>}` brace form too — the name runs to the closing `}`, so
`${msg_profile.tier}` resolves the dotted key. Only the bare `$msg_<name>`
form stops at the dot (`src/vars/mod.rs`'s `interpolate`), so a dotted
flattened key needs the brace form there.

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
