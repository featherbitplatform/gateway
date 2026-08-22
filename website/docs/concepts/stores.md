---
title: Shared Stores & Sessions
description: Named redis/valkey connections powering cluster-accurate rate limiting and revocable server-side sessions.
---

A **store** is a named Redis/Valkey connection declared once, top-level in `gateway.yaml` under `stores:`, and referenced by name from any plugin config that needs cluster-shared state. It is the same "declare once, reference by name" shape as [shared plugin configs](plugin-configs.md), applied to a stateful backend instead of a config profile. Two features currently consume it: `policy: redis` on [`limit-count`](../reference/plugins/limit-count.md) (and the `workflow` limit-count action) for cluster-accurate rate limiting, and `session.storage: redis` on five interactive auth plugins for revocable server-side sessions.

## Declaring a store

```yaml
stores:
  - name: sessions-redis
    type: redis                      # or: valkey (alias, same backend)
    description: "Shared session + counter store"
    url: ${REDIS_URL:-redis://127.0.0.1:6379}
    password: ${REDIS_PASSWORD:-}    # optional; user/password also accepted in the URL
    key_prefix: fb                   # optional, default "fb"
    connect_timeout_ms: 2000         # optional, default 2000
    tls:                             # optional, for rediss:// / private CAs
      ca_cert_path: /etc/ssl/redis-ca.pem
```

| Field | Type | Default | Notes |
|---|---|---|---|
| `name` | string | — | Unique; referenced by plugin config (`store: <name>` / `session.store: <name>`). |
| `type` | string | — | `redis` or `valkey` — aliases for the same RESP backend; both are CI-tested. |
| `description` | string | — | Optional, shown in the UI's Stores editor. |
| `url` | string | — | `redis://` or `rediss://`. Stored **raw**: `${ENV}` placeholders resolve only when the store's client is built (config-apply time), so the Admin API and UI never serve a resolved secret — the same rule as every other `gateway.yaml` resource. |
| `password` | string | — | Optional; overrides any password embedded in `url`. Same raw-`${ENV}` treatment. |
| `key_prefix` | string | `fb` | Namespace prefix applied to every key the store writes. |
| `connect_timeout_ms` | integer | `2000` | Applied to the client and to `ping`. |
| `tls.ca_cert_path` | string | — | PEM CA bundle for a `rediss://` store with a private CA. |
| `topology` | string | `standalone` | Reserved for Sentinel/Cluster. **v1 accepts only `standalone`** and rejects any other value at config load. |
| `urls` | list | — | Reserved for the Sentinel/Cluster endpoint list. **Rejected at config load in v1** — declare `url` instead. |

A client is built lazily and reused across reloads: one connection per store, rebuilt only when that store's resolved config actually changes. Unknown store names, unresolvable env vars, or a bad URL fail policy compilation or config validation with a descriptive error — never at request time. Deleting a store still referenced by a plugin config or a policy node is rejected (`409`, naming the referrers) — see [Managing at runtime](#managing-at-runtime).

The Redis client sits behind the default-on `redis-store` cargo feature. A binary built with `--no-default-features` (or without `redis-store` explicitly) fails config load on any declared `redis`/`valkey` store with a "built without redis-store" error, and answers `ping` with `501`.

## What uses stores

**Cluster-accurate rate limiting.** [`limit-count`](../reference/plugins/limit-count.md) and the `workflow` limit-count action take `policy: redis` + `store: <name>` in place of the default `policy: local`. Counting runs as an atomic increment-and-check against the store, so every gateway instance shares one window instead of each keeping its own — switching from `local` also changes window boundaries from first-request-aligned (per instance) to wall-clock-aligned (`now / time_window`, shared cluster-wide). `limit-count` additionally accepts `allow_degradation` (default `false`, reject on backend failure); the `workflow` action has no such flag and always rejects through `error` on a backend failure.

**Server-side sessions.** Five interactive auth plugins — [`openid-connect`](../reference/plugins/openid-connect.md), [`cas-auth`](../reference/plugins/cas-auth.md), [`authz-casdoor`](../reference/plugins/authz-casdoor.md), [`dingtalk-auth`](../reference/plugins/dingtalk-auth.md), [`feishu-auth`](../reference/plugins/feishu-auth.md) — take a `session:` block:

```yaml
session:
  storage: redis          # cookie (default) | redis
  store: sessions-redis   # required when storage != cookie
```

`storage: cookie` is the unchanged, default behavior (the whole session payload sealed into the client-side cookie). `storage: redis` switches the plugin onto the store — see [Server-side sessions](#server-side-sessions) below.

## Server-side sessions

In `session.storage: redis` mode, the cookie shrinks to a random 128-bit id (`HttpOnly; Secure; SameSite=Lax`, unchanged attributes). The session payload — the same bytes that would otherwise be sealed into the cookie — is encrypted with the plugin's existing cookie sealer **before** being written to the store, keyed by that id; the store itself never sees a plaintext token. Alongside the sealed blob, a small **unencrypted** `SessionMeta` record (subject, plugin type, policy/route name, created-at, expires-at) is written for the operator-facing surface — see [Managing at runtime](#managing-at-runtime).

The transient auth-flow cookie (pre-login OAuth/OIDC state and nonce) always stays client-side, in both storage modes — it predates the session and must not depend on the store being reachable.

**Failure is never silent.** A store error on any session operation — read, write, lock — routes out the node's `error` port as a `503` (`SESSION_STORE_ERROR`), never a `401`. Treating a store outage as "logged out" would send every affected user into a login redirect whose callback also can't persist a session — a redirect loop. There is deliberately no fail-open mode for sessions.

**Refresh coordination.** `openid-connect` additionally supports lock-coordinated token refresh in redis mode (`session.refresh`, default `true`): before refreshing, a plugin instance takes a short-lived per-session lock; the winner refreshes and writes the updated session, the loser re-reads the (now-fresh) session and proceeds without refreshing. `session.storage: cookie` never attempts a refresh — an IdP-side refresh failure falls back to a fresh login, same as an ordinary expired cookie-mode session, and is deliberately not routed through the 503 store-error path.

**dingtalk-auth / feishu-auth.** Both plugins had their session/redirect flow restored on this same opt-in basis, supporting both `cookie` and `redis` storage — a first request exchanges the code and establishes a session, later requests skip the provider callout, and a request with neither a code nor a session gets a 302 to `redirect_uri`. This is a **breaking port-spec change**: both plugins now mandatory-wire a `redirect` output port, the same as the other three session plugins — an existing policy using either node without a `redirect` edge fails to recompile until one is added, even though the stateless default path never actually takes it.

**Key precedence.** All five plugins also accept flat `session_storage` / `session_store` config keys as a fallback for the nested `session.storage` / `session.store` form. When both are present, **the nested form wins**.

## Managing at runtime

**Admin API** (see the [Admin API guide](../guides/admin-api.md) for the full endpoint reference):

| Method | Path | Notes |
|---|---|---|
| `GET` | `/api/stores` | List named stores. |
| `POST` | `/api/stores` | Create a store; `409` if the name exists. |
| `GET` | `/api/stores/:name` | Raw config — `${ENV}` placeholders are never resolved. |
| `PUT` | `/api/stores/:name` | Create or update (upsert). |
| `DELETE` | `/api/stores/:name` | `409 {"error":"in_use","referrers":[...]}` if a plugin config or policy node still references it. |
| `POST` | `/api/stores/:name/ping` | Resolve, connect, `PING`; returns latency and server version. `502` unreachable, `504` timeout, `501` on a headless (no `redis-store`) build. |
| `GET` | `/api/sessions?store=&subject=&plugin=&limit=&cursor=` | List session metadata (never payloads), cursor-paginated. |
| `DELETE` | `/api/sessions/:store/:id` | Revoke one session. |
| `DELETE` | `/api/sessions?store=&subject=` | Revoke every session for a subject; returns `{"revoked": N}`. |

No endpoint ever returns session content — only the unencrypted `SessionMeta` envelope.

**Web UI.** A **Stores** editor in the resource sidebar lists and edits stores (name, type, url, password, TLS, tuning) with a "Ping" action against the endpoint above; deleting a store surfaces the `409` referrer list instead of failing silently. Wherever plugin config takes a store name — `limit-count`'s `store` field, and `session.store` on the five session plugins — the node inspector renders a dropdown fed from `GET /api/stores`, gated by the adjacent `policy`/`storage` selector. A **Sessions panel**, opened from a footer button, lists session metadata filtered by store/subject/plugin, with per-row revoke and "revoke all for subject…" (both behind a confirmation step); on a headless build it shows a notice instead of a list, inferred from the `501` the list call gets back.

**Revocation only reaches store-backed sessions.** `session.storage: cookie` sessions are not listed and cannot be revoked — the whole payload lives in the client's cookie, so the gateway has nothing server-side to delete. This is by design, not a gap: moving to `storage: redis` is the opt-in that buys revocability.

## Observability

| Metric | Type | Labels | Description |
|---|---|---|---|
| `gateway_counter_store_errors_total` | counter | `store` | Backend errors from `policy: redis` counting (`limit-count` / `workflow` limit-count) per named store. |
| `gateway_session_store_errors_total` | counter | `store` | Backend errors from a `session.storage: redis` operation (read, write, lock) per named store — every one of these also produced a `503` on the triggering request. |

Both follow the same `gateway_*_total` counter convention as the rest of the [Prometheus metrics](../guides/observability.md#prometheus-metrics) surface.
