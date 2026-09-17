---
title: store-incr
---

# store-incr

Atomically increments a counter in a declared [store](../../guides/configuration.md).

```yaml
- id: count-retry
  type: store-incr
  config:
    store: sessions
    key: "oidc-retry:{{client.ip}}"
    ttl_seconds: 300
    name: retry_count
```

## Config

| Key | Type | Required | Default | Meaning |
|---|---|---|---|---|
| `store` | string | yes | — | Name of a declared `stores:` entry |
| `key` | string (templated) | yes | — | Counter key |
| `by` | integer | no | `1` | Amount to add; may be negative |
| `ttl_seconds` | integer | no | — (no expiry) | Expiry applied **only when the key is created** (unless `refresh_ttl`); `0` is rejected as a config error |
| `refresh_ttl` | bool | no | `false` | Re-arm the expiry on **every** increment, turning the counter into a sliding window |
| `name` | string | yes | — | `context.message` key receiving the new value; readable as `$msg_<name>` |

:::note[Two different windows, and the default is the safe one]
With `refresh_ttl: false` (the default) the key expires a fixed time after it
**first appears**: "N events since the first one". This is what a retry bound
needs — an expiry that refreshed on every increment could be held open
indefinitely by the very client it limits, and the bound would never reset.

With `refresh_ttl: true` the expiry is pushed back out on each increment:
"N events within `ttl_seconds` of **each other**". That is the sliding window
you want for burst throttling, where a quiet period should clear the counter.

`refresh_ttl: true` without `ttl_seconds` is a config error — there is no
expiry to refresh, and accepting it silently would hide a typo.
:::

## Ports

| Port | Kind | Meaning |
|---|---|---|
| `success` | success | The counter was incremented; its new value is in `context.message` |
| `error` | error | The store could not be reached, the key rendered empty, `ttl_seconds` was invalid, or the key held a non-numeric value |

:::note[The TTL applies at creation, not on every increment]
A counter that bounds retries must expire a fixed time after it **first
appears**. If the expiry were refreshed on each increment, a client that keeps
retrying would keep the counter alive and the bound would never reset.
:::

`INCRBY` and the conditional `EXPIRE` run as a single Lua script, so the check
and the expiry write are atomic and cost one round trip — the same approach
`src/stores/counter.rs` takes for its window arithmetic.

## Keys are namespaced

Every key is stored as `<store key_prefix>:kv:<your key>`, so policy keys cannot
collide with the session, rate-limit and ACME keys that share the same store.

## Errors

| Code | When |
|---|---|
| `STORE_ERROR` | The store could not be reached |
| `STORE_VALUE_INVALID` | The key holds a value that is not an integer — a data problem, not an outage |
| `STORE_KEY_INVALID` | `key` rendered to an empty string (would put every request on one shared key) |

## Worked example: bounding OIDC retries

A CSRF/state mismatch on the OIDC callback is usually a stale login tab, and a
fresh flow fixes it transparently. Retrying forever would loop, so the retry is
counted server-side and capped at three.

```yaml
# on the oidc.denied path
- id: count-retry
  type: store-incr
  config:
    store: sessions
    key: "oidc-retry:{{client.ip}}"
    ttl_seconds: 300
    name: retry_count

- id: under-cap
  type: condition
  config:
    conditions: [["msg_retry_count", "<=", 3]]

- id: relogin
  type: redirect
  config: { ret_code: 302, uri: "/" }
```

Wire `count-retry.success → under-cap.in`, `under-cap.true → relogin.in`, and
`under-cap.false → client.in` so the fourth failure surfaces the original error
instead of looping. `redirect` declares `success` and `redirect` as its own
mandatory-wired outcome ports (`src/plugins/ports.rs`); with a `uri` configured
(as here) it always exits on `redirect`, but `success` still has to go
somewhere or the policy fails to compile — wire both `relogin.redirect →
client.in` and `relogin.success → client.in`. On a successful login, clear the
counter:

```yaml
- id: clear-retries
  type: store-delete
  config:
    store: sessions
    key: "oidc-retry:{{client.ip}}"
```

This replaces the cookie-based guard it grew out of: a client can clear its own
cookie, but not a key in the store.

## See also

[store-get](./store-get.md) · [store-set](./store-set.md) · [store-delete](./store-delete.md)
