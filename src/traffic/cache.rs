//! Response-cache backends for the `proxy-cache` node.
//!
//! One trait, two implementations: a process-local map and (from the redis
//! backend onwards) a shared store. `proxy-cache` holds whichever it was
//! configured with and never learns which.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use bytes::Bytes;
use dashmap::DashMap;

/// A cached upstream response, without any notion of when it expires.
///
/// Lifetime is the backend's business: the local map compares `Instant`s,
/// while a redis entry lives on the key's own TTL. Carrying an `Instant` here
/// would be meaningless to a second instance, which does not share the first
/// one's monotonic clock.
#[derive(Debug, Clone)]
pub struct CachedResponse {
    pub status: u16,
    pub headers: HashMap<String, Vec<String>>,
    pub body: Bytes,
}

/// A backend could not answer. Distinct from a miss; see [`ResponseCache`].
#[derive(Debug)]
pub struct CacheError(pub String);

impl std::fmt::Display for CacheError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "response cache error: {}", self.0)
    }
}

/// Storage for `proxy-cache`.
///
/// `Ok(None)` means the backend answered and had nothing; `Err` means it could
/// not answer. Keeping those apart is deliberate: the caller decides that an
/// outage should read as a miss, and that decision belongs somewhere a
/// reviewer can find it rather than inside a backend that swallows its own
/// errors.
#[async_trait]
pub trait ResponseCache: Send + Sync {
    async fn get(&self, key: &str) -> Result<Option<CachedResponse>, CacheError>;
    async fn put(&self, key: &str, entry: &CachedResponse, ttl: Duration)
        -> Result<(), CacheError>;
}

/// Entries the local cache keeps before it starts evicting.
const DEFAULT_MAX_ENTRIES: usize = 10_000;

/// Process-local cache. Shared by every `policy: local` node in the gateway.
pub struct LocalResponseCache {
    entries: DashMap<String, (CachedResponse, Instant)>,
    /// Read on every insert; set once at startup from `cache.max_entries`.
    capacity: std::sync::atomic::AtomicUsize,
}

impl Default for LocalResponseCache {
    fn default() -> Self {
        Self {
            entries: DashMap::new(),
            capacity: std::sync::atomic::AtomicUsize::new(DEFAULT_MAX_ENTRIES),
        }
    }
}

impl LocalResponseCache {
    /// Sets the entry bound. Called once at startup, before traffic.
    pub fn set_capacity(&self, max_entries: usize) {
        self.capacity
            .store(max_entries.max(1), std::sync::atomic::Ordering::Relaxed);
    }

    /// Entries currently held, live or not yet swept.
    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Makes room for one more entry.
    ///
    /// Expired entries go first, since they are already worthless. If that is
    /// not enough, the entry expiring soonest goes next — deliberately not an
    /// LRU (no LRU crate is in the dependency tree, and adding one for this is
    /// not worth the supply-chain surface). For a cache whose entries all
    /// carry TTLs this discards what was about to become useless anyway; the
    /// cost is that a hot short-TTL entry loses to a cold long-TTL one.
    fn make_room(&self, capacity: usize) {
        if self.entries.len() < capacity {
            return;
        }
        let now = Instant::now();
        self.entries.retain(|_, (_, expires_at)| now < *expires_at);

        while self.entries.len() >= capacity {
            let soonest = self
                .entries
                .iter()
                .min_by_key(|e| e.value().1)
                .map(|e| e.key().clone());
            match soonest {
                Some(key) => {
                    self.entries.remove(&key);
                }
                None => break,
            }
        }
    }
}

#[async_trait]
impl ResponseCache for LocalResponseCache {
    async fn get(&self, key: &str) -> Result<Option<CachedResponse>, CacheError> {
        let Some(found) = self.entries.get(key) else {
            return Ok(None);
        };
        if Instant::now() < found.1 {
            let hit = found.0.clone();
            Ok(Some(hit))
        } else {
            drop(found);
            self.entries.remove(key);
            Ok(None)
        }
    }

    async fn put(
        &self,
        key: &str,
        entry: &CachedResponse,
        ttl: Duration,
    ) -> Result<(), CacheError> {
        self.make_room(self.capacity.load(std::sync::atomic::Ordering::Relaxed));
        self.entries
            .insert(key.to_string(), (entry.clone(), Instant::now() + ttl));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn response(body: &str) -> CachedResponse {
        let mut headers = HashMap::new();
        headers.insert("content-type".to_string(), vec!["text/plain".to_string()]);
        CachedResponse {
            status: 200,
            headers,
            body: Bytes::from(body.to_string()),
        }
    }

    #[tokio::test]
    async fn test_local_round_trips_a_response() {
        let cache = LocalResponseCache::default();
        cache
            .put("k", &response("hello"), Duration::from_secs(60))
            .await
            .unwrap();

        let got = cache.get("k").await.unwrap().expect("a stored entry");
        assert_eq!(got.status, 200);
        assert_eq!(got.body, Bytes::from("hello"));
        assert_eq!(
            got.headers.get("content-type").unwrap(),
            &vec!["text/plain".to_string()]
        );
    }

    #[tokio::test]
    async fn test_local_miss_is_ok_none_not_an_error() {
        // The distinction the whole design rests on: "nothing stored" is a
        // successful answer, not a failure. Only a backend that could not
        // answer returns Err.
        let cache = LocalResponseCache::default();
        assert!(cache.get("absent").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn test_local_entry_expires() {
        let cache = LocalResponseCache::default();
        cache
            .put("k", &response("x"), Duration::from_millis(1))
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(
            cache.get("k").await.unwrap().is_none(),
            "an expired entry must not be served"
        );
    }

    #[tokio::test]
    async fn test_local_evicts_expired_entries_before_live_ones() {
        let cache = LocalResponseCache::default();
        cache.set_capacity(2);

        // One entry that is already dead, one that is not.
        cache
            .put("dead", &response("d"), Duration::from_millis(1))
            .await
            .unwrap();
        cache
            .put("live", &response("l"), Duration::from_secs(60))
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;

        // At capacity: the expired entry is the one that must go, not the
        // useful one.
        cache
            .put("new", &response("n"), Duration::from_secs(60))
            .await
            .unwrap();

        assert!(cache.get("dead").await.unwrap().is_none());
        assert!(
            cache.get("live").await.unwrap().is_some(),
            "a live entry must survive a sweep"
        );
        assert!(cache.get("new").await.unwrap().is_some());
    }

    #[tokio::test]
    async fn test_local_at_capacity_evicts_the_soonest_to_expire() {
        let cache = LocalResponseCache::default();
        cache.set_capacity(2);

        cache
            .put("short", &response("s"), Duration::from_secs(1))
            .await
            .unwrap();
        cache
            .put("long", &response("l"), Duration::from_secs(600))
            .await
            .unwrap();
        cache
            .put("new", &response("n"), Duration::from_secs(600))
            .await
            .unwrap();

        // Nothing has expired, so the bound falls back to discarding what was
        // about to become worthless anyway. This is NOT an LRU and the test
        // says so: a hot short-TTL entry loses to a cold long-TTL one.
        assert!(cache.get("short").await.unwrap().is_none());
        assert!(cache.get("long").await.unwrap().is_some());
        assert!(cache.get("new").await.unwrap().is_some());
    }

    #[tokio::test]
    async fn test_local_never_exceeds_its_capacity() {
        let cache = LocalResponseCache::default();
        cache.set_capacity(4);
        for i in 0..50 {
            cache
                .put(&format!("k{i}"), &response("x"), Duration::from_secs(600))
                .await
                .unwrap();
        }
        assert!(
            cache.len() <= 4,
            "the bound must hold under sustained writes: {}",
            cache.len()
        );
    }
}
