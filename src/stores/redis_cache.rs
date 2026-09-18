//! Shared response cache over a declared `stores:` entry.
//!
//! One redis hash per entry: `status`, `headers` (JSON) and `body` (raw
//! bytes). Redis values are binary-safe, so the body is stored as-is --
//! base64 would inflate every cached response by a third, and a JSON wrapper
//! would escape it on top of that.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use redis::AsyncCommands;

use crate::stores::namespaces;
use crate::stores::redis_store::RedisStoreClient;
use crate::traffic::{CacheError, CachedResponse, ResponseCache};

/// The redis key for a cache entry.
pub(crate) fn cache_key(prefix: &str, key: &str) -> String {
    format!("{}:{}:{}", prefix, namespaces::CACHE, key)
}

pub struct RedisResponseCache {
    pub(crate) client: Arc<RedisStoreClient>,
}

impl RedisResponseCache {
    pub fn new(client: Arc<RedisStoreClient>) -> Self {
        Self { client }
    }

    pub(crate) fn redis_key(&self, key: &str) -> String {
        cache_key(self.client.key_prefix(), key)
    }
}

#[async_trait]
impl ResponseCache for RedisResponseCache {
    async fn get(&self, key: &str) -> Result<Option<CachedResponse>, CacheError> {
        let mut conn = self.client.conn().await.map_err(CacheError)?;
        let fields: HashMap<String, Vec<u8>> = conn
            .hgetall(self.redis_key(key))
            .await
            .map_err(|e| CacheError(e.to_string()))?;

        if fields.is_empty() {
            return Ok(None);
        }

        // A hash the gateway cannot parse is unusable either way, so it reads
        // as a miss rather than an error: the request keeps moving, and the
        // two outcomes stay distinguishable in metrics.
        let (Some(status), Some(headers), Some(body)) = (
            fields.get("status"),
            fields.get("headers"),
            fields.get("body"),
        ) else {
            return Ok(None);
        };
        let Ok(status) = String::from_utf8_lossy(status).parse::<u16>() else {
            return Ok(None);
        };
        let Ok(headers) = serde_json::from_slice::<HashMap<String, Vec<String>>>(headers) else {
            return Ok(None);
        };

        Ok(Some(CachedResponse {
            status,
            headers,
            body: Bytes::copy_from_slice(body),
        }))
    }

    async fn put(
        &self,
        key: &str,
        entry: &CachedResponse,
        ttl: Duration,
    ) -> Result<(), CacheError> {
        let mut conn = self.client.conn().await.map_err(CacheError)?;
        let redis_key = self.redis_key(key);
        let headers = serde_json::to_vec(&entry.headers).map_err(|e| CacheError(e.to_string()))?;

        // Fields and expiry in one pipeline: an entry without a TTL would
        // outlive its freshness and never be swept.
        redis::pipe()
            .atomic()
            .hset(&redis_key, "status", entry.status.to_string())
            .hset(&redis_key, "headers", headers)
            .hset(&redis_key, "body", entry.body.as_ref())
            .expire(&redis_key, ttl.as_secs().max(1) as i64)
            .query_async::<()>(&mut conn)
            .await
            .map_err(|e| CacheError(e.to_string()))
    }
}

#[cfg(all(test, feature = "redis-store"))]
mod tests {
    use super::*;

    fn store_url() -> Option<String> {
        std::env::var("FEATHERBIT_TEST_REDIS_URL")
            .ok()
            .filter(|s| !s.is_empty())
    }

    fn client(url: &str) -> Arc<crate::stores::redis_store::RedisStoreClient> {
        let cfg: crate::config::StoreConfig = serde_yaml::from_str(&format!(
            "name: cache-test\ntype: redis\nurl: {url}\nkey_prefix: fbtest\n"
        ))
        .unwrap();
        Arc::new(crate::stores::redis_store::RedisStoreClient::build(&cfg).unwrap())
    }

    fn response(body: &[u8]) -> CachedResponse {
        let mut headers = HashMap::new();
        headers.insert(
            "x-multi".to_string(),
            vec!["a".to_string(), "b".to_string()],
        );
        CachedResponse {
            status: 203,
            headers,
            body: Bytes::copy_from_slice(body),
        }
    }

    /// The point of the whole feature: an entry written by one instance is
    /// readable by another. Two handles over the same store stand in for two
    /// gateways -- nothing else demonstrates sharing.
    #[tokio::test]
    async fn test_an_entry_written_by_one_handle_is_read_by_another() {
        let Some(url) = store_url() else { return };
        let writer = RedisResponseCache::new(client(&url));
        let reader = RedisResponseCache::new(client(&url));
        let key = format!("share-{}", uuid::Uuid::new_v4());

        writer
            .put(&key, &response(b"shared"), Duration::from_secs(60))
            .await
            .unwrap();

        let got = reader
            .get(&key)
            .await
            .unwrap()
            .expect("the second handle must see it");
        assert_eq!(got.body, Bytes::from_static(b"shared"));
    }

    /// Binary bodies and multi-value headers must survive the encoding: a
    /// base64 or JSON wrapper would be the easy way to get this wrong.
    #[tokio::test]
    async fn test_a_binary_body_and_multi_value_header_round_trip() {
        let Some(url) = store_url() else { return };
        let cache = RedisResponseCache::new(client(&url));
        let key = format!("bin-{}", uuid::Uuid::new_v4());
        let body = vec![0u8, 159, 146, 150, 255];

        cache
            .put(&key, &response(&body), Duration::from_secs(60))
            .await
            .unwrap();

        let got = cache.get(&key).await.unwrap().unwrap();
        assert_eq!(got.status, 203);
        assert_eq!(got.body.as_ref(), body.as_slice());
        assert_eq!(
            got.headers.get("x-multi").unwrap(),
            &vec!["a".to_string(), "b".to_string()]
        );
    }

    #[tokio::test]
    async fn test_a_missing_key_is_ok_none() {
        let Some(url) = store_url() else { return };
        let cache = RedisResponseCache::new(client(&url));
        assert!(cache
            .get(&format!("absent-{}", uuid::Uuid::new_v4()))
            .await
            .unwrap()
            .is_none());
    }

    /// Expiry is redis's own TTL, never an Instant comparison -- two instances
    /// do not share a monotonic clock.
    #[tokio::test]
    async fn test_the_key_carries_a_ttl() {
        let Some(url) = store_url() else { return };
        let cache = RedisResponseCache::new(client(&url));
        let key = format!("ttl-{}", uuid::Uuid::new_v4());
        cache
            .put(&key, &response(b"x"), Duration::from_secs(30))
            .await
            .unwrap();

        let mut conn = cache.client.conn().await.unwrap();
        let ttl: i64 = redis::cmd("TTL")
            .arg(cache.redis_key(&key))
            .query_async(&mut conn)
            .await
            .unwrap();
        assert!(
            ttl > 0,
            "the entry must expire on redis's clock, not ours: {ttl}"
        );
    }
}
