# Session Storage & Shared Stores — Design

**Date:** 2026-08-21
**Status:** Approved design, pending implementation plan

## Motivation

The gateway was designed stateless: interactive SSO plugins seal the whole
session payload into an encrypted client-side cookie
(`src/plugins/util/cookie_session.rs`), which scales horizontally with no
coordination. That design has known limits, all inherent to client-stored
sessions:

1. **No server-side revocation** — a session cannot be killed before its
   embedded expiry (no logout-everywhere, no admin kill-switch, no
   stolen-cookie response).
2. **~4 KB cookie cap** — large ID tokens / access+refresh pairs can overflow.
3. **Single shared secret** — compromise reads/mints every session; rotation
   invalidates all sessions at once.
4. **No cross-instance refresh coordination** — concurrent token refreshes
   from multiple gateway instances race against the IdP.
5. **Two APISIX ports were amputated by the stateless design** —
   `dingtalk-auth` and `feishu-auth` are session plugins upstream; our ports
   dropped the session/cookie/redirect machinery and validate against the
   provider on **every request**.

Separately, rate limiting is per-instance today: `src/ratelimit/mod.rs`
defines a pluggable `CounterStore` with only a `local` backend; the `redis`
policy is documented as planned. In an N-instance cluster the effective limit
is ~N × the configured limit.

Note on what this does **not** fix: CSRF. Cookie-auto-attachment applies
equally to client-side payloads and server-side session IDs; mitigations
remain `SameSite=Lax; HttpOnly; Secure` (already the sealer default) and the
`csrf` plugin. etcd stays config-only — it is a Raft consensus store, wrong
for per-request session/counter churn.

## Scope

**In:**

- A new shared, named **`stores:`** top-level resource in `gateway.yaml`
  (Redis/Valkey connections), reusable by any feature.
- **Server-side sessions** (opaque-ID cookie + sealed-at-rest payload in the
  store) as an opt-in `session.storage: redis` mode for:
  `openid-connect`, `cas-auth`, `authz-casdoor`, and **restored** session
  support (both cookie and redis modes) for `dingtalk-auth` and `feishu-auth`.
- **Refresh coordination** via a per-session store lock (openid-connect).
- **Redis `CounterStore`** implementing the planned `policy: redis` for
  `limit-count` and the `workflow` limit-count action.
- **Admin API + UI**: stores CRUD + ping, session list/revoke endpoints,
  a Stores editor, store pickers in node config, and a Sessions panel.

**Out (documented follow-ups):**

- `rate-limit` (token bucket) and `limit-conn` distributed backends — their
  semantics don't map onto `incr_fixed_window`; they will reuse `stores:`.
- `proxy-cache` Redis backend — different serialization/size concerns; own
  design.
- `jwt-auth` revocation denylist (the light "sealed cookie + denylist" mode).
- `hmac-auth` store-backed nonce replay prevention (no APISIX parity
  pressure; enhancement).
- Authorization **decision caching** for `authz-keycloak` / `opa` /
  `forward-auth` — staleness/security trade-offs deserve their own design.
- `wolf-rbac` login/session flow — a full login implementation, not a cache
  restoration.
- `api-breaker` cluster-shared breaker state — easy on the store, but
  per-instance breakers are often correct (each instance observes its own
  connectivity); opt-in mode at most.
- A `store:get/set/incr` API for the `script` (Lua) plugin — cluster-shared
  state for user scripts; likely the highest-leverage follow-up, making the
  store a platform primitive.
- Store-backed sticky sessions / traffic affinity — consistent hashing
  already solves affinity statelessly; out unless a concrete need appears.
- Redis Sentinel/Cluster topologies (v1 is standalone + auth + TLS), **but
  v1 is cluster-ready by construction** — see "Cluster readiness" below.

## Chosen approach

**Opaque session ID + sealed-at-rest content.** In `storage: redis` mode the
cookie shrinks to a random 128-bit session ID (unchanged attributes:
`HttpOnly; Secure; SameSite=Lax`). The session payload — the same bytes today
sealed into the cookie — is encrypted with the existing `CookieSealer`
**before** being written to the store. Rejected alternatives:

- *Plain server-side sessions* (unencrypted payload in Redis): simpler, but
  tokens readable in Redis is a security regression.
- *Sealed cookie + revocation denylist*: fails the 4 KB, token-off-client,
  and session-listing motivations; kept as a future light mode for jwt-auth.

## 1. The `stores:` resource

New top-level resource in `gateway.yaml`, peer to `routes` / `policies` /
`consumers` / `supernodes` / `plugin_configs`:

```yaml
stores:
  - name: sessions-redis
    type: redis            # or: valkey (alias, same backend)
    url: ${REDIS_URL:-redis://127.0.0.1:6379}
    password: ${REDIS_PASSWORD:-}   # optional; user/password also accepted in the URL
    key_prefix: fb                  # optional, default "fb"
    # Amendment (as shipped): v1 uses a single auto-reconnecting multiplexed
    # connection, not a pool — there is no `pool_size` field.
    connect_timeout_ms: 2000        # optional
    tls:                            # optional, for rediss:// / private CAs
      ca_cert_path: /etc/ssl/redis-ca.pem
```

- **Valkey**: protocol-compatible; `type: valkey` is a first-class alias
  resolving to the same backend. Both are CI-tested (see Testing).
- **Interpolation**: stored raw; `${ENV}` placeholders resolve when the
  store's client is built (config-apply time), same rule as all `gateway.yaml`
  resources. The Admin API/UI never serve resolved secrets.
- **Resolution**: plugins reference stores by name; unknown name,
  unresolvable env var, or bad URL fail config validation with a descriptive
  error — never at request time.
- **One client per store**: a `StoreRegistry` in `SharedState` holds one
  connection pool per named store, rebuilt only when that store's config
  changes on reload; all referencing plugins/counters share it.
- **etcd mode**: new key family `<prefix>/stores/<name>` in
  `src/config_store/etcd.rs`. Release note: an older build sharing the prefix
  garbage-collects the unknown family on its next commit (existing
  documented behavior).
- **Cargo feature**: the Redis client (`redis` crate, tokio + rustls) sits
  behind a default-on `redis-store` feature. Without it, declaring a
  `redis`/`valkey` store fails config load with a clear
  "built without redis-store" error.
- Deleting a store still referenced by any plugin config or policy node is
  rejected (409 naming the referrers).

### Cluster readiness

Sentinel and Cluster ship as follow-ups, but two v1 decisions make them
purely additive later (no breaking config change, no data migration):

1. **Hash-tagged keys from day one.** All per-session keys embed the id as a
   Redis Cluster hash tag — `sess:{<id>}`, `sess:{<id>}:meta`,
   `lock:{<id>}` — so a session's keys always share a slot and multi-key
   operations stay valid under Cluster. Counter keys are single-key already.
2. **Topology-ready schema.** v1 accepts `url` (standalone). The schema
   reserves `topology: standalone|sentinel|cluster` (default `standalone`)
   and `urls: [...]` for the follow-up; validation rejects non-standalone
   topologies in v1 with a "not yet supported" error rather than ignoring
   them.

Known follow-up costs, recorded for planning: Sentinel = crate feature +
failover discovery config + a failover test (cheap; standalone semantics).
Cluster = `cluster-async` client, per-node `SCAN` for session listing, and a
cluster container in the CI matrix.

## 2. `SessionStore` and plugin integration

New module `src/sessions/`, mirroring `src/ratelimit/`:

```rust
#[async_trait]
pub trait SessionStore: Send + Sync {
    async fn put(&self, id: &SessionId, sealed: &[u8], ttl: Duration,
                 meta: &SessionMeta) -> Result<(), StoreError>;
    async fn get(&self, id: &SessionId) -> Result<Option<Vec<u8>>, StoreError>;
    async fn delete(&self, id: &SessionId) -> Result<(), StoreError>;
    async fn delete_subject(&self, subject: &str) -> Result<u64, StoreError>;
    async fn list(&self, filter: &SessionFilter) -> Result<Vec<SessionMeta>, StoreError>;
    async fn try_lock(&self, id: &SessionId, ttl: Duration) -> Result<bool, StoreError>;
    async fn unlock(&self, id: &SessionId) -> Result<(), StoreError>;
}
```

`SessionMeta` is the small **unencrypted** envelope for the operator surface:
subject, plugin type, route/policy name, created-at, expires-at. The token
payload arrives already sealed — the store never sees plaintext tokens.

**Redis key layout** (under the store's `key_prefix`):

| Key | Value | TTL |
|---|---|---|
| `sess:{<id>}` | sealed blob | session TTL (Redis enforces expiry; the blob's embedded expiry stays as defense in depth) |
| `sess:{<id>}:meta` | JSON `SessionMeta` | same |
| `subj:{<sha256(subject)>}` | SET of session ids | refreshed on write; members lazily pruned on read |
| `lock:{<id>}` | refresh lock via `SET NX PX` | short (~10 s) |

Braces around the id are literal Redis Cluster **hash tags** (see "Cluster
readiness"): a session's keys always hash to the same slot.

`list` uses `SCAN` over `sess:*:meta` (cursor-paginated); an index can come
later if scale demands.

**Plugin config** (all five session plugins):

```yaml
session:
  storage: redis          # cookie (default) | redis
  store: sessions-redis   # required when storage != cookie
```

In `redis` mode, exactly one seam changes per plugin: where the payload was
sealed into the cookie, generate a random 128-bit id, `put` the sealed
payload, set the cookie to the id; where the cookie was opened, `get` by id,
then unseal. The **transient auth-flow cookie** (pre-login OAuth state/nonce)
stays client-side in both modes — it predates the session and must not depend
on the store.

**dingtalk-auth / feishu-auth restoration**: reinstate the upstream session
behavior (first request exchanges the code and establishes a session; later
requests skip the provider callout; 302 to `redirect_uri` when neither code
nor session is present), supporting both `cookie` and `redis` storage. The
config keys dropped by the stateless ports (`secret`, `redirect_uri`,
`cookie_expires_in` etc.) return, mapped onto the shared `session:` block;
module docs' Deviations sections are updated.

**Refresh coordination** (openid-connect): before refreshing, `try_lock(id)`.
Winner refreshes, `put`s updated session, unlocks. Loser re-`get`s (usually
finds fresh tokens) and proceeds without refreshing — no waiting loop, no
thundering herd against the IdP.

**Failure semantics**: a `StoreError` on any session operation routes out the
node's **`error` port with a 503** — never 401 (treating an outage as
"logged out" would redirect every user to an IdP whose callback also cannot
persist a session: a redirect loop). There is deliberately **no fail-open
mode for sessions**.

## 3. Redis `CounterStore`

`RedisCounterStore` implements the existing `CounterStore` trait and
registers in `CounterStoreRegistry` under `redis`.

**Plugin config** (`limit-count`, `workflow` limit-count action):

```yaml
policy: redis             # local (default) | redis
store: sessions-redis     # any declared redis/valkey store
allow_degradation: false  # optional, default false
```

**Atomicity**: increment-and-check runs as a server-side Lua script
(`EVALSHA`, loaded once per connection):

```
key = {prefix}:cnt:{window_start}:{counter_key}
INCR key; if first increment, PEXPIRE to window end; return (count, pttl)
```

`window_start = now / window` from wall clock, so all instances agree on
boundaries. The `(count, pttl)` result maps onto the existing
`WindowResult { allowed, remaining, reset, limit }` — plugins change only by
accepting the two new config fields. Documented behavior change when
switching policy: windows become clock-aligned across the cluster rather
than first-request-aligned per instance.

**Failure semantics**: `allow_degradation: false` (default) rejects with the
plugin's configured rejection response when the store is unreachable — a
limit that silently stops limiting is a security hole. `true` fails open
where availability beats accuracy. Errors are logged and counted in
Prometheus (`gateway_counter_store_errors_total{store}` — amendment: gains
the house `gateway_` metric prefix as shipped).

## 4. Admin API + UI

**Admin API** (existing basic-auth middleware):

- `GET/POST/PUT/DELETE /api/stores[/{name}]` — CRUD; responses carry raw
  config (placeholders, never resolved secrets); referenced-delete → 409
  naming referrers.
- `POST /api/stores/{name}/ping` — resolve, connect, `PING`; returns RTT and
  server info (Redis/Valkey version).
- `GET /api/sessions?store=&subject=&plugin=&limit=&cursor=` — lists
  `SessionMeta`, cursor-paginated over `SCAN`.
- `DELETE /api/sessions/{store}/{id}` — revoke one.
- `DELETE /api/sessions?store=&subject=` — logout-everywhere; returns count.
- No endpoint ever returns session *content*.

**UI**:

- **Stores editor** — new resource section (list + form: name, type
  redis/valkey, url, password, TLS, tuning). Placeholders entered/displayed
  verbatim. "Test connection" button drives the ping endpoint; delete
  surfaces the 409 referrer list.
- **Store pickers** — wherever plugin config takes a `store` name
  (`session.store`, limit-count `store`), the node config panel renders a
  dropdown fed by `GET /api/stores`, gated by the `storage`/`policy`
  selector.
- **Sessions panel** — filter by store/subject/plugin; table of session
  metadata with per-row revoke and "revoke all for subject…", both behind
  confirmation. Stated limitation in panel + docs: revocation applies to
  store-backed sessions only; `storage: cookie` sessions remain unrevocable
  by design.

## 5. Testing

**Unit (hermetic)**: an in-memory `FakeSessionStore` drives plugin-side
tests — cookie-shrinks-to-id, seal→put/get→unseal round trip, 503
error-port routing on `StoreError`, refresh-lock winner/loser, transient
flow-cookie stays client-side, dingtalk/feishu session-mode flows.
Config-layer tests: unknown `store:` fails compile descriptively; `${ENV}`
resolves at client-build time (raw in storage); referenced-delete lists
referrers; `type: valkey` aliases.

**Integration (live store)**: suite gated by `FEATHERBIT_TEST_REDIS_URL`
(skipped when unset, `cargo test` stays hermetic). Exercises
`RedisSessionStore` + `RedisCounterStore`: TTL expiry, `delete_subject`
fan-out, SCAN cursor listing, `SET NX` lock semantics, Lua-script atomicity
under concurrency (N tasks × M increments never exceed the limit;
clock-aligned windows). CI runs the suite in a service-container matrix:
`redis:7` **and** `valkey:8` — Valkey support is tested, not assumed.

**End-to-end (Playwright, `e2e/E2E_TESTBOOK.md`)**:

1. Redis-backed OIDC login against the existing Keycloak test realm with
   `storage: redis`: cookie is a bare id; session survives a gateway restart.
2. Revocation: log in, revoke via Sessions panel, next request re-enters the
   login flow.
3. Stores editor: declare a store with an `${ENV}` placeholder in the UI,
   ping it, wire it via the store picker, verify the raw placeholder (not
   the secret) round-trips through the API.

**Build/CI guards**: `--no-default-features` build proves `redis-store`
compiles out; config-load test asserts the "built without redis-store"
error; clippy/tests as usual; the `redis` crate enters the existing
dependency-audit surface automatically.
