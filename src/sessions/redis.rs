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
        let meta_json =
            serde_json::to_string(meta).map_err(|e| self.err(format!("meta serialize: {e}")))?;
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
