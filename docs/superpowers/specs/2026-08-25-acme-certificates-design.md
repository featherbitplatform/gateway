# Automatic Certificates (ACME) — Design

**Date:** 2026-08-25
**Status:** Approved design, pending implementation plan

## Motivation

TLS termination today takes certificates only from files (`tls.cert_path` /
`tls.key_path`, per-`sni_certs` entry, `admin.tls`), with paths optionally
env-interpolated. Rotation is the operator's problem: something external
(certbot, cert-manager, a sidecar) must obtain and renew the certificate and
write it where `spawn_cert_watcher` (`src/server/tls.rs`) will notice it.
For the primary target of this feature — a single gateway instance with a
public DNS name, ports exposed directly — that means running and babysitting a
second tool for a job the gateway is already in the best position to do: it
owns port 443.

This design makes the gateway obtain and renew its own certificates through
the ACME protocol (RFC 8555) from Let's Encrypt or any other ACME CA
(ZeroSSL, Google Trust Services, an internal step-ca), validated with the
TLS-ALPN-01 challenge (RFC 8737) inside the existing TLS listener. It is
built for one instance but does not preclude an N-instance cluster: state
can live in a redis/valkey `stores:` entry, renewal is lease-coordinated,
and peers pick up a cert one of them issued.

## Scope

**In:**

- A top-level `acme:` block in `system.yaml` (CA directory URL, contact,
  ToS agreement, optional EAB, key type, renewal window, storage).
- `acme: { domains: [...] }` as an alternative to `cert_path`/`key_path` on
  the data-plane default cert and on any `sni_certs` entry, mixed freely
  with file-based entries.
- **TLS-ALPN-01** as the only challenge type, solved by the existing
  `ResolvesServerCert` on the data-plane listener.
- Two storage backends in v1: **filesystem** and **`stores:` (redis/valkey)**,
  behind one `CertStorage` trait; private keys sealed at rest in redis.
- Placeholder-cert bootstrap: the listener is up before the first issuance;
  `/readyz` reports not-ready until every managed cert is real.
- Renewal scheduler with lease coordination, backoff, and ARI (RFC 9773
  renewal-info) when the CA offers it.
- Admin API: `GET /api/acme/certs`, `POST /api/acme/certs/:id/renew`.
- UI: a read-only **Certificates panel** with a per-row "Renew now".
- Prometheus metrics for expiry/state/renewal outcomes.
- Pebble-backed integration tests in CI; docs + roadmap update.

**Out (documented follow-ups):**

- **HTTP-01** — needs a plaintext `:80` listener the gateway does not have;
  the `ChallengeSolver` trait leaves the slot open.
- **DNS-01 / wildcard certificates** — needs DNS-provider integrations;
  wildcard `server_name`s in an ACME'd slot are rejected at load.
- **`admin.tls` via ACME** — the admin listener is rarely on a public 443.
- **RSA certificate keys** — rustls/ring cannot generate RSA keys and the
  pure-Rust `rsa` crate carries an open RUSTSEC advisory the SAST pipeline
  rejects; `key_type: rsa-*` is refused at load with a pointed message.
- **OCSP stapling**, CRL — unchanged from the existing TLS roadmap.
- **ACME config CRUD** in the Admin API — ACME config lives in `system.yaml`,
  which is restart-gated like every other TLS setting.
- Pub/sub cert propagation between instances — v1 peers poll storage.

## Chosen approach

**A separate `src/acme/` subsystem that *produces* certificates; `tls.rs`
only *consumes* them.** `tls.rs` learns exactly two things: a certificate
slot may be `File` or `Managed(cert_id)`, and a ClientHello with ALPN
`acme-tls/1` is a challenge to answer. Everything ACME — protocol client,
order state machine, storage, scheduling, metrics, admin endpoints — lives
in its own module and publishes finished `CertifiedKey`s through a shared
atomic map. This mirrors how `stores:` was added (a new module with a small
surface into existing code) and keeps issuance unit-testable without a TLS
listener or a network.

Rejected alternatives:

- *Bolt into `tls.rs`*: `build_server_config` would gain issuance and the
  cert watcher would run renewals; `tls.rs` (already ~1100 lines with tests)
  would own TLS + ACME protocol + storage + scheduling, and every issuance
  test would drag a rustls listener in.
- *External sidecar + file hot-reload*: is the status quo, documented as a
  deployment pattern, not a feature.

## 1. Configuration (`system.yaml`)

```yaml
acme:                                   # optional; absent = feature off
  directory_url: https://acme-v02.api.letsencrypt.org/directory   # default
  contact: ["mailto:ops@example.com"]   # optional
  terms_of_service_agreed: true         # required to be true when acme: is present
  eab:                                  # optional (ZeroSSL, Google Trust Services, step-ca)
    key_id: ${ACME_EAB_KID}
    hmac_key: ${ACME_EAB_HMAC}          # base64url, as issued by the CA
  directory_ca_path: /etc/pki/step-root.pem   # optional; trust root for the CA's HTTPS endpoint (private CAs, Pebble)
  key_type: ecdsa-p256                  # default; or ecdsa-p384
  renew_before: 30d                     # duration; renew when not_after - now < this
  storage:
    type: filesystem                    # default
    dir: /var/lib/featherbit/acme
    # -- or --
    # type: store
    # store: sessions-redis             # a `stores:` name from gateway.yaml
    # encryption_key: ${ACME_STORAGE_KEY}   # required for type: store

tls:
  acme:                                 # replaces cert_path/key_path for the default cert
    domains: [api.example.com, www.example.com]
  min_version: "1.2"
  sni_certs:
    - server_name: tenant.example.com
      acme: {}                          # domains defaults to [server_name]
    - server_name: "*.legacy.example.com"
      cert_path: /etc/gateway/tls/legacy.crt   # file-based entries unchanged
      key_path: /etc/gateway/tls/legacy.key
```

Types (`src/config/system.rs`):

- `SystemConfig.acme: Option<AcmeConfig>`.
- `TlsConfig.cert_path`/`key_path` become `Option<String>`; new
  `TlsConfig.acme: Option<AcmeSlot>`. Same on `SniCert`.
  `AcmeSlot { domains: Vec<String> }` with `domains` defaulting to empty
  (meaning "the `server_name`" on an `SniCert`).
- `AcmeConfig { directory_url, directory_ca_path, contact,
  terms_of_service_agreed, eab, key_type, renew_before, storage }`;
  `AcmeStorageConfig` is a
  `#[serde(tag = "type")]` enum `Filesystem { dir }` |
  `Store { store, encryption_key }`.
- `renew_before` parses `Nd`/`Nh` (and plain seconds); default 30 days.

Validation at load (fail-fast, `SystemConfig` validation, before any
listener binds):

- A slot must have **exactly one** of (`cert_path` + `key_path`) or `acme`.
  Half a file pair is an error, as it is today.
- Any `acme` slot requires the top-level `acme:` block.
- `terms_of_service_agreed` must be `true` when `acme:` is present.
- Wildcard domains (`*.`) in any slot's resolved `domains` are rejected
  ("TLS-ALPN-01 cannot issue wildcard certificates; use a file-based cert").
  Non-DNS identifiers (IPs, empty, uppercase-normalized) are rejected.
- A managed **default** cert must list explicit `domains` (nothing to infer
  from).
- `admin.tls.acme` is rejected with a pointed message.
- `storage.type: store` requires the `redis-store` cargo feature (else the
  same "built without redis-store" error `stores:` uses) and
  `encryption_key` to be non-empty after `${ENV}` interpolation.
- `directory_url` must be `https://` (Pebble in tests uses `https://` too;
  a plaintext CA is refused). `directory_ca_path`, when set, must be a
  readable PEM bundle; it replaces the system roots for the CA connection.
- `key_type` must be `ecdsa-p256` or `ecdsa-p384`; anything else (including
  `rsa-2048`) is refused with a message naming the supported values.

The store *name* is checked once `gateway.yaml` is loaded (both configs are
loaded before listeners start): an unknown name aborts startup. The ACME
storage borrows the store's client from the `StoreRegistry`
(`src/stores/mod.rs`) — one connection per store, shared with sessions and
counters. Deleting that store via `DELETE /api/stores/:name` is refused with
the existing `409 {"error":"in_use","referrers":[...]}` envelope; `referrers`
gains an `"acme.storage"` entry. Editing the store's URL/credentials via the
Admin API rebuilds the client as today, and ACME storage picks it up on its
next operation (it resolves the client per call, not once).

All of `acme:` and `tls:` stay **restart-gated**, like every TLS setting.

## 2. Runtime architecture

```
src/acme/
  mod.rs          AcmeConfig validation helpers, ManagedCerts, startup wiring (`acme::start`)
  client.rs       `AcmeClient` trait + `InstantAcmeClient` (instant-acme 0.8, ring, hyper-rustls)
  order.rs        one issuance: new order -> authz -> TLS-ALPN-01 -> finalize -> download -> verify
  challenge.rs    `ChallengeSolver` trait; TLS-ALPN-01 solver = key-auth in storage + challenge cert (rcgen)
  storage/
    mod.rs        `CertStorage` trait: account key, cert bundle per cert_id, challenge, renewal lease
    fs.rs         filesystem backend (temp+rename atomic writes, 0600 on unix)
    redis.rs      `stores:` backend (feature redis-store); lease = SET NX PX
  manager.rs      renewal scheduler task: decide -> lease -> issue -> publish -> sleep
  metrics.rs      Prometheus gauges/counters
src/admin/acme.rs GET /api/acme/certs, POST /api/acme/certs/:id/renew
```

New dependencies (all ring-only, matching the tree's rule):

- `instant-acme = { version = "0.8", default-features = false,
  features = ["ring", "hyper-rustls"] }` — ACME protocol (JWS, directory,
  account/EAB, orders, ARI).
- `rcgen` moves from dev-dependencies to dependencies at `0.14`,
  `default-features = false, features = ["crypto", "pem", "ring"]` — CSRs,
  challenge certs (`CustomExtension::new_acme_identifier`), placeholder
  certs. Test code that uses rcgen 0.13 today is updated to 0.14.
- `x509-parser` (already present) parses `not_before`/`not_after`/issuer/
  serial/SANs from issued chains.

### 2.1 Certificate identity

`CertId` = the resolved `domains`, lowercased, sorted, deduplicated, joined
with `,`. Two slots with the same domain set share one certificate; a config
reorder does not re-issue. Storage paths/keys and metric labels use a
filesystem/redis-safe form (the same string; commas are legal in both, and
DNS labels contain nothing else problematic).

### 2.2 Publishing: `ManagedCerts`

```rust
pub struct ManagedCert { key: Arc<CertifiedKey>, state: CertState, meta: CertMeta }
pub type ManagedCerts = Arc<ArcSwap<HashMap<CertId, ManagedCert>>>;
```

`CertState` = `Placeholder | Issued | Renewing | Failed`; `CertMeta` holds
`not_before`, `not_after`, `issuer`, `serial`, `next_renewal_at`,
`last_attempt_at`, `last_error`. Publishing a new cert is a clone-modify-
`store()` of the map — **not** a `ServerConfig` rebuild, and it never
touches file-based slots. The existing file watcher keeps working unchanged
for file-based slots (its watch list simply skips managed ones).

### 2.3 Resolver (`tls.rs`)

`SniCertResolver` becomes:

```rust
enum CertSlot { File(Arc<CertifiedKey>), Managed(CertId) }
struct SniCertResolver {
    certs: Vec<(SniPattern, CertSlot)>,
    default: CertSlot,
    managed: Option<ManagedCerts>,           // None when acme: is absent
    challenges: Option<Arc<dyn ChallengeSolver>>,
}
```

`resolve()`:

1. If the ClientHello's ALPN list is exactly `[b"acme-tls/1"]` and
   `challenges` is set: look up the SNI name (required; none ⇒ `None`) via
   `ChallengeSolver::challenge_cert(name)`. Found ⇒ return that self-signed
   cert (SAN = name, critical `acmeIdentifier` extension = SHA-256 of the
   key authorization). Not found ⇒ `None` (handshake fails — RFC 8737 §3).
   A normal ALPN client is never affected.
2. Otherwise today's logic: SNI match → slot → default; a `Managed` slot
   resolves through `managed.load()` to the current key (placeholder or
   issued). A managed id missing from the map cannot happen after startup
   (every managed slot is seeded) but resolves to `None` defensively.

When `acme:` is configured, `acme-tls/1` is appended to the server config's
`alpn_protocols` (after `h2`/`http/1.1`). This is required: rustls aborts a
handshake with a `no_application_protocol` alert when the client offers ALPN
and none of it matches the configured list, and the validator needs the
handshake to *complete* to read the certificate (RFC 8737 §3). Because
`h2`/`http/1.1` come first, a browser offering both never negotiates
`acme-tls/1`. After the handshake, the connection handler in
`server::listener` checks the negotiated protocol: `acme-tls/1` ⇒ close the
connection without serving (the validator has what it needs; nothing else
may run on that connection). The admin listener never advertises it.

### 2.4 Storage trait

```rust
#[async_trait]
pub trait CertStorage: Send + Sync {
    async fn load_account(&self) -> Result<Option<AccountCredentials>>;
    async fn save_account(&self, creds: &AccountCredentials) -> Result<()>;
    async fn load_cert(&self, id: &CertId) -> Result<Option<StoredCert>>;   // chain PEM + key PEM + issued_at
    async fn save_cert(&self, id: &CertId, cert: &StoredCert) -> Result<()>;
    async fn put_challenge(&self, domain: &str, key_auth: &str, ttl: Duration) -> Result<()>;
    async fn get_challenge(&self, domain: &str) -> Result<Option<String>>;
    async fn remove_challenge(&self, domain: &str) -> Result<()>;
    async fn try_acquire_lease(&self, id: &CertId, owner: &str, ttl: Duration) -> Result<bool>;
    async fn renew_lease(&self, id: &CertId, owner: &str, ttl: Duration) -> Result<bool>;
    async fn release_lease(&self, id: &CertId, owner: &str) -> Result<()>;
}
```

- **Filesystem** (`dir/`): `account.json` (instant-acme credentials,
  contains the account private key — 0600), `certs/<cert_id>/chain.pem`,
  `certs/<cert_id>/key.pem` (0600), `certs/<cert_id>/meta.json`,
  `challenges/<domain>` (key-auth + expiry timestamp),
  `leases/<cert_id>` (owner + expiry). Every write is temp-file + rename.
  Expired lease/challenge files are treated as absent and overwritten.
- **Redis** (through the `StoreRegistry` client, keys under the store's
  `key_prefix`): `acme:account`, `acme:cert:{<cert_id>}`,
  `acme:challenge:<domain>` (with TTL), `acme:lease:{<cert_id>}`
  (`SET NX PX`; `renew_lease`/`release_lease` are compare-owner Lua scripts,
  same pattern as the session refresh lock). Account credentials and the
  cert private key are **sealed** with `CookieSealer`
  (`src/plugins/util/cookie_session.rs`, AES-256-GCM, key = SHA-256 of
  `storage.encryption_key`) before being written; the chain is stored in
  clear. Hash tags keep a cert's keys on one Cluster slot, matching the
  sessions convention.
- The resolver's challenge lookup is **synchronous** (rustls calls
  `resolve` on the connection's thread). The solver therefore keeps an
  in-process `RwLock<HashMap<domain, ChallengeCert>>` cache that is written
  when *this* instance registers a challenge and, for the redis backend,
  refreshed by a background task every 2 s while any order is in flight
  anywhere (detected by the lease key). This is what lets a peer instance
  answer a validation request that the CA routed to it through a TCP load
  balancer. The filesystem backend serves the local cache only (a shared
  volume cluster is not a supported topology for challenges; documented).

### 2.5 Startup sequence

In `server::start_server`, before the accept loop (and only when `acme:`
is present):

1. Build the `CertStorage`; load the account or register a new one with the
   CA (`newAccount`, with EAB when configured) and persist it. A CA that is
   unreachable at this point is **not** fatal: registration is retried by
   the manager with the same backoff as issuance, and the listener still
   comes up.
2. For each managed `CertId`: `load_cert`. If present, parses, key matches,
   and `not_after > now` ⇒ publish as `Issued`. Otherwise mint a
   **placeholder**: self-signed, CN/SAN = first domain, 1-hour validity,
   never persisted ⇒ publish as `Placeholder`.
3. Build the `ServerConfig` with the extended resolver and start accepting.
4. Spawn the manager task with `ManagedCerts`, the storage, and a
   `Notify` per cert for on-demand renewal.

The admin listener is unaffected.

### 2.6 Manager loop

One tokio task iterates all managed certs; per cert:

- `next_action_at = min(not_after - renew_before, ari_window.start)` where
  the ARI window comes from the CA's `renewalInfo` endpoint when advertised
  (instant-acme exposes it); `Placeholder` ⇒ now; `Failed` ⇒ now + backoff.
- When due: `try_acquire_lease(cert_id, owner = hostname:pid:random, ttl =
  5 min)`. Refused ⇒ another instance is renewing: poll `load_cert` every
  60 s and publish anything newer than what is held (by `not_after`), then
  go back to sleep. Acquired ⇒ set state `Renewing` and run the order.
- **Order** (`order.rs`): generate a fresh private key (per `key_type`) →
  `newOrder` for all domains → for each `pending` authorization, compute the
  key authorization, `put_challenge(domain, …, 10 min)` (also warms the local
  cache) → tell the CA the challenge is ready → poll the order until
  `ready`/`invalid` (bounded, renewing the lease every 60 s) → CSR via rcgen
  → `finalize` → poll until `valid` → download the chain → **verify** (PEM
  parses, leaf's public key matches the generated key, `not_after > now`,
  SANs cover all domains) → `save_cert` → publish `Issued` with parsed
  metadata → `remove_challenge` for each domain → `release_lease`.
- **Failure** at any step: `error!` with the ACME problem detail, state
  `Failed` with `last_error`, current cert (placeholder or old) keeps
  serving, `remove_challenge` + `release_lease` in a `finally`-style path,
  backoff `1m → 2m → … → 1h` cap, reset on success.
- `POST …/renew` fires the cert's `Notify`; the loop wakes and treats the
  cert as due, honoring the lease. With a still-valid cert outside the renew
  window the request re-issues only when `?force=true` (Let's Encrypt
  duplicate-cert limits are the classic footgun); otherwise it returns `200`
  with `{"scheduled": false, "reason": "not_due"}`.

### 2.7 Readiness

`readyz` (`src/admin/status.rs`) additionally returns `503` with
`{"status":"not_ready","reason":"acme placeholder certs","acme":{"placeholder":[...cert_ids]}}`
while any managed cert is `Placeholder`. `Issued`, `Renewing`, and `Failed`
(with a real cert still serving) are all ready — a renewal failure never
degrades readiness. `/healthz` is unchanged.

## 3. Admin API, UI, metrics

### Admin API (`src/admin/acme.rs`, Basic Auth like the rest)

- `GET /api/acme/certs` →
  `{"enabled": true, "storage": "filesystem" | "store:<name>", "certs": [ { "id", "domains", "state", "not_before", "not_after", "issuer", "serial", "next_renewal_at", "last_attempt_at", "last_error" } ]}`.
  With `acme:` absent: `200 {"enabled": false, "certs": []}` — so the UI
  infers "not configured" from the body, not from a status-code guess.
- `POST /api/acme/certs/:id/renew[?force=true]` → `202 {"scheduled": true}`
  when the manager is nudged; `200 {"scheduled": false, "reason": "not_due"}`
  when outside the window without `force`; `409 {"error":"in_progress"}`
  when an order is already running; `404` for an unknown id; `501` when
  `acme:` is absent.
- No CRUD (config is `system.yaml`).

### UI — Certificates panel

`ui/src/components/CertificatesPanel.tsx`, opened from a footer button next
to Sessions and laid out like `SessionsPanel.tsx`:

- Table: domains, state badge (grey Placeholder / green Issued / blue
  Renewing / red Failed), "expires in" with thresholds (amber < 30 d, red
  < 7 d), issuer, next renewal, last error (collapsed, expandable).
- Per-row **Renew now** with a confirm dialog; a second confirm step for
  "force" when the API answers `not_due`. Errors surface inline.
- When `enabled: false`: an informational notice "ACME is not configured
  (`acme:` in system.yaml)"; the footer button stays visible but the panel
  is inert.
- Polls every 30 s while open. No editing.

### Metrics (`/metrics`, `src/acme/metrics.rs`)

- `featherbit_acme_cert_not_after_timestamp_seconds{cert_id}` gauge (0 for
  placeholders).
- `featherbit_acme_cert_state{cert_id,state}` gauge — 1 for the current
  state, 0 for the others.
- `featherbit_acme_renewals_total{cert_id,result="success"|"failure"}`
  counter.
- `featherbit_acme_last_renewal_attempt_timestamp_seconds{cert_id}` gauge.

## 4. Error handling & security

- **Private keys**: filesystem writes `0600` (unix; best-effort on Windows,
  documented); redis stores keys and account credentials sealed with
  `CookieSealer` under `storage.encryption_key` (raw `${ENV}` placeholder in
  config, resolved at use — the `stores:` password rule). Keys never appear
  in the Admin API, UI, logs, or traces.
- **CA rate limits**: `CertId` dedup; no re-issue of a valid, not-due cert
  without `?force=true`; exponential backoff; docs steer experiments to the
  staging directory URL.
- **Bad chains**: verification before persistence — a CA returning garbage
  never evicts a working cert. Clock skew tolerance: `not_before` up to
  5 min in the future is accepted.
- **Challenge hygiene**: challenges carry a 10-minute TTL (redis TTL, fs/
  cache timestamp) so a stuck order cannot leave a validatable challenge
  cert behind; the local cache evicts expired entries on every lookup.
- **Placeholder honesty**: self-signed, 1-hour validity, re-minted on every
  restart and hourly while still in `Placeholder` state, never persisted;
  the state is `warn!`-logged on startup and every failed attempt.
- **Lease safety**: owner-checked renew/release; a crashed holder's lease
  expires in 5 min; a peer that finds a corrupt storage entry logs and keeps
  its current cert.
- **Config secrets**: `eab.hmac_key` and `storage.encryption_key` are
  resolved from `${ENV}` at load like the rest of `system.yaml`; the
  `GET /api/acme/certs` response never includes config.

## 5. Testing

- **Unit, no network** (`src/acme/**`, `src/config/system.rs`):
  - config validation: exactly-one-of file/acme per slot, missing top-level
    block, ToS not agreed, wildcard rejection, admin.tls rejection, default
    cert without domains, `store` type without the feature/without
    encryption key, non-https directory.
  - `CertId` derivation (case, order, duplicates).
  - `CertStorage` contract test suite run against the fs backend (tempdir)
    and, gated on `FEATHERBIT_TEST_REDIS_URL` like the session tests, the
    redis backend (CI's redis/valkey matrix job runs it): round-trips,
    challenge TTL, lease acquire/refuse/expire/owner-checked release, sealed
    key bytes never equal to the plaintext.
  - `when_to_renew(not_after, ari, now, renew_before)` as a pure function.
  - `order.rs` against a **mock `AcmeClient`**: happy path; authz `invalid`;
    finalize failure; chain that does not match the key is rejected and the
    old cert stays; backoff progression; lease refresh during a slow poll;
    challenge cleanup on failure.
- **Resolver** (`src/server/tls.rs`): rustls client offering ALPN
  `acme-tls/1` + SNI `x` receives a cert whose `acmeIdentifier` extension is
  the SHA-256 of the key-auth; with no pending challenge the handshake is
  refused; an `h2`/`http/1.1` client with the same SNI gets the managed cert;
  swapping `ManagedCerts` is visible on the next connection without a
  `ServerConfig` rebuild; a connection that negotiated `acme-tls/1` is
  closed after the handshake without a response; `acme-tls/1` is absent from
  the ALPN list when `acme:` is not configured; file-based slots and their
  hot-reload behave as before.
- **Integration with Pebble** (Let's Encrypt's test CA; Linux CI, run with
  `docker run --network host` so its validator reaches the gateway on the
  runner's loopback, `tlsPort` pointed at the gateway's listener,
  `directory_ca_path` = Pebble's `pebble.minica.pem`): gateway boots with a placeholder → `/readyz` is 503 → cert is
  issued → `/readyz` is 200 → `GET /api/acme/certs` shows `issued` with a
  Pebble issuer → `POST …/renew?force=true` rotates the serial → a restart
  reuses the stored cert (no re-issue). Run for both storage backends. Gated
  on `FEATHERBIT_TEST_PEBBLE_URL`; `dev/` gets a compose file to run Pebble
  locally.
- **E2E (Playwright)**: `E2E-ACME-*` — ungated: the panel shows the
  not-configured notice; Pebble-gated: the table shows the issued cert and
  "Renew now" changes the serial. Catalogued in `e2e/E2E_TESTBOOK.md`.

## 6. Documentation

- `website/docs/guides/tls.md`: new "Automatic certificates (ACME)" section
  — config, choosing storage, cluster notes (TCP passthrough LB required,
  redis storage, lease behavior), staging advice, readiness semantics,
  Admin API, and why HTTP-01/wildcards are not in v1.
- `website/docs/reference/roadmap.md`: TLS row gains the feature and the
  follow-ups (HTTP-01, DNS-01/wildcards, admin-listener ACME, pub/sub
  propagation, OCSP stapling).
- `CLAUDE.md`: one sentence in the TLS paragraph pointing at `src/acme/`.
- `config/system.yaml`: a commented-out `acme:` example.

## Implementation notes for the plan

- Order of work that keeps every step shippable: config types +
  validation → `CertStorage` (fs, then redis) → `ManagedCerts` + resolver
  `CertSlot`/ALPN branch → `AcmeClient` trait + mock + `order.rs` → manager
  + startup wiring + readiness → metrics → Admin API → UI panel → Pebble
  CI job + docs.
- `TlsConfig.cert_path`/`key_path` turning `Option` touches
  `build_server_config`, `spawn_cert_watcher`'s path list, the admin
  listener, and tests that construct `TlsConfig` literally — do that first
  and keep the file-only behavior byte-for-byte identical.
- rcgen 0.13 → 0.14 in tests is a mechanical update
  (`CertificateParams`/`KeyPair` API is stable across the two; the ring
  feature name is `ring`).
