# Server-Side Sessions Implementation Plan (Plan 2)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Opt-in `session.storage: redis` for the five session plugins (`openid-connect`, `cas-auth`, `authz-casdoor`, plus **restored** session support for `dingtalk-auth` / `feishu-auth`), with sealed-at-rest payloads, revocation via a sessions Admin API, and lock-coordinated token refresh for openid-connect.

**Architecture:** A new `src/sessions/` module defines `SessionStore` (trait) + `RedisSessionStore` (hash-tagged keys over Plan 1's `RedisStoreClient`). `StoreRegistry` gains `session_store(name)` mirroring `counter_store`. A shared helper (`src/plugins/util/server_session.rs`) parses the `session.storage`/`session.store` config keys and gives all five plugins one `establish`/`load`/`destroy` seam: in cookie mode the sealed payload lives in the cookie exactly as today; in redis mode the cookie holds a bare random 128-bit id and the *same sealed bytes* live in the store with a `SessionMeta` envelope. Store failures surface as 503 on the `error` port — never 401, never fail-open. The listener exposes `__route`/`__policy` in `ctx.message` so sessions carry operator-facing attribution.

**Tech Stack:** Rust; Plan 1's `redis-store` feature + `RedisStoreClient`; ring `SystemRandom` for ids; axum admin API.

**Spec:** `docs/superpowers/specs/2026-08-21-session-storage-design.md` §2 (SessionStore + plugin integration), "Cluster readiness" (hash-tagged keys), §4 sessions endpoints. Plan 1 (`2026-08-21-stores-and-redis-counters.md`) shipped everything this plan consumes.

## Global Constraints

- **Branch:** create `feature/server-side-sessions` off `feature/session-storage` (PR #28 is open; this stacks on it — Task 1 Step 0 creates the branch).
- Conventional Commits, **no Co-Authored-By trailer**. `git add` only files you changed (dirty unrelated files exist; never `git add -A`).
- Per task before committing: `cargo fmt`; `cargo clippy --all-targets -- -D warnings`; `cargo clippy --no-default-features --all-targets -- -D warnings`; `cargo check --no-default-features`; full `cargo test` once. `ui/dist` must exist for default-feature builds.
- `cargo test <name> -- --exact` matches the FULL module path; if 0 tests run, drop `--exact`.
- Config-path errors are `Result<_, String>`; request-path session-store failures are `StoreError(pub String)` → **503 through the `error` port** (new per-plugin helper; the existing `infra_error` helpers hardcode 401 and must not be reused for store failures). No fail-open mode for sessions.
- Security invariants: session payloads are sealed with the plugin's existing `CookieSealer` **before** touching the store (the store never sees plaintext tokens); in redis mode the cookie carries only the bare 128-bit hex id; the transient flow cookies (`*_flow`) stay client-side in both modes; no resolved secret in errors/logs/Debug; Admin sessions endpoints never return session *content* (meta only).
- Redis keys are Cluster hash-tagged (spec "Cluster readiness"): `{prefix}:sess:{<id>}`, `{prefix}:sess:{<id>}:meta`, `{prefix}:lock:{<id>}`, `{prefix}:subj:{<sha256(subject)>}` — braces literal.
- Config convention: every new session key supports the nested form AND the flat UI fallback (`session.storage` / `session_storage`, `session.store` / `session_store`) like the existing `session_cookie_field` helpers.
- **Known breaking change (deliberate, documented in Task 13):** `dingtalk-auth`/`feishu-auth` move from `AUTH_SPEC` to `INTERACTIVE_AUTH_SPEC`, making the `redirect` outcome port mandatory-wired for existing policies — same posture as `openid-connect` in bearer-only mode, and required for the restored 302 flow.
- Spec deviations locked in by this plan (Task 13 amends the spec): (a) token refresh is **greenfield** (the oidc port never captured `refresh_token`) and ships in **redis mode only** — cookie mode keeps its documented re-authenticate-on-expiry behavior; (b) `SessionMeta` carries `policy` AND `route` (via new `__route`/`__policy` context vars), satisfying the spec's "route/policy name".
- Live-Redis tests are env-gated on `FEATHERBIT_TEST_REDIS_URL` (self-skip otherwise). Never run docker inside a task.

---

### Task 1: `src/sessions/` — types, trait, fake store

**Files:**
- Create: `src/sessions/mod.rs`
- Modify: `src/main.rs` (module list: `mod sessions;` after `mod server;`… keep alphabetical: `ratelimit, server, sessions, state, stores, stream…`)

**Interfaces:**
- Consumes: nothing new (ring is already a dependency).
- Produces (every later task consumes these exact names):
  - `pub struct SessionId(String)` with `SessionId::random() -> Self`, `SessionId::parse(&str) -> Option<Self>`, `as_str(&self) -> &str`
  - `pub struct SessionMeta { pub id: String, pub subject: String, pub plugin: String, pub policy: String, pub route: String, pub created_at: u64, pub expires_at: u64 }` (serde Serialize/Deserialize/Clone/Debug; `id` is empty on `put`, filled by `list`)
  - `pub struct SessionFilter { pub subject: Option<String>, pub plugin: Option<String>, pub limit: usize, pub cursor: Option<String> }`
  - `pub struct SessionPage { pub sessions: Vec<SessionMeta>, pub next_cursor: Option<String> }`
  - `pub struct StoreError(pub String)` with `Display`
  - `pub trait SessionStore: Send + Sync` with the seven async methods below
  - `#[cfg(test)] pub struct FakeSessionStore` (in-memory, crate-visible to all unit tests)

- [ ] **Step 0: Create the branch**

```bash
git checkout -b feature/server-side-sessions feature/session-storage
```

- [ ] **Step 1: Write the module with its failing tests**

Create `src/sessions/mod.rs`:

```rust
//! Server-side sessions for the interactive auth plugins.
//!
//! In `session.storage: redis` mode a plugin's session cookie shrinks to a
//! bare random 128-bit id; the payload — the same bytes the plugin seals
//! into the cookie today — is stored sealed-at-rest under that id with a
//! small unencrypted [`SessionMeta`] envelope for the operator surface
//! (list/revoke). The store never sees plaintext tokens.
//!
//! Backends implement [`SessionStore`]; `RedisSessionStore` (the `redis`
//! submodule, `redis-store` feature) is the real one, [`FakeSessionStore`]
//! serves unit tests. Failure semantics are the spec's: a [`StoreError`]
//! surfaces as a 503 on the plugin's `error` port — never 401, never
//! fail-open.

use std::collections::HashMap;
use std::time::Duration;

use async_trait::async_trait;
use ring::rand::{SecureRandom, SystemRandom};
use serde::{Deserialize, Serialize};

#[cfg(feature = "redis-store")]
pub mod redis;

/// A 128-bit random session id, hex-encoded (32 chars). The only thing the
/// browser holds in redis mode, and deliberately unguessable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionId(String);

impl SessionId {
    /// Fresh random id from the system RNG (house pattern, see
    /// `authz_casdoor::random_state`).
    pub fn random() -> Self {
        let mut bytes = [0u8; 16];
        SystemRandom::new()
            .fill(&mut bytes)
            .expect("system RNG must produce a session id");
        Self(bytes.iter().map(|b| format!("{b:02x}")).collect())
    }

    /// Accepts exactly 32 lowercase hex chars; anything else (including a
    /// sealed legacy cookie value) is not an id. Keeps junk out of store keys.
    pub fn parse(value: &str) -> Option<Self> {
        if value.len() == 32 && value.bytes().all(|b| b.is_ascii_hexdigit()) {
            Some(Self(value.to_ascii_lowercase()))
        } else {
            None
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Unencrypted envelope for the operator surface. `id` is left empty on
/// `put` (the key already carries it) and filled in by `list`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionMeta {
    #[serde(default)]
    pub id: String,
    /// Authenticated subject; may be empty (e.g. an opaque token with no
    /// decodable claims) — such sessions are not indexed by subject.
    #[serde(default)]
    pub subject: String,
    pub plugin: String,
    #[serde(default)]
    pub policy: String,
    #[serde(default)]
    pub route: String,
    pub created_at: u64,
    pub expires_at: u64,
}

/// Listing filter; `cursor` is backend-opaque (Redis SCAN cursor).
#[derive(Debug, Clone, Default)]
pub struct SessionFilter {
    pub subject: Option<String>,
    pub plugin: Option<String>,
    pub limit: usize,
    pub cursor: Option<String>,
}

/// One page of session metadata.
#[derive(Debug, Clone)]
pub struct SessionPage {
    pub sessions: Vec<SessionMeta>,
    pub next_cursor: Option<String>,
}

/// Session-store backend failure. Always maps to 503 on the plugin's
/// `error` port; callers must never treat it as "unauthenticated".
#[derive(Debug)]
pub struct StoreError(pub String);

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "session store error: {}", self.0)
    }
}

/// A backend holding sealed session payloads plus their meta envelopes.
#[async_trait]
pub trait SessionStore: Send + Sync {
    /// Upserts a session: sealed payload + meta, both expiring after `ttl`.
    async fn put(
        &self,
        id: &SessionId,
        sealed: &[u8],
        ttl: Duration,
        meta: &SessionMeta,
    ) -> Result<(), StoreError>;
    /// The sealed payload, or None if absent/expired.
    async fn get(&self, id: &SessionId) -> Result<Option<Vec<u8>>, StoreError>;
    /// Revokes one session (payload + meta + subject-index entry).
    async fn delete(&self, id: &SessionId) -> Result<(), StoreError>;
    /// Revokes every session for `subject`; returns how many were removed.
    async fn delete_subject(&self, subject: &str) -> Result<u64, StoreError>;
    /// One page of metadata matching `filter`.
    async fn list(&self, filter: &SessionFilter) -> Result<SessionPage, StoreError>;
    /// Best-effort short lock for refresh coordination: true = acquired.
    async fn try_lock(&self, id: &SessionId, ttl: Duration) -> Result<bool, StoreError>;
    async fn unlock(&self, id: &SessionId) -> Result<(), StoreError>;
}

/// In-memory `SessionStore` for unit tests across the crate.
#[cfg(test)]
#[derive(Default)]
pub struct FakeSessionStore {
    #[allow(clippy::type_complexity)]
    inner: std::sync::Mutex<HashMap<String, (Vec<u8>, SessionMeta, std::time::Instant)>>,
    locks: std::sync::Mutex<std::collections::HashSet<String>>,
    /// When set, every call fails — for 503-path tests.
    pub fail: std::sync::atomic::AtomicBool,
}

#[cfg(test)]
impl FakeSessionStore {
    fn check(&self) -> Result<(), StoreError> {
        if self.fail.load(std::sync::atomic::Ordering::Relaxed) {
            Err(StoreError("fake store failure".to_string()))
        } else {
            Ok(())
        }
    }
}

#[cfg(test)]
#[async_trait]
impl SessionStore for FakeSessionStore {
    async fn put(
        &self,
        id: &SessionId,
        sealed: &[u8],
        ttl: Duration,
        meta: &SessionMeta,
    ) -> Result<(), StoreError> {
        self.check()?;
        self.inner.lock().unwrap().insert(
            id.as_str().to_string(),
            (sealed.to_vec(), meta.clone(), std::time::Instant::now() + ttl),
        );
        Ok(())
    }

    async fn get(&self, id: &SessionId) -> Result<Option<Vec<u8>>, StoreError> {
        self.check()?;
        let mut inner = self.inner.lock().unwrap();
        match inner.get(id.as_str()) {
            Some((_, _, exp)) if *exp <= std::time::Instant::now() => {
                inner.remove(id.as_str());
                Ok(None)
            }
            Some((sealed, _, _)) => Ok(Some(sealed.clone())),
            None => Ok(None),
        }
    }

    async fn delete(&self, id: &SessionId) -> Result<(), StoreError> {
        self.check()?;
        self.inner.lock().unwrap().remove(id.as_str());
        Ok(())
    }

    async fn delete_subject(&self, subject: &str) -> Result<u64, StoreError> {
        self.check()?;
        let mut inner = self.inner.lock().unwrap();
        let before = inner.len();
        inner.retain(|_, (_, meta, _)| meta.subject != subject);
        Ok((before - inner.len()) as u64)
    }

    async fn list(&self, filter: &SessionFilter) -> Result<SessionPage, StoreError> {
        self.check()?;
        let inner = self.inner.lock().unwrap();
        let mut sessions: Vec<SessionMeta> = inner
            .iter()
            .map(|(id, (_, meta, _))| {
                let mut m = meta.clone();
                m.id = id.clone();
                m
            })
            .filter(|m| filter.subject.as_deref().is_none_or(|s| m.subject == s))
            .filter(|m| filter.plugin.as_deref().is_none_or(|p| m.plugin == p))
            .collect();
        sessions.sort_by(|a, b| a.id.cmp(&b.id));
        if filter.limit > 0 {
            sessions.truncate(filter.limit);
        }
        Ok(SessionPage {
            sessions,
            next_cursor: None,
        })
    }

    async fn try_lock(&self, id: &SessionId, _ttl: Duration) -> Result<bool, StoreError> {
        self.check()?;
        Ok(self.locks.lock().unwrap().insert(id.as_str().to_string()))
    }

    async fn unlock(&self, id: &SessionId) -> Result<(), StoreError> {
        self.check()?;
        self.locks.lock().unwrap().remove(id.as_str());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_session_id_random_and_parse() {
        let id = SessionId::random();
        assert_eq!(id.as_str().len(), 32);
        assert!(id.as_str().bytes().all(|b| b.is_ascii_hexdigit()));
        assert_ne!(SessionId::random().as_str(), id.as_str());

        assert_eq!(SessionId::parse(id.as_str()), Some(id.clone()));
        // Uppercase normalizes; junk and sealed-cookie-shaped values do not parse.
        assert!(SessionId::parse(&id.as_str().to_uppercase()).is_some());
        assert!(SessionId::parse("").is_none());
        assert!(SessionId::parse("nothex-nothex-nothex-nothex-noth").is_none());
        assert!(SessionId::parse("abcd").is_none());
    }

    #[tokio::test]
    async fn test_fake_store_round_trip_and_revocation() {
        let store = FakeSessionStore::default();
        let id = SessionId::random();
        let meta = SessionMeta {
            id: String::new(),
            subject: "alice".to_string(),
            plugin: "openid-connect".to_string(),
            policy: "p1".to_string(),
            route: "r1".to_string(),
            created_at: 1,
            expires_at: 2,
        };
        store
            .put(&id, b"sealed", Duration::from_secs(60), &meta)
            .await
            .unwrap();
        assert_eq!(store.get(&id).await.unwrap().as_deref(), Some(&b"sealed"[..]));

        let page = store.list(&SessionFilter::default()).await.unwrap();
        assert_eq!(page.sessions.len(), 1);
        assert_eq!(page.sessions[0].id, id.as_str());
        assert_eq!(page.sessions[0].subject, "alice");

        assert_eq!(store.delete_subject("alice").await.unwrap(), 1);
        assert_eq!(store.get(&id).await.unwrap(), None);
    }

    #[tokio::test]
    async fn test_fake_store_lock_and_failure_mode() {
        let store = FakeSessionStore::default();
        let id = SessionId::random();
        assert!(store.try_lock(&id, Duration::from_secs(10)).await.unwrap());
        assert!(!store.try_lock(&id, Duration::from_secs(10)).await.unwrap());
        store.unlock(&id).await.unwrap();
        assert!(store.try_lock(&id, Duration::from_secs(10)).await.unwrap());

        store.fail.store(true, std::sync::atomic::Ordering::Relaxed);
        assert!(store.get(&id).await.is_err());
    }
}
```

(The `#[cfg(feature = "redis-store")] pub mod redis;` line lands with a stub in this task: create `src/sessions/redis.rs` containing only `//! Redis-backed SessionStore — implemented in the next task.` — an empty module compiles; Task 2 fills it.)

Add `mod sessions;` to `src/main.rs`'s module list (alphabetical).

- [ ] **Step 2: Run the new tests**

Run: `cargo test sessions:: && cargo check --no-default-features`
Expected: 3 tests PASS; headless compiles (the fake is cfg(test); the trait/types are feature-agnostic).

- [ ] **Step 3: Full check + commit**

Run: `cargo test && cargo fmt && cargo clippy --all-targets -- -D warnings && cargo clippy --no-default-features --all-targets -- -D warnings`
Expected: green. (`is_none_or` needs Rust ≥1.82 — if clippy/rustc balks, use `.map_or(true, |s| ...)`.)

```bash
git add src/sessions/ src/main.rs
git commit -m "feat(sessions): session store trait, meta envelope, and test fake"
```

---

### Task 2: `RedisSessionStore`

**Files:**
- Rewrite: `src/sessions/redis.rs`

**Interfaces:**
- Consumes: `RedisStoreClient` (`src/stores/redis_store.rs`): `conn() -> Result<redis::aio::ConnectionManager, String>` (:127), `key_prefix() -> &str` (:183), `name() -> &str` (:179). `SessionStore` etc. from Task 1.
- Produces: `pub struct RedisSessionStore` with `pub fn new(client: Arc<RedisStoreClient>, metrics: Option<Arc<GatewayMetrics>>) -> Self`, implementing `SessionStore`. Key layout (braces literal, Cluster hash tags): `{prefix}:sess:{<id>}` sealed payload, `{prefix}:sess:{<id>}:meta` JSON meta, `{prefix}:lock:{<id>}` SET NX PX lock, `{prefix}:subj:{<sha256hex(subject)>}` SET of ids. Store errors increment `gateway_counter_store_errors_total{store}`? **No** — sessions get their own counter: this task ADDS `gateway_session_store_errors_total{store}` to `GatewayMetrics` (same registration pattern as `counter_store_errors`).

- [ ] **Step 1: Write the failing key-layout unit test, then the module**

Replace `src/sessions/redis.rs`:

```rust
//! Redis/Valkey-backed [`SessionStore`] (`redis-store` feature).
//!
//! Keys are Cluster hash-tagged on the session id (spec "Cluster
//! readiness"): a session's payload, meta, and lock always share a slot.
//! The subject index is a SET per sha256(subject), lazily pruned. Payloads
//! arrive already sealed (AES-256-GCM via the plugin's `CookieSealer`) —
//! nothing readable sits in Redis. Errors bump
//! `gateway_session_store_errors_total{store}` and surface as
//! [`StoreError`] (503 at the plugin).

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use redis::AsyncCommands;
use ring::digest::{digest, SHA256};

use crate::metrics::GatewayMetrics;
use crate::stores::redis_store::RedisStoreClient;

use super::{SessionFilter, SessionId, SessionMeta, SessionPage, SessionStore, StoreError};

pub struct RedisSessionStore {
    client: Arc<RedisStoreClient>,
    metrics: Option<Arc<GatewayMetrics>>,
}

fn sha256_hex(input: &str) -> String {
    digest(&SHA256, input.as_bytes())
        .as_ref()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Key builders — pure, unit-tested. Braces are literal Redis Cluster hash
/// tags, so `sess`/`meta`/`lock` for one id always share a slot.
fn sess_key(prefix: &str, id: &str) -> String {
    format!("{prefix}:sess:{{{id}}}")
}
fn meta_key(prefix: &str, id: &str) -> String {
    format!("{prefix}:sess:{{{id}}}:meta")
}
fn lock_key(prefix: &str, id: &str) -> String {
    format!("{prefix}:lock:{{{id}}}")
}
fn subj_key(prefix: &str, subject: &str) -> String {
    format!("{prefix}:subj:{{{}}}", sha256_hex(subject))
}
/// Extracts the id from a meta key produced by [`meta_key`].
fn id_of_meta_key(prefix: &str, key: &str) -> Option<String> {
    key.strip_prefix(&format!("{prefix}:sess:{{"))?
        .strip_suffix("}:meta")
        .map(str::to_string)
}

impl RedisSessionStore {
    pub fn new(client: Arc<RedisStoreClient>, metrics: Option<Arc<GatewayMetrics>>) -> Self {
        Self { client, metrics }
    }

    fn err(&self, msg: String) -> StoreError {
        if let Some(ref m) = self.metrics {
            m.session_store_errors
                .with_label_values(&[self.client.name()])
                .inc();
        }
        tracing::warn!(store = %self.client.name(), "session store error: {}", msg);
        StoreError(msg)
    }

    async fn conn(&self) -> Result<redis::aio::ConnectionManager, StoreError> {
        self.client.conn().await.map_err(|e| self.err(e))
    }
}

#[async_trait]
impl SessionStore for RedisSessionStore {
    async fn put(
        &self,
        id: &SessionId,
        sealed: &[u8],
        ttl: Duration,
        meta: &SessionMeta,
    ) -> Result<(), StoreError> {
        let p = self.client.key_prefix();
        let ttl_secs = ttl.as_secs().max(1);
        let meta_json = serde_json::to_string(meta)
            .map_err(|e| self.err(format!("meta serialize: {e}")))?;
        let mut conn = self.conn().await?;
        let mut pipe = redis::pipe();
        pipe.set_ex(sess_key(p, id.as_str()), sealed, ttl_secs)
            .set_ex(meta_key(p, id.as_str()), meta_json, ttl_secs);
        if !meta.subject.is_empty() {
            let sk = subj_key(p, &meta.subject);
            pipe.sadd(&sk, id.as_str()).expire(&sk, ttl_secs as i64);
        }
        pipe.query_async::<()>(&mut conn)
            .await
            .map_err(|e| self.err(format!("put: {e}")))
    }

    async fn get(&self, id: &SessionId) -> Result<Option<Vec<u8>>, StoreError> {
        let p = self.client.key_prefix();
        let mut conn = self.conn().await?;
        conn.get::<_, Option<Vec<u8>>>(sess_key(p, id.as_str()))
            .await
            .map_err(|e| self.err(format!("get: {e}")))
    }

    async fn delete(&self, id: &SessionId) -> Result<(), StoreError> {
        let p = self.client.key_prefix();
        let mut conn = self.conn().await?;
        // Read the meta first so the subject index entry can be pruned.
        let meta: Option<String> = conn
            .get(meta_key(p, id.as_str()))
            .await
            .map_err(|e| self.err(format!("delete meta read: {e}")))?;
        let _: () = conn
            .del(&[sess_key(p, id.as_str()), meta_key(p, id.as_str())])
            .await
            .map_err(|e| self.err(format!("delete: {e}")))?;
        if let Some(m) = meta.and_then(|s| serde_json::from_str::<SessionMeta>(&s).ok()) {
            if !m.subject.is_empty() {
                let _: () = conn
                    .srem(subj_key(p, &m.subject), id.as_str())
                    .await
                    .map_err(|e| self.err(format!("delete srem: {e}")))?;
            }
        }
        Ok(())
    }

    async fn delete_subject(&self, subject: &str) -> Result<u64, StoreError> {
        let p = self.client.key_prefix();
        let mut conn = self.conn().await?;
        let sk = subj_key(p, subject);
        let ids: Vec<String> = conn
            .smembers(&sk)
            .await
            .map_err(|e| self.err(format!("delete_subject smembers: {e}")))?;
        let mut removed = 0u64;
        for id in &ids {
            // Individual DELs: cross-slot under Cluster, so no multi-key op.
            let n: u64 = conn
                .del(&[sess_key(p, id), meta_key(p, id)])
                .await
                .map_err(|e| self.err(format!("delete_subject del: {e}")))?;
            if n > 0 {
                removed += 1;
            }
        }
        let _: () = conn
            .del(&sk)
            .await
            .map_err(|e| self.err(format!("delete_subject index del: {e}")))?;
        Ok(removed)
    }

    async fn list(&self, filter: &SessionFilter) -> Result<SessionPage, StoreError> {
        let p = self.client.key_prefix();
        let mut conn = self.conn().await?;
        let cursor: u64 = filter
            .cursor
            .as_deref()
            .unwrap_or("0")
            .parse()
            .map_err(|_| StoreError("invalid cursor".to_string()))?;
        let count = if filter.limit > 0 { filter.limit } else { 50 };
        let (next, keys): (u64, Vec<String>) = redis::cmd("SCAN")
            .arg(cursor)
            .arg("MATCH")
            .arg(format!("{p}:sess:*:meta"))
            .arg("COUNT")
            .arg(count)
            .query_async(&mut conn)
            .await
            .map_err(|e| self.err(format!("list scan: {e}")))?;
        let mut sessions = Vec::new();
        for key in keys {
            let Some(id) = id_of_meta_key(p, &key) else {
                continue;
            };
            let raw: Option<String> = conn
                .get(&key)
                .await
                .map_err(|e| self.err(format!("list get: {e}")))?;
            let Some(mut meta) = raw.and_then(|s| serde_json::from_str::<SessionMeta>(&s).ok())
            else {
                continue;
            };
            meta.id = id;
            if filter.subject.as_deref().is_some_and(|s| meta.subject != s) {
                continue;
            }
            if filter.plugin.as_deref().is_some_and(|pl| meta.plugin != pl) {
                continue;
            }
            sessions.push(meta);
        }
        Ok(SessionPage {
            sessions,
            next_cursor: if next == 0 {
                None
            } else {
                Some(next.to_string())
            },
        })
    }

    async fn try_lock(&self, id: &SessionId, ttl: Duration) -> Result<bool, StoreError> {
        let p = self.client.key_prefix();
        let mut conn = self.conn().await?;
        let acquired: Option<String> = redis::cmd("SET")
            .arg(lock_key(p, id.as_str()))
            .arg("1")
            .arg("NX")
            .arg("PX")
            .arg(ttl.as_millis().max(1) as u64)
            .query_async(&mut conn)
            .await
            .map_err(|e| self.err(format!("try_lock: {e}")))?;
        Ok(acquired.is_some())
    }

    async fn unlock(&self, id: &SessionId) -> Result<(), StoreError> {
        let p = self.client.key_prefix();
        let mut conn = self.conn().await?;
        conn.del::<_, ()>(lock_key(p, id.as_str()))
            .await
            .map_err(|e| self.err(format!("unlock: {e}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The hash-tag layout is the cluster-readiness contract: one id's keys
    /// share a slot, and the meta-key parser inverts the builder.
    #[test]
    fn test_key_layout_and_meta_parse() {
        assert_eq!(sess_key("fb", "ab12"), "fb:sess:{ab12}");
        assert_eq!(meta_key("fb", "ab12"), "fb:sess:{ab12}:meta");
        assert_eq!(lock_key("fb", "ab12"), "fb:lock:{ab12}");
        assert!(subj_key("fb", "alice").starts_with("fb:subj:{"));
        assert!(subj_key("fb", "alice").ends_with('}'));
        assert_ne!(subj_key("fb", "alice"), subj_key("fb", "bob"));
        assert_eq!(
            id_of_meta_key("fb", "fb:sess:{ab12}:meta").as_deref(),
            Some("ab12")
        );
        assert_eq!(id_of_meta_key("fb", "fb:sess:{ab12}"), None);
    }

    /// Live round-trip; skipped unless FEATHERBIT_TEST_REDIS_URL is set.
    #[tokio::test]
    async fn test_redis_session_store_live() {
        let Ok(url) = std::env::var("FEATHERBIT_TEST_REDIS_URL") else {
            eprintln!("skipping test_redis_session_store_live: FEATHERBIT_TEST_REDIS_URL not set");
            return;
        };
        let cfg: crate::config::StoreConfig = serde_yaml::from_str(&format!(
            "name: live\ntype: redis\nurl: {url}\nkey_prefix: fbsess{}\n",
            std::process::id()
        ))
        .unwrap();
        let client = Arc::new(RedisStoreClient::build(&cfg).unwrap());
        let store = RedisSessionStore::new(client, None);

        let id = SessionId::random();
        let meta = SessionMeta {
            id: String::new(),
            subject: "alice".to_string(),
            plugin: "openid-connect".to_string(),
            policy: "p".to_string(),
            route: "r".to_string(),
            created_at: 1,
            expires_at: 9999999999,
        };
        store
            .put(&id, b"sealed-bytes", Duration::from_secs(60), &meta)
            .await
            .unwrap();
        assert_eq!(
            store.get(&id).await.unwrap().as_deref(),
            Some(&b"sealed-bytes"[..])
        );

        // Lock: winner/loser then release.
        assert!(store.try_lock(&id, Duration::from_secs(5)).await.unwrap());
        assert!(!store.try_lock(&id, Duration::from_secs(5)).await.unwrap());
        store.unlock(&id).await.unwrap();

        // List finds it (drain SCAN cursors until exhausted).
        let mut cursor: Option<String> = None;
        let mut found = false;
        loop {
            let page = store
                .list(&SessionFilter {
                    subject: Some("alice".to_string()),
                    plugin: None,
                    limit: 10,
                    cursor: cursor.clone(),
                })
                .await
                .unwrap();
            if page.sessions.iter().any(|m| m.id == id.as_str()) {
                found = true;
            }
            match page.next_cursor {
                Some(c) => cursor = Some(c),
                None => break,
            }
        }
        assert!(found);

        // Revoke by subject.
        assert!(store.delete_subject("alice").await.unwrap() >= 1);
        assert_eq!(store.get(&id).await.unwrap(), None);
    }
}
```

- [ ] **Step 2: Add the metric**

`src/metrics/mod.rs`: add field + registration, exactly like `counter_store_errors` (same `IntCounterVec` pattern, same headless `cfg_attr` if that one carries it):

```rust
    /// Session-store (stores:) backend errors, per named store.
    pub session_store_errors: IntCounterVec,
```

name `gateway_session_store_errors_total`, help `"Total session-store backend errors per named store"`, labels `&["store"]`.

- [ ] **Step 3: Run**

Run: `cargo test test_key_layout_and_meta_parse -- --exact && cargo test && cargo check --no-default-features && cargo clippy --all-targets -- -D warnings && cargo clippy --no-default-features --all-targets -- -D warnings && cargo fmt`
Expected: green (live test self-skips). Optional once, locally:

```bash
docker run -d --rm -p 16379:6379 --name fb-test-redis redis:7
FEATHERBIT_TEST_REDIS_URL=redis://127.0.0.1:16379 cargo test test_redis_session_store_live
docker stop fb-test-redis
```

- [ ] **Step 4: Commit**

```bash
git add src/sessions/ src/metrics/mod.rs
git commit -m "feat(sessions): redis session store with hash-tagged keys and error metric"
```

---

### Task 3: `StoreRegistry::session_store` + test injection helper

**Files:**
- Modify: `src/stores/mod.rs`

**Interfaces:**
- Consumes: `RedisSessionStore::new(client, metrics)` (Task 2).
- Produces: `StoreRegistry::session_store(&self, name: &str) -> Result<Arc<dyn crate::sessions::SessionStore>, String>` (both feature variants — headless errors like `counter_store` does); rebuild populates one `RedisSessionStore` per client; `#[cfg(test)] pub fn StoreRegistry::with_fake_session_store(name: &str, store: Arc<dyn SessionStore>) -> StoreRegistry` for plugin unit tests.

- [ ] **Step 1: Failing test**

In `src/stores/mod.rs` tests:

```rust
    #[tokio::test]
    async fn test_session_store_lookup_and_fake_injection() {
        let reg = StoreRegistry::default();
        let err = reg.session_store("nope").unwrap_err();
        assert!(err.contains("'nope'"), "{err}");

        let fake: std::sync::Arc<dyn crate::sessions::SessionStore> =
            std::sync::Arc::new(crate::sessions::FakeSessionStore::default());
        let reg = StoreRegistry::with_fake_session_store("s1", fake);
        assert!(reg.session_store("s1").is_ok());
    }
```

Run: `cargo test test_session_store_lookup_and_fake_injection -- --exact` — Expected: COMPILE FAIL.

- [ ] **Step 2: Implement**

In `StoreRegistry` (feature-on): add field `sessions: HashMap<String, Arc<dyn crate::sessions::SessionStore>>`; in `rebuild`'s per-store loop (next to the counters insert):

```rust
            sessions.insert(
                cfg.name.clone(),
                Arc::new(crate::sessions::redis::RedisSessionStore::new(
                    client.clone(),
                    metrics.clone(),
                )) as Arc<dyn crate::sessions::SessionStore>,
            );
```

Accessor (mirror `counter_store`, same unknown-name error listing declared stores):

```rust
    /// Resolves the session backend for a named store.
    #[cfg(feature = "redis-store")]
    pub fn session_store(
        &self,
        name: &str,
    ) -> Result<Arc<dyn crate::sessions::SessionStore>, String> {
        self.sessions.get(name).cloned().ok_or_else(|| {
            let mut names: Vec<&str> = self.clients.keys().map(String::as_str).collect();
            names.sort_unstable();
            format!(
                "unknown store '{}' — declared stores: {}",
                name,
                if names.is_empty() {
                    "(none)".to_string()
                } else {
                    names.join(", ")
                }
            )
        })
    }

    #[cfg(not(feature = "redis-store"))]
    pub fn session_store(
        &self,
        name: &str,
    ) -> Result<Arc<dyn crate::sessions::SessionStore>, String> {
        Err(format!(
            "store '{}': this binary was built without the redis-store feature",
            name
        ))
    }

    /// Test-only registry holding one injected fake session store.
    #[cfg(test)]
    pub fn with_fake_session_store(
        name: &str,
        store: Arc<dyn crate::sessions::SessionStore>,
    ) -> StoreRegistry {
        let mut reg = StoreRegistry::default();
        #[cfg(feature = "redis-store")]
        reg.sessions.insert(name.to_string(), store);
        #[cfg(not(feature = "redis-store"))]
        let _ = (name, store);
        reg
    }
```

(If the headless `StoreRegistry` variant has no `sessions` field, the `with_fake_session_store` headless arm just returns the default — the headless test run never reaches redis-mode plugin tests, which are `#[cfg(feature = "redis-store")]`-gated in later tasks.)

- [ ] **Step 3: Run + commit**

Run: `cargo test test_session_store_lookup_and_fake_injection -- --exact && cargo test && cargo check --no-default-features && cargo clippy --all-targets -- -D warnings && cargo clippy --no-default-features --all-targets -- -D warnings && cargo fmt`

```bash
git add src/stores/mod.rs
git commit -m "feat(stores): session-store accessor on the registry"
```

---

### Task 4: `__route` / `__policy` context vars

**Files:**
- Modify: `src/server/listener.rs` (route-match site, ~:199 where `(route_name, policy_name, graph)` is resolved)
- Modify: `src/server/websocket.rs` IF it builds its own Context for graph execution (verify; the upgrade path "runs the policy graph" per CLAUDE.md — find where its Context is built and set the same keys)
- Test: inline in `src/server/listener.rs`

**Interfaces:**
- Produces: on every graph execution, `ctx.message["__route"]` and `ctx.message["__policy"]` hold the matched route/policy names as JSON strings, set BEFORE `CompiledGraph::execute`. Precedent: the `__client_cert_*` internal vars. Session plugins (Tasks 6-11) read them for `SessionMeta`.

- [ ] **Step 1: Failing test**

In `src/server/listener.rs` tests (follow the file's existing test style — it has graph-execution tests like `test_graceful_shutdown_drains_in_flight`; if a lighter seam exists, e.g. a helper that builds the Context from a matched route, test that helper directly):

```rust
    /// Every request's context carries its route/policy attribution for
    /// plugins that need it (session meta, diagnostics).
    #[tokio::test]
    async fn test_context_carries_route_and_policy_names() {
        // Build the minimal state the file's other tests use (echo policy),
        // send one request through the data plane or the internal handler,
        // and assert a plugin observed __route/__policy. The `echo` plugin
        // reflects ctx.message? If not: use a `script` node returning
        // message values, or assert via a debug-trace capture — pick the
        // cheapest existing seam in this file's tests and mirror it.
        // Concretely: reuse the file's existing end-to-end test scaffolding
        // (test_state + one request), with the policy's terminal node
        // configured to surface message["__policy"] into a response header.
    }
```

NOTE to implementer: the assertion mechanism depends on this file's existing harness — mirror the nearest end-to-end test. If no existing plugin cleanly surfaces `ctx.message`, add the assertion in a `script` (Lua) node: `ctx.response.headers["x-test-policy"] = ctx.message.__policy` (Lua sees message per the runtime marshalling). The REQUIRED outcome: a test that fails before Step 2 and proves both keys are set with the route/policy names from the matched route.

Run it — Expected: FAIL (keys absent).

- [ ] **Step 2: Implement**

At the route-match site in `src/server/listener.rs` (right after `(route_name, policy_name, graph)` resolution, before `graph.execute`), on the freshly-built Context:

```rust
        ctx.message.insert(
            "__route".to_string(),
            serde_json::Value::String(route_name.clone()),
        );
        ctx.message.insert(
            "__policy".to_string(),
            serde_json::Value::String(policy_name.clone()),
        );
```

Check `src/server/websocket.rs`: if the upgrade path builds its own Context before running the graph, add the same two inserts there (it has the route/policy in scope from its own match). If it reuses the listener's context-building path, note that in the report and change nothing.

- [ ] **Step 3: Run + commit**

Run: the new test + `cargo test` + the four gates.

```bash
git add src/server/listener.rs src/server/websocket.rs
git commit -m "feat(server): expose __route/__policy context vars to plugins"
```

---

### Task 5: shared `server_session` helper

**Files:**
- Create: `src/plugins/util/server_session.rs`
- Modify: `src/plugins/util/mod.rs` (add `pub mod server_session;`)

**Interfaces:**
- Consumes: `SessionStore`/`SessionId`/`SessionMeta`/`StoreError` (Task 1), `StoreRegistry::session_store` (Task 3), `CookieSealer`/`build_set_cookie`/`CookieAttrs`/`delete_cookie` (existing).
- Produces (Tasks 6-11 consume verbatim):

```rust
pub enum SessionBackend {
    Cookie,
    Store { store: Arc<dyn SessionStore>, store_name: String },
}
pub fn parse_backend(config: &HashMap<String, serde_json::Value>, resources: &Arc<PluginResources>, plugin: &str) -> Result<SessionBackend, String>;
pub fn meta_now(ctx: &Context, plugin: &str, subject: &str, ttl: Duration) -> SessionMeta;
pub async fn establish(backend: &SessionBackend, sealer: &CookieSealer, payload: &[u8], ttl: Duration, meta: SessionMeta, cookie_name: &str, attrs: &CookieAttrs<'_>) -> Result<String, StoreError>;  // returns the Set-Cookie value
pub async fn load(backend: &SessionBackend, sealer: &CookieSealer, cookie_value: &str) -> Result<Option<Vec<u8>>, StoreError>;  // Ok(None) = unauthenticated; Err = 503
pub async fn destroy(backend: &SessionBackend, cookie_value: Option<&str>, cookie_name: &str, path: &str) -> Result<String, StoreError>;  // returns the delete-cookie value; store delete failures are Err (revocation is the point)
pub async fn update(backend: &SessionBackend, sealer: &CookieSealer, cookie_value: &str, payload: &[u8], ttl: Duration, meta: SessionMeta) -> Result<Option<String>, StoreError>;  // redis: put under the SAME id, returns None (cookie unchanged); cookie: returns Some(new Set-Cookie)
```

- [ ] **Step 1: Write the module + tests**

Create `src/plugins/util/server_session.rs`:

```rust
//! Shared session-backend plumbing for the interactive auth plugins.
//!
//! One config surface (`session.storage: cookie|redis` + `session.store:
//! <name>`, with the flat `session_storage`/`session_store` UI fallbacks)
//! and one establish/load/destroy seam, so the five session plugins differ
//! only in payload shape and flow, not in storage mechanics. Cookie mode is
//! byte-identical to the pre-existing behavior; redis mode puts the SAME
//! sealed bytes server-side and hands the browser a bare 128-bit id.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::context::Context;
use crate::plugins::resources::PluginResources;
use crate::sessions::{SessionId, SessionMeta, SessionStore, StoreError};

use super::cookie_session::{build_set_cookie, delete_cookie, CookieAttrs, CookieSealer};

pub enum SessionBackend {
    Cookie,
    Store {
        store: Arc<dyn SessionStore>,
        store_name: String,
    },
}

fn nested_or_flat<'a>(
    config: &'a HashMap<String, serde_json::Value>,
    nested: &str,
    flat: &str,
) -> Option<&'a str> {
    config
        .get("session")
        .and_then(|s| s.get(nested))
        .or_else(|| config.get(flat))
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
}

/// Parses `session.storage` / `session.store` and resolves the named store
/// at construction time — a bad reference fails policy compilation.
pub fn parse_backend(
    config: &HashMap<String, serde_json::Value>,
    resources: &Arc<PluginResources>,
    plugin: &str,
) -> Result<SessionBackend, String> {
    match nested_or_flat(config, "storage", "session_storage").unwrap_or("cookie") {
        "cookie" => Ok(SessionBackend::Cookie),
        "redis" => {
            let name = nested_or_flat(config, "store", "session_store").ok_or_else(|| {
                format!(
                    "{plugin}: session.storage 'redis' requires 'session.store' naming a declared stores: entry"
                )
            })?;
            let store = resources.stores.load().session_store(name)?;
            Ok(SessionBackend::Store {
                store,
                store_name: name.to_string(),
            })
        }
        other => Err(format!(
            "{plugin}: unknown session.storage '{other}' — supported: cookie, redis"
        )),
    }
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Builds the meta envelope from the context's `__route`/`__policy` vars.
pub fn meta_now(ctx: &Context, plugin: &str, subject: &str, ttl: Duration) -> SessionMeta {
    let var = |k: &str| {
        ctx.message
            .get(k)
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string()
    };
    let now = now_unix();
    SessionMeta {
        id: String::new(),
        subject: subject.to_string(),
        plugin: plugin.to_string(),
        policy: var("__policy"),
        route: var("__route"),
        created_at: now,
        expires_at: now.saturating_add(ttl.as_secs()),
    }
}

/// Seals `payload` and returns the session Set-Cookie value. Cookie mode:
/// the sealed blob IS the cookie. Redis mode: the sealed blob goes under a
/// fresh random id; the cookie carries the bare id.
pub async fn establish(
    backend: &SessionBackend,
    sealer: &CookieSealer,
    payload: &[u8],
    ttl: Duration,
    meta: SessionMeta,
    cookie_name: &str,
    attrs: &CookieAttrs<'_>,
) -> Result<String, StoreError> {
    let sealed = sealer.seal(payload, ttl);
    match backend {
        SessionBackend::Cookie => Ok(build_set_cookie(cookie_name, &sealed, attrs)),
        SessionBackend::Store { store, .. } => {
            let id = SessionId::random();
            store.put(&id, sealed.as_bytes(), ttl, &meta).await?;
            Ok(build_set_cookie(cookie_name, id.as_str(), attrs))
        }
    }
}

/// Opens a session cookie value. `Ok(None)` = treat as unauthenticated
/// (absent/expired/tampered/junk id); `Err` = store outage (503, never 401).
pub async fn load(
    backend: &SessionBackend,
    sealer: &CookieSealer,
    cookie_value: &str,
) -> Result<Option<Vec<u8>>, StoreError> {
    match backend {
        SessionBackend::Cookie => Ok(sealer.open(cookie_value).ok()),
        SessionBackend::Store { store, .. } => {
            let Some(id) = SessionId::parse(cookie_value) else {
                return Ok(None);
            };
            let Some(sealed) = store.get(&id).await? else {
                return Ok(None);
            };
            let sealed = String::from_utf8(sealed).unwrap_or_default();
            Ok(sealer.open(&sealed).ok())
        }
    }
}

/// Revokes the session and returns the delete-cookie header value. A store
/// delete failure is an Err — server-side revocation is the entire point of
/// redis mode, so a logout that silently leaves the session live must fail
/// loudly (503) instead.
pub async fn destroy(
    backend: &SessionBackend,
    cookie_value: Option<&str>,
    cookie_name: &str,
    path: &str,
) -> Result<String, StoreError> {
    if let (SessionBackend::Store { store, .. }, Some(value)) = (backend, cookie_value) {
        if let Some(id) = SessionId::parse(value) {
            store.delete(&id).await?;
        }
    }
    Ok(delete_cookie(cookie_name, path))
}

/// Rewrites an existing session's payload (token refresh). Redis mode: put
/// under the SAME id (cookie unchanged → returns None). Cookie mode: the
/// caller must send a fresh cookie (returns Some(set_cookie)).
pub async fn update(
    backend: &SessionBackend,
    sealer: &CookieSealer,
    cookie_value: &str,
    payload: &[u8],
    ttl: Duration,
    meta: SessionMeta,
    cookie_name: &str,
    attrs: &CookieAttrs<'_>,
) -> Result<Option<String>, StoreError> {
    let sealed = sealer.seal(payload, ttl);
    match backend {
        SessionBackend::Cookie => Ok(Some(build_set_cookie(cookie_name, &sealed, attrs))),
        SessionBackend::Store { store, .. } => {
            let Some(id) = SessionId::parse(cookie_value) else {
                return Ok(None);
            };
            store.put(&id, sealed.as_bytes(), ttl, &meta).await?;
            Ok(None)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sessions::FakeSessionStore;

    fn store_backend(fake: Arc<FakeSessionStore>) -> SessionBackend {
        SessionBackend::Store {
            store: fake,
            store_name: "s1".to_string(),
        }
    }

    fn meta() -> SessionMeta {
        SessionMeta {
            id: String::new(),
            subject: "alice".to_string(),
            plugin: "test".to_string(),
            policy: String::new(),
            route: String::new(),
            created_at: 0,
            expires_at: 0,
        }
    }

    #[tokio::test]
    async fn test_cookie_mode_matches_legacy_shape() {
        let sealer = CookieSealer::new("k");
        let set = establish(
            &SessionBackend::Cookie,
            &sealer,
            b"payload",
            Duration::from_secs(60),
            meta(),
            "test_session",
            &CookieAttrs::default(),
        )
        .await
        .unwrap();
        // Legacy shape: the sealed blob is the cookie value itself.
        let value = set
            .strip_prefix("test_session=")
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_string();
        assert_eq!(
            load(&SessionBackend::Cookie, &sealer, &value)
                .await
                .unwrap()
                .as_deref(),
            Some(&b"payload"[..])
        );
    }

    #[tokio::test]
    async fn test_store_mode_round_trip_id_cookie_and_revocation() {
        let fake = Arc::new(FakeSessionStore::default());
        let backend = store_backend(fake.clone());
        let sealer = CookieSealer::new("k");
        let set = establish(
            &backend,
            &sealer,
            b"payload",
            Duration::from_secs(60),
            meta(),
            "test_session",
            &CookieAttrs::default(),
        )
        .await
        .unwrap();
        let value = set
            .strip_prefix("test_session=")
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_string();
        // The cookie is a bare 32-hex id, far under 4 KB, not the payload.
        assert_eq!(value.len(), 32);
        assert!(crate::sessions::SessionId::parse(&value).is_some());

        assert_eq!(
            load(&backend, &sealer, &value).await.unwrap().as_deref(),
            Some(&b"payload"[..])
        );
        // Junk cookie values are unauthenticated, not errors.
        assert_eq!(load(&backend, &sealer, "not-an-id").await.unwrap(), None);

        // Destroy revokes server-side.
        destroy(&backend, Some(&value), "test_session", "/")
            .await
            .unwrap();
        assert_eq!(load(&backend, &sealer, &value).await.unwrap(), None);
    }

    #[tokio::test]
    async fn test_store_outage_is_error_not_unauthenticated() {
        let fake = Arc::new(FakeSessionStore::default());
        fake.fail.store(true, std::sync::atomic::Ordering::Relaxed);
        let backend = store_backend(fake);
        let sealer = CookieSealer::new("k");
        let id = crate::sessions::SessionId::random();
        assert!(load(&backend, &sealer, id.as_str()).await.is_err());
        assert!(destroy(&backend, Some(id.as_str()), "n", "/").await.is_err());
    }

    #[tokio::test]
    async fn test_update_keeps_id_in_store_mode() {
        let fake = Arc::new(FakeSessionStore::default());
        let backend = store_backend(fake);
        let sealer = CookieSealer::new("k");
        let set = establish(
            &backend,
            &sealer,
            b"v1",
            Duration::from_secs(60),
            meta(),
            "n",
            &CookieAttrs::default(),
        )
        .await
        .unwrap();
        let value = set
            .strip_prefix("n=")
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_string();
        let out = update(
            &backend,
            &sealer,
            &value,
            b"v2",
            Duration::from_secs(60),
            meta(),
            "n",
            &CookieAttrs::default(),
        )
        .await
        .unwrap();
        assert!(out.is_none(), "store mode must not reissue the cookie");
        assert_eq!(
            load(&backend, &sealer, &value).await.unwrap().as_deref(),
            Some(&b"v2"[..])
        );
    }

    #[test]
    fn test_parse_backend_errors() {
        use crate::plugins::resources::PluginResources;
        let resources = PluginResources::empty();
        let cfg = |json: serde_json::Value| -> HashMap<String, serde_json::Value> {
            serde_json::from_value(json).unwrap()
        };
        assert!(matches!(
            parse_backend(&cfg(serde_json::json!({})), &resources, "p").unwrap(),
            SessionBackend::Cookie
        ));
        let err = parse_backend(
            &cfg(serde_json::json!({"session": {"storage": "redis"}})),
            &resources,
            "p",
        )
        .unwrap_err();
        assert!(err.contains("requires 'session.store'"), "{err}");
        let err = parse_backend(
            &cfg(serde_json::json!({"session": {"storage": "memcached"}})),
            &resources,
            "p",
        )
        .unwrap_err();
        assert!(err.contains("unknown session.storage"), "{err}");
        let err = parse_backend(
            &cfg(serde_json::json!({"session_storage": "redis", "session_store": "nope"})),
            &resources,
            "p",
        )
        .unwrap_err();
        assert!(err.contains("'nope'"), "{err}");
    }
}
```

(`update`'s signature in the Interfaces block above omitted `cookie_name`/`attrs` — the code here is authoritative: it takes them, for the cookie-mode arm.)

- [ ] **Step 2: Run + commit**

Run: the five new tests, then `cargo test` + four gates.

```bash
git add src/plugins/util/server_session.rs src/plugins/util/mod.rs
git commit -m "feat(plugins): shared session backend (cookie/redis) for interactive auth"
```

---

### Task 6: openid-connect redis mode

**Files:**
- Modify: `src/plugins/native/openid_connect.rs`

**Interfaces:**
- Consumes: `parse_backend`, `establish`, `load`, `destroy`, `meta_now` (Task 5).
- Produces: `Interactive` gains `backend: SessionBackend`; config keys `session.storage`/`session.store` (+flat) on this plugin; a `store_error` helper: 503, code `SESSION_STORE_ERROR`, `error` port.

Current seams (verbatim locations, from the exploration of this exact revision):
- WRITE: `handle_callback` :830-861 (seal → `build_set_cookie` → redirect with `[set_session, clear_flow]`)
- READ: `read_session` :693-700 (sync, `Option<SessionData>`), sole caller :662
- DELETE: logout at :649-654
- Flow cookie :711-742/:864-871 — UNTOUCHED (stays client-side)
- `build_interactive` :973-1040; helpers `session_field`/`session_cookie_field` :1042-1067

- [ ] **Step 1: Failing tests**

Append to the plugin's test module (reuse `interactive_explicit_cfg()` :2037 and `req_ctx` :1916; fake-store resources helper):

```rust
    #[cfg(feature = "redis-store")]
    fn resources_with_fake_store() -> (
        std::sync::Arc<crate::plugins::resources::PluginResources>,
        std::sync::Arc<crate::sessions::FakeSessionStore>,
    ) {
        let fake = std::sync::Arc::new(crate::sessions::FakeSessionStore::default());
        let resources = crate::plugins::resources::PluginResources::empty();
        resources.stores.store(std::sync::Arc::new(
            crate::stores::StoreRegistry::with_fake_session_store("s1", fake.clone()),
        ));
        (resources, fake)
    }

    /// redis storage requires a store name; unknown stores fail at config.
    #[test]
    fn test_session_storage_redis_requires_store() {
        let mut cfg = interactive_explicit_cfg();
        cfg.insert(
            "session".to_string(),
            serde_json::json!({"secret": "cookie-signing-secret", "storage": "redis"}),
        );
        let err = OpenidConnectPlugin::from_config(&cfg, &PluginResources::empty()).unwrap_err();
        assert!(err.contains("requires 'session.store'"), "{err}");
    }

    /// In redis mode a valid id-cookie authenticates from the store, and a
    /// store outage is a 503 on the error port — never a silent re-login.
    #[cfg(feature = "redis-store")]
    #[tokio::test]
    async fn test_redis_session_read_and_store_outage_503() {
        let (resources, fake) = resources_with_fake_store();
        let mut cfg = interactive_explicit_cfg();
        cfg.insert(
            "session".to_string(),
            serde_json::json!({
                "secret": "cookie-signing-secret",
                "storage": "redis",
                "store": "s1"
            }),
        );
        let plugin = OpenidConnectPlugin::from_config(&cfg, &resources).unwrap();

        // Establish a session by hand: seal SessionData, put under an id.
        let sealer = CookieSealer::new("cookie-signing-secret");
        let data = serde_json::json!({"claims": {"sub": "u1"}});
        let sealed = sealer.seal(&serde_json::to_vec(&data).unwrap(), Duration::from_secs(60));
        let id = crate::sessions::SessionId::random();
        let meta = crate::sessions::SessionMeta {
            id: String::new(),
            subject: "u1".to_string(),
            plugin: "openid-connect".to_string(),
            policy: String::new(),
            route: String::new(),
            created_at: 0,
            expires_at: 0,
        };
        fake.put(&id, sealed.as_bytes(), Duration::from_secs(60), &meta)
            .await
            .unwrap();

        let mut ctx = req_ctx("/api", "");
        ctx.request.headers.insert(
            "cookie".to_string(),
            vec![format!("oidc_session={}", id.as_str())],
        );
        let out = plugin.execute(ctx).await.unwrap();
        assert!(out.port.is_none(), "valid store session must pass");
        assert_eq!(out.context.message["user_id"], "u1");

        // Outage: same request, failing store.
        fake.fail.store(true, std::sync::atomic::Ordering::Relaxed);
        let mut ctx = req_ctx("/api", "");
        ctx.request.headers.insert(
            "cookie".to_string(),
            vec![format!("oidc_session={}", id.as_str())],
        );
        let err = plugin.execute(ctx).await.unwrap_err();
        assert_eq!(err.error.code, "SESSION_STORE_ERROR");
        assert_eq!(err.context.response.status_code, 503);
    }
```

Run — Expected: FAIL (no `session.store` handling; no store read path).

- [ ] **Step 2: Implement**

1. `Interactive` gains `backend: crate::plugins::util::server_session::SessionBackend`. `build_interactive` gains a `resources: &Arc<PluginResources>` parameter (update its one call site :289) and sets `backend: parse_backend(config, resources, "openid-connect")?`.
2. `read_session` becomes async + Result:

```rust
    /// Reads the session cookie via the configured backend. `Ok(None)` =
    /// unauthenticated; `Err` = store outage (503 via `store_error`).
    async fn read_session(&self, ctx: &Context) -> Result<Option<SessionData>, StoreError> {
        let Some(flow) = self.interactive.as_ref() else {
            return Ok(None);
        };
        let Some(cookie_header) = ctx.request.headers.get("cookie").and_then(|v| v.first())
        else {
            return Ok(None);
        };
        let Some(raw) = read_cookie(cookie_header, &flow.session_cookie) else {
            return Ok(None);
        };
        let bytes = server_session::load(&flow.backend, &flow.sealer, raw).await?;
        Ok(bytes.and_then(|b| serde_json::from_slice(&b).ok()))
    }
```

Caller :662 becomes:

```rust
        let session = match self.read_session(&ctx).await {
            Ok(s) => s,
            Err(e) => return Err(Self::store_error(ctx, e)),
        };
        if let Some(session) = session {
```

3. New helper next to `infra_error` (:615):

```rust
    /// Session-store outage: 503 through the error port. Deliberately NOT
    /// 401 — bouncing users to an IdP whose callback also cannot persist a
    /// session is a redirect loop disguised as an outage.
    fn store_error(mut ctx: Context, e: crate::sessions::StoreError) -> PluginExecutionError {
        ctx.response.status_code = 503;
        ctx.response.body = Bytes::from(r#"{"error": "session store unavailable"}"#.as_bytes());
        ctx.response.headers.insert(
            "content-type".to_string(),
            vec!["application/json".to_string()],
        );
        PluginExecutionError {
            context: ctx,
            error: GatewayError {
                node_id: String::new(),
                code: "SESSION_STORE_ERROR".to_string(),
                message: e.to_string(),
                metadata: HashMap::new(),
            },
        }
    }
```

4. WRITE seam (:830-861): replace the seal + `build_set_cookie` pair with:

```rust
        let payload = match serde_json::to_vec(&session) {
            Ok(b) => b,
            Err(e) => {
                return Err(Self::infra_error(ctx, format!("session serialize failed: {}", e)))
            }
        };
        let subject = claims
            .get("sub")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let meta = server_session::meta_now(&ctx, "openid-connect", &subject, flow.session_lifetime);
        let set_session = match server_session::establish(
            &flow.backend,
            &flow.sealer,
            &payload,
            flow.session_lifetime,
            meta,
            &flow.session_cookie,
            &CookieAttrs {
                path: &flow.cookie_path,
                max_age: Some(flow.session_lifetime.as_secs()),
                http_only: true,
                secure: request_is_https(&ctx),
                same_site: SameSite::Lax,
            },
        )
        .await
        {
            Ok(s) => s,
            Err(e) => return Err(Self::store_error(ctx, e)),
        };
```

(the `clear_flow` + redirect lines after it stay verbatim).

5. DELETE seam (logout, :649-654):

```rust
        if let Some(logout_path) = &flow.logout_path {
            if ctx.request.path == *logout_path {
                let cookie_value = ctx
                    .request
                    .headers
                    .get("cookie")
                    .and_then(|v| v.first())
                    .and_then(|h| read_cookie(h, &flow.session_cookie))
                    .map(str::to_string);
                let clear = match server_session::destroy(
                    &flow.backend,
                    cookie_value.as_deref(),
                    &flow.session_cookie,
                    &flow.cookie_path,
                )
                .await
                {
                    Ok(c) => c,
                    Err(e) => return Err(Self::store_error(ctx, e)),
                };
                return redirect(ctx, &flow.post_logout_redirect_uri, vec![clear]);
            }
        }
```

6. Imports: `use crate::plugins::util::server_session::{self, SessionBackend};` etc. Flow-cookie code untouched.

- [ ] **Step 3: Run**

Run: the two new tests + the plugin's full test file (`cargo test openid`) + `cargo test` + four gates.
Expected: all green; every pre-existing openid test still passes (cookie mode is the default and byte-identical).

- [ ] **Step 4: Commit**

```bash
git add src/plugins/native/openid_connect.rs
git commit -m "feat(openid-connect): redis session storage with 503 store-failure semantics"
```

---

### Task 7: openid-connect token refresh (redis mode only)

**Files:**
- Modify: `src/plugins/native/openid_connect.rs`

**Interfaces:**
- Consumes: `SessionStore::{try_lock, unlock}` via the backend; `server_session::update`; `exchange_code`'s request-building pattern (:874-917); `token_endpoint()` (:930-937).
- Produces: `SessionData` gains `#[serde(default, skip_serializing_if = "Option::is_none")] refresh_token: Option<String>` and `expires_at: Option<u64>` (access-token expiry, epoch secs). Populated ONLY in redis mode. New config key `session.refresh` (bool, default `true` in redis mode — set false to disable). Greenfield: this plugin never had refresh (module doc :45 says so — update it).

Behavior contract:
- Callback (redis mode only): capture `refresh_token` and `expires_in` from the token response; `expires_at = now + expires_in`.
- On session read (redis mode, refresh enabled): if `expires_at` is `Some(t)` and `now >= t - 30` and `refresh_token` is `Some`: `try_lock(id, 10s)`. Winner → POST `grant_type=refresh_token` to the token endpoint → on success re-validate the new id_token if present (else keep existing claims), write the updated `SessionData` back under the SAME id via `server_session::update`, `unlock`, proceed authenticated with fresh tokens. On refresh failure (IdP error) → `unlock`, fall through to `begin_auth` (re-login — an IdP problem is not a store outage, so no 503). Loser (lock not acquired) → re-`get` the session once; proceed with whatever is there (the winner usually already refreshed).
- Cookie mode: completely unchanged (fields stay `None`; no refresh attempt).

- [ ] **Step 1: Failing test**

```rust
    /// Redis-mode refresh: expired access token + refresh_token triggers a
    /// locked refresh; the session is rewritten under the same id. The IdP
    /// being unreachable falls back to re-login (redirect), not 503.
    #[cfg(feature = "redis-store")]
    #[tokio::test]
    async fn test_redis_refresh_lock_and_fallback() {
        let (resources, fake) = resources_with_fake_store();
        let mut cfg = interactive_explicit_cfg(); // token_endpoint: http://127.0.0.1:1 (unreachable)
        cfg.insert(
            "session".to_string(),
            serde_json::json!({
                "secret": "cookie-signing-secret",
                "storage": "redis",
                "store": "s1"
            }),
        );
        let plugin = OpenidConnectPlugin::from_config(&cfg, &resources).unwrap();

        let sealer = CookieSealer::new("cookie-signing-secret");
        // Stale access token (expires_at in the past) + a refresh token.
        let data = serde_json::json!({
            "claims": {"sub": "u1"},
            "access_token": "old",
            "refresh_token": "rt",
            "expires_at": 1
        });
        let sealed = sealer.seal(&serde_json::to_vec(&data).unwrap(), Duration::from_secs(600));
        let id = crate::sessions::SessionId::random();
        let meta = crate::sessions::SessionMeta {
            id: String::new(), subject: "u1".into(), plugin: "openid-connect".into(),
            policy: String::new(), route: String::new(), created_at: 0, expires_at: 0,
        };
        fake.put(&id, sealed.as_bytes(), Duration::from_secs(600), &meta).await.unwrap();

        let mut ctx = req_ctx("/api", "");
        ctx.request.headers.insert(
            "cookie".to_string(),
            vec![format!("oidc_session={}", id.as_str())],
        );
        // Token endpoint unreachable → refresh fails → fall back to re-login.
        let out = plugin.execute(ctx).await.unwrap();
        assert_eq!(out.port, Some("redirect"), "failed refresh re-enters login");
        // The lock was released (unlock on the failure path).
        assert!(fake.try_lock(&id, Duration::from_secs(1)).await.unwrap());
    }
```

Run — Expected: FAIL (no refresh fields/logic; today a valid-seal session just passes).

- [ ] **Step 2: Implement**

1. `SessionData` (:84-90) gains the two optional fields (serde defaults keep old sealed sessions parseable).
2. `handle_callback`: in redis mode (`matches!(flow.backend, SessionBackend::Store { .. })` and `flow.refresh_enabled`), extract `refresh_token` (string) and `expires_in` (u64) from the token-response JSON next to the existing `access_token` extraction (:805-808); set the fields (cookie mode: leave `None`).
3. New `async fn refresh_tokens(&self, refresh_token: &str) -> Result<serde_json::Value, String>` cloned from `exchange_code`'s request shape with body `grant_type=refresh_token&refresh_token=...&client_id=...&client_secret=...`; returns the parsed token-response JSON.
4. In the session-read success path (:662+), before attaching identity, insert the refresh check implementing the Behavior contract above. The lock id comes from `SessionId::parse(raw_cookie_value)`; hold `raw` from the read for this. On winner-success: rebuild `SessionData` (new access_token; new refresh_token if rotated, else keep; new expires_at; claims from re-validated id_token when present), `server_session::update(...)` with ttl = `meta`-remaining (`flow.session_lifetime` is acceptable — session absolute lifetime restarts on refresh is NOT wanted; use remaining: `expires_at_meta.saturating_sub(now)`… simplest correct: keep the store TTL by re-putting with the ORIGINAL session lifetime minus elapsed — derive from the session's meta if available, else `flow.session_lifetime`; document the choice in a comment), then proceed. Always `unlock` on every exit path of the winner branch (use a scopeguard-free explicit unlock before each return).
5. `Interactive` gains `refresh_enabled: bool` from `session.refresh` (default true); parse next to the other session fields.
6. Update the module doc (:45-47): refresh now exists in redis mode; cookie mode keeps re-login-on-expiry.

- [ ] **Step 3: Run + commit**

Run: new test + `cargo test openid` + `cargo test` + four gates.

```bash
git add src/plugins/native/openid_connect.rs
git commit -m "feat(openid-connect): lock-coordinated token refresh for redis sessions"
```

---

### Task 8: cas-auth redis mode

**Files:**
- Modify: `src/plugins/native/cas_auth.rs`

**Dispatch note (controller):** this brief references Task 6's `store_error` helper and test shape — paste that helper's code and Task 6's redis-mode test verbatim into the dispatch prompt; the standalone brief cannot see them.

**Interfaces:** consumes Task 5's helper; same `store_error` 503 pattern (code `SESSION_STORE_ERROR`), same config keys. Current seams: struct fields :91-103 (`sealer: Option<CookieSealer>`, cookie fields); write :369-384; read `read_session` :310-318 (sync) with caller :364; logout :353-360; NO flow cookie (CAS carries state in the service URL); subject = the sealed `user` field.

- [ ] **Step 1: Failing tests** — mirror Task 6's two tests, adapted: config `session_secret` + `session: {storage: redis, store: s1}` (note cas-auth reads `session_secret`/`session.secret` via `session_secret()` — the storage keys come from Task 5's `parse_backend`, which reads `session.storage`; both coexist); hand-put a sealed `CasSession { user: "alice" }`; assert pass-through sets `x-cas-user: alice`; assert outage → 503 `SESSION_STORE_ERROR`; assert `from_config` with `storage: redis` and no store errors with `requires 'session.store'`.
- [ ] **Step 2: Implement** — add `backend: SessionBackend` field (parse in `from_config` next to the sealer block :178-188; only meaningful when `sealer.is_some()`, but parse unconditionally — a redis backend without a secret is a config error: add `if sealer.is_none() && !matches!(backend, SessionBackend::Cookie) { return Err("cas-auth: session.storage requires session_secret (interactive mode)".into()) }`). Make `read_session` async+Result via `server_session::load`; update caller; write path via `server_session::establish` with `meta_now(&ctx, "cas-auth", &user, Duration::from_secs(self.cookie_lifetime))` (build meta BEFORE moving `user` into the payload); logout via `server_session::destroy` (read the cookie value first, as in Task 6 step 5); add the `store_error` helper (associated fn, matching this file's style).
- [ ] **Step 3: Run + commit**

```bash
git add src/plugins/native/cas_auth.rs
git commit -m "feat(cas-auth): redis session storage"
```

---

### Task 9: authz-casdoor redis mode

**Files:**
- Modify: `src/plugins/native/authz_casdoor.rs`

**Dispatch note (controller):** as Task 8 — carry Task 6's `store_error` helper code and redis-mode test shape in the dispatch prompt.

**Interfaces:** as Task 8. Current seams: session write in `handle_callback` :455-463 (two Set-Cookie values: session + flow-delete — keep both); read `read_session` :324-331 (sync) with caller :410; logout :396-401; flow cookie :466-493/:333-340 UNTOUCHED (client-side); subject = `claims["sub"]` as str, may be absent → empty subject (unindexed; document with a comment).

- [ ] **Step 1: Failing tests** — mirror Task 8: hand-put sealed `CasdoorSession { access_token: "tok", client_id: "app", claims: {"sub": "u1"} }` (must match `interactive_cfg()`'s client_id); assert pass sets `authorization: Bearer tok` + `message["user_id"] == "u1"`; outage → 503; config-validation test.
- [ ] **Step 2: Implement** — `backend` field parsed in `from_config` (same secret-required guard as Task 8, message prefixed `authz-casdoor:`); async read + caller; establish in `handle_callback` (meta subject from `claims.get("sub").and_then(|v| v.as_str()).unwrap_or("")`; `ttl = Duration::from_secs(self.cookie_lifetime)`); keep `del_flow` and the two-cookie redirect verbatim; logout via destroy; `store_error` associated fn.
- [ ] **Step 3: Run + commit**

```bash
git add src/plugins/native/authz_casdoor.rs
git commit -m "feat(authz-casdoor): redis session storage"
```

---

### Task 10: dingtalk-auth session restoration

**Files:**
- Modify: `src/plugins/native/dingtalk_auth.rs`, `src/plugins/mod.rs` (port-spec arm)

**Interfaces:**
- Consumes: Task 5 helper; existing per-request machinery (`extract_code` :170, `access_token` :191, `fetch_userinfo` :226, `attach_identity` :367, `reject` :254, `upstream_error` :272).
- Produces: restored APISIX session behavior. New config (all optional — **absent `session` secret = today's stateless token-validation mode, fully backward compatible**): `session.secret` (enables session mode), `session.storage`/`session.store` (Task 5), `session.cookie.name` default `"dingtalk_session"`, `session.cookie.path` default `/`, `session.cookie.lifetime` default `86400` (APISIX `cookie_expires_in`), `redirect_uri` (required in session mode: where to 302 when no code and no session). Flat fallbacks per convention. **Port spec: move `"dingtalk-auth"` (and in Task 11 `"feishu-auth"`) from the `AUTH_SPEC` arm (src/plugins/mod.rs:471-474) to the `INTERACTIVE_AUTH_SPEC` arm (:476)** — breaking for existing policies (documented Task 13).

Restored flow (session mode), per the APISIX original quoted in the module docs:
1. strip `x-userinfo` → 2. read session: valid → deserialize the stored userinfo JSON, `attach_identity`, success. Store outage → 503 `SESSION_STORE_ERROR`. Undecodable payload → destroy + treat as no-session (APISIX destroys + 302). → 3. `extract_code`: none → 302 to `redirect_uri` (redirect port). → 4. code present → existing token+userinfo callouts (unchanged) → establish session (payload = the userinfo `result` JSON bytes; subject = `userid` else `unionid` else ""; ttl = cookie lifetime) → attach + success with the Set-Cookie on the response (`ctx.response.headers` "set-cookie" insert — success responses CAN carry Set-Cookie; the engine forwards response headers).

- [ ] **Step 1: Failing tests**

```rust
    #[tokio::test]
    async fn test_session_mode_no_code_redirects() {
        let plugin = DingtalkAuthPlugin::from_config(
            &cfg(&[
                ("app_key", "k"), ("app_secret", "s"),
                ("redirect_uri", "https://login.example.com/start"),
            ])
            .into_iter()
            .chain([( "session".to_string(), serde_json::json!({"secret": "s3cr3t"}) )])
            .collect(),
            &PluginResources::empty(),
        )
        .unwrap();
        let out = plugin.execute(base_ctx()).await.unwrap();
        assert_eq!(out.port, Some("redirect"));
        assert_eq!(out.context.response.status_code, 302);
        assert_eq!(
            out.context.response.headers["location"],
            vec!["https://login.example.com/start".to_string()]
        );
    }

    #[tokio::test]
    async fn test_session_mode_valid_cookie_skips_callout() {
        // Cookie mode: hand-seal the userinfo JSON, present it, assert
        // success + identity attached WITHOUT any HTTP callout (endpoints
        // point at 127.0.0.1:1 — reaching them would error).
        // (build config with session.secret, token/userinfo URLs at
        // http://127.0.0.1:1, timeout 200; seal {"userid": "u1"} with
        // CookieSealer::new("s3cr3t"); cookie "dingtalk_session=<sealed>")
        // assert out.port.is_none() and message["user_id"] == "u1".
    }

    #[test]
    fn test_session_mode_requires_redirect_uri() {
        // session.secret set but no redirect_uri → config error naming it.
    }

    #[test]
    fn test_stateless_mode_unchanged() {
        // No session key → from_config succeeds without redirect_uri and
        // the existing test_missing_code_rejected_401 behavior holds.
    }
```

(Write the two sketched tests fully following the file's `cfg`/`base_ctx`/`resp` helpers — the comments state the required assertions.)

Run — Expected: FAIL / compile fail.

- [ ] **Step 2: Implement**

Add to the struct: `session: Option<DingtalkSession>` where

```rust
/// Session-mode settings (present when `session.secret` is configured).
struct DingtalkSession {
    sealer: CookieSealer,
    backend: crate::plugins::util::server_session::SessionBackend,
    cookie_name: String,
    cookie_path: String,
    cookie_lifetime: u64,
    redirect_uri: String,
}
```

Parse in `from_config` using the same `session_secret`/`session_cookie_str`/`session_cookie_u64` reader shapes as cas_auth :424-461 (copy those three helpers into this file — matching the existing duplication convention — or reference them if a shared home already exists); defaults `dingtalk_session`/`/`/`86400`; `redirect_uri` required iff session mode (`require_string` when `session.secret` present). `parse_backend` for storage. Rewrite `execute` per the flow contract; reuse `redirect`-style helper (copy the canonical `fn redirect` from openid_connect :1129-1141 as an associated fn) and add `store_error` (503). Move the factory port-spec arm: in `src/plugins/mod.rs`, remove `"dingtalk-auth"` from the AUTH_SPEC arm and add it to the `"cas-auth" | "openid-connect" | "authz-casdoor"` INTERACTIVE_AUTH_SPEC arm. Update the module doc's Deviations section: session machinery restored (cookie + redis modes), keys `secret`/`redirect_uri`/`cookie_expires_in`→`session.cookie.lifetime` returned; `secret_fallbacks` still NOT supported (single-secret CookieSealer — note as remaining deviation).

Also add one redis-mode test mirroring Task 6's (fake store, id cookie, outage→503), `#[cfg(feature = "redis-store")]`-gated.

- [ ] **Step 3: Run + commit**

Run: new tests + `cargo test dingtalk` + `cargo test` + four gates.

```bash
git add src/plugins/native/dingtalk_auth.rs src/plugins/mod.rs
git commit -m "feat(dingtalk-auth): restore session mode (cookie + redis) with 302 login redirect"
```

---

### Task 11: feishu-auth session restoration

**Files:**
- Modify: `src/plugins/native/feishu_auth.rs`, `src/plugins/mod.rs` (port-spec arm)

**Interfaces:** as Task 10 with two feishu-specific deltas (per the APISIX original): (a) cookie name default `"feishu_session"`; (b) the session payload caches BOTH the userinfo JSON and the app access token with its expiry — `struct FeishuSessionData { userinfo: serde_json::Value, access_token: Option<String>, access_token_expires_at: Option<u64> }` — on a NEW code exchange store `expires_at = now + expires_in - 60` (APISIX's skew), and when a session exists but a fresh code arrives... (keep it simple and APISIX-faithful: a session with valid userinfo short-circuits everything; the token cache matters only within the exchange flow — store it, reuse it if a future request needs a re-exchange after userinfo failure). Token response parsing already extracts `expires_in`? Check `parse_access_token` :286-314 — it currently validates presence of `access_token` only; extend it to also return `expires_in: Option<u64>` (tuple), adjusting its unit test. Subject = `user_id` else `open_id` else `union_id`.

- [ ] **Step 1: Failing tests** — mirror Task 10's four (no-code→302 to `redirect_uri`; valid sealed cookie skips callouts; session mode requires `redirect_uri`; stateless unchanged — note feishu ALSO keeps its existing required `auth_redirect_uri`, which is the token-exchange body field, distinct from the new interactive `redirect_uri`) + one redis-mode fake-store test.
- [ ] **Step 2: Implement** — as Task 10, plus the `parse_access_token` extension and the `FeishuSessionData` payload. Port-spec arm moves alongside (if Task 10 already moved both names, verify only). Module-doc Deviations updated (`secret_fallbacks` remains unsupported).
- [ ] **Step 3: Run + commit**

```bash
git add src/plugins/native/feishu_auth.rs src/plugins/mod.rs
git commit -m "feat(feishu-auth): restore session mode (cookie + redis) with 302 login redirect"
```

---

### Task 12: sessions Admin API

**Files:**
- Create: `src/admin/sessions.rs`
- Modify: `src/admin/mod.rs` (`mod sessions;` + `.merge(sessions::router())` BEFORE the auth `.layer(...)`)

**Interfaces:**
- Consumes: `SharedState.resources.stores.load().session_store(name)` (Task 3); `SessionFilter`/`SessionPage` (Task 1); admin harness conventions (`test_state`/`app`/`send` from `src/admin/stores.rs:285-319` — **dispatch note (controller):** paste those three helper functions verbatim into the dispatch prompt; the standalone brief cannot see stores.rs).
- Produces:
  - `GET /api/sessions?store=<name>&subject=&plugin=&limit=&cursor=` → `200 {"sessions": [SessionMeta...], "next_cursor": "..."|null}`; `store` is REQUIRED (400 without it); unknown store → 404; store outage → 502 `{"error": ...}`.
  - `DELETE /api/sessions/{store}/{id}` → `200 {"status":"deleted"}`; bad id format → 400; unknown store → 404; outage → 502.
  - `DELETE /api/sessions?store=<name>&subject=<s>` → `200 {"revoked": N}`; both params required (400).
  - Headless build: all three answer 501 (same pattern as `ping_store`).
  - No endpoint ever returns session content — meta only.

- [ ] **Step 1: Failing tests**

In the new file's test module (reuse the stores.rs harness shapes; a fake store must be injected into the LIVE registry: build `test_state` with a declared store `s1`, then `state.resources.stores.store(Arc::new(StoreRegistry::with_fake_session_store("s1", fake)))` — overriding the compiled registry with the fake for the test):

```rust
    #[cfg(feature = "redis-store")]
    #[tokio::test]
    async fn test_sessions_list_revoke_roundtrip() {
        // seed: fake store with two sessions (subjects alice, bob; plugin openid-connect)
        // GET /api/sessions?store=s1 → 200, 2 sessions, meta fields present, no payload field
        // GET /api/sessions?store=s1&subject=alice → 1
        // DELETE /api/sessions/s1/{alice's id} → 200 deleted
        // DELETE /api/sessions?store=s1&subject=bob → 200 {"revoked": 1}
        // GET → 0 sessions
    }

    #[cfg(feature = "redis-store")]
    #[tokio::test]
    async fn test_sessions_param_validation() {
        // GET /api/sessions (no store) → 400
        // GET /api/sessions?store=unknown → 404
        // DELETE /api/sessions/s1/not-a-valid-id → 400
        // DELETE /api/sessions?store=s1 (no subject) → 400
    }
```

(Write them fully; the comments are the required assertions. `serde` for query params: use `axum::extract::Query<HashMap<String, String>>` to keep it dependency-free.)

Run — Expected: COMPILE FAIL (no module).

- [ ] **Step 2: Implement**

`src/admin/sessions.rs` skeleton (follow stores.rs conventions exactly — `Router<Arc<SharedState>>`, ad-hoc `json!` envelopes, `#[cfg(feature = "redis-store")]` handler + 501 headless twin):

```rust
//! Admin surface for server-side sessions: list and revoke. Meta only —
//! session payloads are sealed and never leave the store through this API.

pub fn router() -> Router<Arc<SharedState>> {
    Router::new()
        .route(
            "/api/sessions",
            get(list_sessions).delete(delete_by_subject),
        )
        .route("/api/sessions/{store}/{id}", axum::routing::delete(delete_session))
}
```

Handlers resolve the store via `state.resources.stores.load().session_store(&name)` → unknown name = 404 `{"error":"not_found"}`; `SessionFilter { subject, plugin, limit: limit.unwrap_or(50), cursor }`; `StoreError` → 502 `{"error": e.to_string()}`; `SessionId::parse` failure → 400 `{"error":"invalid session id"}`. `delete_by_subject` requires both `store` and `subject` query params (400 listing the missing one).

- [ ] **Step 3: Run + commit**

Run: the two tests + `cargo test` + four gates (`cargo check --no-default-features` proves the 501 twins compile).

```bash
git add src/admin/sessions.rs src/admin/mod.rs
git commit -m "feat(admin): session list and revocation endpoints"
```

---

### Task 13: docs, spec amendments, final verification

**Files:**
- Modify: `website/docs/guides/admin-api.md` (sessions endpoint rows + mutation-notes bullet; front-matter description)
- Modify: `website/docs/reference/plugins/{openid-connect,cas-auth,authz-casdoor,dingtalk-auth,feishu-auth}.md`
- Modify: `website/docs/reference/roadmap.md`
- Modify: `docs/superpowers/specs/2026-08-21-session-storage-design.md` (inline "as shipped" amendments)
- Modify: `CLAUDE.md`

- [ ] **Step 1: Plugin pages**

- The three SSO pages: document `session.storage` (`cookie` default | `redis`), `session.store`, the 503 store-failure semantics, the revocation capability (+ pointer to `/api/sessions`), and that the flow cookie stays client-side. openid-connect additionally documents `session.refresh` (redis mode; default true; lock-coordinated; failed refresh falls back to re-login).
- dingtalk/feishu pages: REPLACE the ":::note Limitations — token validation only" admonitions — session mode restored (both storages), new keys (`session.secret`, `session.cookie.*` incl. `lifetime` default 86400, `redirect_uri`), the 302-when-unauthenticated behavior, and the remaining deviation (`secret_fallbacks` unsupported). **Breaking-change callout:** both plugins now declare the `redirect` outcome port — existing policies must wire it (compile error otherwise).

- [ ] **Step 2: Admin API reference + roadmap + CLAUDE.md**

admin-api.md rows (`:name` docs style):

```markdown
| GET | `/api/sessions?store=&subject=&plugin=&limit=&cursor=` | List server-side session metadata (never payloads) | 400 missing store; 404 unknown store; 502 store outage; 501 headless build |
| DELETE | `/api/sessions/:store/:id` | Revoke one session | 400 bad id; 404; 502; 501 |
| DELETE | `/api/sessions?store=&subject=` | Revoke every session for a subject | 400 missing params; 404; 502; 501 |
```

Mutation-notes bullet: revocation applies to store-backed sessions only; `storage: cookie` sessions remain unrevocable by design. Roadmap: sessions shipped for the five plugins; UI panel + e2e + CI matrix = Plan 3. CLAUDE.md: extend the Shared-stores bullet (sessions now a consumer) and the openid-connect/session mentions; note the dingtalk/feishu restoration and their port change.

- [ ] **Step 3: Spec amendments (inline "Amendment (as shipped):" style)**

In §2: (a) refresh was greenfield — the oidc port had no refresh; shipped in redis mode only, cookie mode unchanged; (b) `SessionMeta` carries `policy` and `route` via the new `__route`/`__policy` context vars; (c) `session_store_errors` metric is `gateway_session_store_errors_total{store}`; (d) dingtalk/feishu port-spec move is a breaking change (redirect port mandatory); (e) `secret_fallbacks` remains unsupported (single-secret sealer).

- [ ] **Step 4: Full verification**

```bash
cargo fmt --check
cargo test
cargo clippy --all-targets -- -D warnings
cargo check --no-default-features
cargo clippy --no-default-features --all-targets -- -D warnings
cargo tree -i aws-lc-sys   # still empty
cd website && npm run build
```

Optional live pass: `docker run -d --rm -p 16379:6379 redis:7` + `FEATHERBIT_TEST_REDIS_URL=redis://127.0.0.1:16379 cargo test live` (then the same against `valkey/valkey:8`).

- [ ] **Step 5: Commit**

```bash
git add website/docs CLAUDE.md docs/superpowers/specs/2026-08-21-session-storage-design.md
git commit -m "docs: server-side sessions, restored dingtalk/feishu session mode, spec amendments"
```

---

## Out of scope for this plan (tracked)

- **Plan 3**: UI (Sessions panel, `session.storage`/`session.store` pickers in the node config schema — `ui/src/pluginConfig.ts` entries for all five plugins, dingtalk/feishu schema gains the session keys), e2e scenarios (redis-backed OIDC login against the Keycloak realm in `tests/`, revocation via panel, restart-survival), CI redis/valkey service-container matrix, `website/docs/concepts/stores.md`.
- `secret_fallbacks` (key rotation) for the session sealer — deviation retained, documented.
- IdP-side logout (end-session endpoint / CAS single logout) — pre-existing gap, unchanged.
- The old `e2e/E2E_TESTBOOK.md` rows land with Plan 3's e2e work.
