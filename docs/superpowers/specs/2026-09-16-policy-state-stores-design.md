# Persisted state for policies — `store-get` / `store-set` / `store-incr` / `store-delete`

**Date:** 2026-09-16
**Status:** approved design, not yet implemented
**Target version:** 0.10.0 (new capability, not a patch)
**Origin:** item 1 of `docs/superpowers/notes/2026-09-15-follow-ups.md`

## 1. Problem

A policy cannot remember anything across requests.

Bounding the OIDC callback retry to a single attempt needed a value that survives a
request. The only cross-request store a policy can write today is a **cookie**, via
`response-rewrite` `headers.add: ["Set-Cookie: …"]`, read back as `cookie_<name>`. That
shipped and works, but it is client-side: size-limited, spoofable, per-browser, and
invisible to any other gateway instance.

Meanwhile `stores:` (redis/valkey) already exist. They are declared in `gateway.yaml`,
validated at config load, CRUD-managed through `/api/stores`, etcd-synced, and consumed
by `limit-count`'s cluster-accurate counters, by server-side sessions, and by ACME
certificate storage. **No node can read or write an arbitrary key.**

The OIDC session is not a substitute. It is owned by `openid-connect`, has no
policy-facing write API, and in the retry case does not exist yet — the whole problem is
that login has not completed.

Two consequences follow, and both showed up in practice:

- Anything stateful gets faked with a cookie, or with `limit-count` used as a counter it
  was not designed to be.
- Anything computed reaches for a Lua `script` node, which until recently had no enforced
  timeout at all. Keeping scripts rare is a safety property, not just a style preference.

## 2. What already exists

This design adds nodes on top of infrastructure that is already in place. It does not add
a backend, a connection pool, or a config surface.

| Piece | Where | Note |
|---|---|---|
| `StoreConfig` | `src/config/gateway.rs:228` | `name`, `type` (`redis`\|`valkey`), `url`, password, `key_prefix` |
| `StoreRegistry` | `src/stores/mod.rs:73` | `client(name) -> Arc<RedisStoreClient>` |
| `RedisStoreClient` | `src/stores/redis_store.rs:28` | `conn()`, `key_prefix()`, `connect_timeout()` |
| Plugin access | `src/plugins/resources.rs:40` | `PluginResources.stores: ArcSwap<StoreRegistry>` |

The consumption pattern is set by `limit-count` (`src/plugins/native/limit_count.rs:130`):
resolve the store **by name at construction time**, hold the resulting `Arc`, and never
touch the registry on the request path. These nodes follow it exactly.

Key namespacing convention is set by ACME storage (`src/acme/storage/redis.rs`): every key
is written under the store's configured `key_prefix`.

## 3. Decisions

| Question | Decision | Rationale |
|---|---|---|
| How many node types? | **Four**: `store-get`, `store-set`, `store-incr`, `store-delete` | Forced by the port model — see §4 |
| Missing key on read | **A dedicated `miss` outcome port** | Makes "key absent" an explicit branch the compiler forces you to wire |
| Atomic increment | **In scope**, as its own node | The motivating use case; read-modify-write would be racy across instances |
| Value shape | **Strings, with opt-in `json: true`** that flattens a JSON object's top-level fields into `message` | `message` is a flat namespace with no nested traversal, so flattening is the only shape it can express — see §5.1 |
| `ttl_seconds` on `store-set` | **Optional**; absent means the key persists | Requiring it would block legitimate durable flags |
| TTL on `store-incr` | **Applied at creation only**, never refreshed | Otherwise sustained traffic keeps a counter alive forever and it never resets |
| Delete semantics | **Idempotent**: `success` + `error`, no `miss` | `DEL` on an absent key is not a failure; a mandatory port firing on "already gone" would be noise in every graph |
| Store outage | **`error` port, `503`, never fail-open** | The rule the five session plugins already follow |
| Key namespace | All keys under `{key_prefix}:kv:`, enforced by a namespace registry with drift tests | Policy keys cannot collide with `acme:`, `sess:` or `cnt:` keys — see §6 |
| Non-redis backends | **Out of scope** | `stores:` is redis/valkey today; nothing here presumes otherwise |

## 4. Why four node types and not one

`PortSpec` is static per node **type** (`src/plugins/ports.rs`):

> One `PortSpec` per plugin type, resolved through `crate::plugins::port_spec` — the single
> source of truth shared by the graph compiler (edge validation), the admin catalog, and by
> extension the UI editor. The `Plugin` trait has no port method at all: the registry match
> in `port_spec` IS the declaration, so a plugin cannot drift from its own ports.

Ports therefore cannot vary with config. A single `store` node taking `op: get|set|incr`
would have to declare `miss` for every op, and `miss` is an `outcome` port — **mandatory
wiring**. Every `store` node in `set` mode would be forced to wire an edge that can never
be taken. The alternative — omitting `miss` and signalling a read miss some other way —
gives up the property that makes the design worth having.

Splitting by operation also matches the existing split between `limit-count` and
`limit-conn`, and keeps each node's config coherent: `value` means nothing to a read,
`by` means nothing to a write.

## 5. The four nodes

### 5.1 `store-get`

```yaml
- id: read-retries
  type: store-get
  config:
    store: sessions
    key: "retry:{{request.cookies.fb_sid}}"
    name: retry_count
    json: false
```

| Key | Type | Required | Meaning |
|---|---|---|---|
| `store` | string | yes | Name of a declared `stores:` entry |
| `key` | string (templated) | yes | Key to read, under the `kv:` namespace |
| `name` | string | yes | `context.message` key to write; readable as `$msg_<name>` |
| `json` | bool | no (`false`) | Parse a JSON **object** value and flatten its top-level fields into `message` (see below) |

**Ports:** `success` (key existed, value written to `message`), `miss` (outcome — key absent;
nothing written to `message`), `error`.

With `json: true` and a value that is not valid JSON, the node exits `error` with
`STORE_VALUE_INVALID`. Silently falling back to the string would make a malformed value
indistinguishable from a well-formed one downstream.

**Why `json` flattens rather than nests.** `context.message` is read through a **flat**
lookup: `message_str` does `ctx.message.get(key)`, and `{{message.a.b}}` resolves the literal
key `"a.b"` rather than traversing into an object (`src/vars/mod.rs:691`, and the
`message.<key>` doc note that "key may itself contain dots"). There is no JSONPath over
`message` either — the JSONPath subjects are request- and response-body only. So writing a
parsed object under a single name would render it as one `to_string()` blob that nothing
downstream can index into.

`json: true` therefore parses the value and writes each **top-level** field as
`message["<name>.<field>"]`, which the existing dotted-key support then makes readable:

```yaml
# stored at kv:profile:u17  ->  {"tier":"gold","seats":3}
- id: read-profile
  type: store-get
  config: { store: sessions, key: "profile:{{request.headers.x-user}}", name: profile, json: true }

# downstream:  {{message.profile.tier}} -> gold     {{message.profile.seats}} -> 3
```

Nested objects and arrays are written as their JSON text under their own flattened key, not
recursed into; one level is what the flat namespace can express honestly. Note this is
reachable through the `{{message.…}}` form but **not** through legacy `$msg_<name>`, where a
dot terminates the token — the plugin page must say so.

A value that parses as a JSON scalar (a bare number, string or boolean) is written under
`name` unchanged, so `$msg_<name>` keeps working for the common counter case.

### 5.2 `store-set`

```yaml
- id: mark-seen
  type: store-set
  config:
    store: sessions
    key: "seen:{{request.headers.x-session}}"
    value: "1"
    ttl_seconds: 300
```

| Key | Type | Required | Meaning |
|---|---|---|---|
| `store` | string | yes | Declared `stores:` entry |
| `key` | string (templated) | yes | Key to write |
| `value` | string (templated) | yes | Value to write |
| `ttl_seconds` | integer | no | Expiry in seconds; absent means the key persists |

**Ports:** `success`, `error`.

`SET key value` with `EX ttl` when `ttl_seconds` is present. A `ttl_seconds` of `0` is a
config error rather than "no expiry" — the two readings are too easy to confuse, and the
way to say "no expiry" is to omit the key.

### 5.3 `store-incr`

```yaml
- id: count-retry
  type: store-incr
  config:
    store: sessions
    key: "retry:{{request.cookies.fb_sid}}"
    by: 1
    ttl_seconds: 300
    name: retry_count
```

| Key | Type | Required | Meaning |
|---|---|---|---|
| `store` | string | yes | Declared `stores:` entry |
| `key` | string (templated) | yes | Counter key |
| `by` | integer | no (`1`) | Amount to add; may be negative |
| `ttl_seconds` | integer | no | Expiry applied **only when the key is created** |
| `name` | string | yes | `context.message` key receiving the new value |

**Ports:** `success`, `error`.

The TTL rule is the point of the node. A counter bounding retries must expire a fixed time
after it first appears; refreshing the expiry on every increment means a client that keeps
retrying keeps the counter alive and the bound never resets. Implemented as one Lua script —
`INCRBY`, then set the expiry only if the key has none — so it is atomic and costs one round
trip, the same approach `src/stores/counter.rs` takes for its window arithmetic.

Incrementing a key holding a non-numeric value exits `error` with `STORE_VALUE_INVALID`.

### 5.4 `store-delete`

```yaml
- id: clear-retries
  type: store-delete
  config:
    store: sessions
    key: "retry:{{request.cookies.fb_sid}}"
```

**Ports:** `success`, `error`.

Deleting a key that does not exist is a success. This is the one place the design
deliberately does *not* mirror `store-get`: a `miss` port here would be mandatory-wired in
every policy that clears state, to signal something almost no caller acts on.

## 6. Key namespacing

Every key is `{key_prefix}:kv:{rendered key}`, where `key_prefix` is the store's configured
prefix (default `fb`). This matches the convention every existing subsystem already follows.

### 6.1 The complete namespace inventory

featherbit writes keys from exactly three places today. There are no others — the only other
redis calls in the codebase are `PING` and `INFO`, which touch no keys.

| Namespace | Keys | Source |
|---|---|---|
| `cnt` | `{p}:cnt:{slot}:{key}` | `src/stores/counter.rs:86` |
| `acme` | `{p}:acme:account`, `{p}:acme:cert:{id}`, `{p}:acme:challenge:{domain}`, `{p}:acme:lease:{id}` | `src/acme/storage/redis.rs:40-51` |
| `sess` | `{p}:sess:{id}`, `{p}:sess:{id}:meta` | `src/sessions/redis.rs:38-42` |

`kv` is unused, so it is free to take.

### 6.2 Making it stay true

Being free today is not the same as being safe tomorrow. Two things could break the
separation, and neither is prevented by choosing a good name:

1. A future subsystem picks `kv`, or a future `store-*` change picks `sess`.
2. A policy key escapes its namespace and names a managed key.

Both are closed structurally rather than by documentation.

**A namespace registry** (`src/stores/namespaces.rs`) becomes the single declaration of every
namespace, with the three existing key builders refactored to use it:

```rust
pub const COUNTERS: &str = "cnt";
pub const ACME: &str = "acme";
pub const SESSIONS: &str = "sess";
/// Policy-written keys (`store-get`/`set`/`incr`/`delete`).
pub const POLICY_KV: &str = "kv";

/// Namespaces owned by featherbit itself. A policy can never address these.
pub const MANAGED: &[&str] = &[COUNTERS, ACME, SESSIONS];
```

Tests assert that `POLICY_KV` is not in `MANAGED`, that all four are pairwise distinct, and —
the part that catches real drift — that each subsystem's **actual key builder** still produces
keys under its declared namespace. A subsystem that changes its prefix, or a new one that
reuses `kv`, fails the build.

**Escape is structurally impossible**, and a test proves it rather than assuming it. The
rendered policy key is a *suffix*: `format!("{prefix}:kv:{rendered}")`. Even an adversarial
rendered value — `../x`, or `:sess:{abc}` from a header a caller controls — produces
`fb:kv::sess:{abc}`, which is not `fb:sess:{abc}`. There is no traversal syntax in redis keys
for it to exploit. The test asserts this against hostile inputs, because `key` is templated
and templates read caller-supplied data.

**An empty rendered key is rejected** with `STORE_KEY_INVALID`. `fb:kv:` stays inside the
namespace so it is not a collision, but a template that silently collapses to nothing would
put every request on one shared key — a template typo becoming a cross-tenant data leak. It
fails loudly instead.

### 6.3 What this costs

A key written by some *other* system is not reachable from a policy. That is accepted for v1.
An escape hatch (`raw_key: true`, say) is easy to add later and should only be added when
something actually needs it — and it would deliberately give up the guarantee above, so it
wants its own decision.

## 7. Failure handling

A store outage — connection refused, timeout, a RESP error — exits the node's `error` port
with a `503` prepared and code `STORE_ERROR`, carrying the store name and the operation in
metadata. It is never swallowed and never silently treated as a miss.

This matters most for `store-get`: `miss` and `error` must stay distinguishable, or a redis
outage would look exactly like "no retries recorded" and a policy would take its
happy-path branch during precisely the incident where that is most wrong.

Per the error-port fallback chain, a node that leaves `error` unwired falls through to the
policy catch-all and then to a default 500. Nothing fails open by default; a policy that
genuinely wants to continue past a store outage wires `error` onward explicitly, and that
choice is then visible in the graph.

Timeouts reuse the store's existing configured connect timeout. No new knob.

## 8. Streaming

All four nodes read and write `context.message`, never the response body, so
`reads_response_body()` is `false` — none of them forces an upstream to buffer.

With one exception, which is already handled: `key` and `value` are `Template`s, so they run
through `Template::references_response_body()` (added in PR #52). A `store-set` whose value is
`{{response.body}}` or `$resp_body` genuinely reads the body and correctly reports `true`,
forcing buffering. The four nodes therefore implement:

```rust
// store-set; the other three check only the fields they actually have
fn reads_response_body(&self) -> bool {
    self.key.references_response_body() || self.value.references_response_body()
}
```

`store-get` and `store-delete` have no `value`, so they check `key` alone.

## 9. What this unlocks

Recorded so the design can be judged against real uses rather than in the abstract:

- **Bounded OIDC retries.** `store-incr` with a TTL, a `condition` on `$msg_retry_count`, and
  `store-delete` on success — replacing the `fb_retry` cookie with something server-side that
  a client cannot forge or clear.
- **Idempotency keys.** `store-get` on an `Idempotency-Key` header, `miss` → proceed and
  `store-set`, `success` → return the recorded outcome.
- **Per-user feature flags** read at request time without a backend call.
- **Cross-instance coordination** that today has to be faked with `limit-count`.

It also removes the main reason to reach for a Lua `script` node, which is the only way to
compute anything today.

## 10. Registration points

Each of the four nodes touches every registration point in `CLAUDE.md`'s "Adding a node type"
list. The `admin::policies` tests enforce items 3–5, so the **full** `cargo test` must run —
a filtered run will not catch the drift.

1. `src/plugins/native/store_get.rs` (etc.) + `pub mod` in `src/plugins/native/mod.rs`
2. `KNOWN_PLUGIN_TYPES` and the `create_plugin` match arm in `src/plugins/mod.rs`
3. `CATALOG` in `src/admin/policies.rs`
4. `ui/src/pluginCategories.ts`, `ui/src/pluginMeta.tsx`, `ui/src/pluginConfig.ts` — including
   the `optionsFrom` store picker the session plugins and `limit-count` already use
5. `website/docs/reference/plugins/store-get.md` (etc.) + `website/sidebars.ts` + the table in
   `website/docs/reference/plugins/index.md`
6. Port specs in `src/plugins/ports.rs`; rows in `docs/apisix-parity.md`

`store-get`'s `miss` port is a new port name and needs a `PortDecl` with a description the UI
will render.

## 11. Documentation and MCP exposure

These nodes are useless to an agent — and to the UI — if they exist only in YAML. The
exposure is not extra plumbing; it falls out of registration points 3–5, and the existing
suite already enforces it.

**The chain.** `CATALOG` (`src/admin/policies.rs`) is read by `plugin_catalog()`, which
serves both `GET /api/plugins` and the MCP tools `list_node_types` and `get_node_type`
(`src/mcp/tools/catalog.rs`). `get_node_type` additionally attaches the node's docs page,
rust-embedded from `website/docs/reference/plugins/<type>.md`, as its `docs` field — and
returns `Null` there when the page is missing. Catalog entries carry their `ports`, so an
agent sees `store-get`'s `miss` port and knows it must be wired. The same embedded pages are
served as `featherbit://docs/...` MCP resources, so no separate work is needed for those.

**Already enforced.** Four drift guards in `admin::policies` fail the build if a catalog
entry lacks any of it:

| Test | Guards |
|---|---|
| `test_catalog_covers_factory` | every factory type is in the catalog — i.e. MCP-visible at all |
| `test_every_catalog_plugin_has_a_docs_page` | the page `get_node_type` hands an agent exists |
| `test_every_plugin_docs_page_is_in_the_sidebar` | it is reachable on the docs site |
| `test_every_catalog_plugin_has_an_icon` / `…_is_in_a_palette_category` | the UI palette can render it |

They are why the **full** `cargo test` matters here: a filtered run will not catch the drift.

**What each of the four docs pages must carry**, beyond the standard config table:

- The node's ports and what reaching each one means — in particular that `miss` is a normal
  outcome and `error` is a store outage, and that conflating them is the mistake §7 warns
  about.
- Every error code it can raise: `STORE_ERROR`, `STORE_VALUE_INVALID`.
- The `kv:` namespacing rule (§6), since it determines which keys are reachable at all.
- On `store-incr`, the TTL-at-creation rule (§5.3) stated explicitly, with the reason — it is
  the node's whole purpose and the least guessable thing about it.

**One worked end-to-end example** — the bounded OIDC retry of §9 — belongs in `store-incr.md`,
with the other three pages cross-linking to it. It is the case that motivated the feature, and
a reader who sees the four nodes cooperating understands them faster than four isolated
snippets. `docs/apisix-parity.md` gets a row per node, and the plugin index table gets four.

**Two tests beyond the drift guards**, asserting the agent-facing surface directly rather than
trusting it:

- `get_node_type("store-get")` returns a non-null `docs` field — the guards prove the file
  exists, not that it reaches an agent through this path.
- `get_node_type("store-get")["ports"]["outputs"]` contains `miss` as an `outcome`, mirroring
  the existing assertion for `condition`'s `true`/`false` ports.

## 12. Testing

| Level | Test |
|---|---|
| Unit | Config parsing: missing `store`, unknown store name, `ttl_seconds: 0`, absent `name` |
| Unit | `reads_response_body` is `false` for ordinary config and `true` when `key` or `value` references the response body |
| Unit | Port specs: `store-get` declares `miss` as an `outcome`; the other three do not declare it |
| Integration (gated) | `store-set` then `store-get` round-trips a value; `store-get` on an absent key exits `miss` |
| Integration (gated) | `json: true` parses an object into `message`; invalid JSON exits `error` |
| Integration (gated) | **`store-incr` does not refresh the TTL** — increment, wait, increment again, assert the remaining TTL keeps falling. This is the behavior the node exists for and the one most likely to regress |
| Integration (gated) | `store-delete` on an absent key exits `success` |
| Integration (gated) | A store outage exits `error`, not `miss` — the distinction §7 turns on |
| e2e | `E2E-STORE-*` scenarios in `e2e/E2E_TESTBOOK.md` |

Gated integration tests run against `FEATHERBIT_TEST_REDIS_URL`, which CI already provides
through its `redis:7` + `valkey/valkey:8` service-container matrix — so both backends are
actually exercised rather than assumed.

## 13. Out of scope

- `store-delete` by pattern, and any list/scan operation. Both invite unbounded work on the
  request path.
- Non-redis backends.
- Reading keys outside the `kv:` namespace (§6).
- Arithmetic in `set-vars` (item 2b of the follow-ups memo). `store-incr` delivers the
  counter case that motivated it; whether general arithmetic is still wanted afterwards is a
  separate question, and deliberately left open here.
