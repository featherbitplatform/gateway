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

/// Process-local cache. Shared by every `policy: local` node in the gateway.
#[derive(Default)]
pub struct LocalResponseCache {
    entries: DashMap<String, (CachedResponse, Instant)>,
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
}
