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

use std::time::Duration;

use async_trait::async_trait;
use ring::rand::{SecureRandom, SystemRandom};
use serde::{Deserialize, Serialize};

#[cfg(feature = "redis-store")]
pub mod redis;

/// A 128-bit random session id, hex-encoded (32 chars). The only thing the
/// browser holds in redis mode, and deliberately unguessable.
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(dead_code)]
pub struct SessionId(String);

#[allow(dead_code)]
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
#[allow(dead_code)]
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
#[allow(dead_code)]
pub struct SessionFilter {
    pub subject: Option<String>,
    pub plugin: Option<String>,
    pub limit: usize,
    pub cursor: Option<String>,
}

/// One page of session metadata.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct SessionPage {
    pub sessions: Vec<SessionMeta>,
    pub next_cursor: Option<String>,
}

/// Session-store backend failure. Always maps to 503 on the plugin's
/// `error` port; callers must never treat it as "unauthenticated".
#[derive(Debug)]
#[allow(dead_code)]
pub struct StoreError(pub String);

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "session store error: {}", self.0)
    }
}

/// A backend holding sealed session payloads plus their meta envelopes.
#[async_trait]
#[allow(dead_code)]
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
    inner: std::sync::Mutex<
        std::collections::HashMap<String, (Vec<u8>, SessionMeta, std::time::Instant)>,
    >,
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
            (
                sealed.to_vec(),
                meta.clone(),
                std::time::Instant::now() + ttl,
            ),
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
        assert_eq!(
            store.get(&id).await.unwrap().as_deref(),
            Some(&b"sealed"[..])
        );

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
