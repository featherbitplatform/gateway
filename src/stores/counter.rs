//! Redis-backed fixed-window counter (`policy: redis` for limit-count and
//! the workflow limit-count action).
//!
//! Increment-and-check runs as one server-side Lua script so concurrent
//! gateway instances count atomically. Window boundaries are wall-clock
//! aligned (`now / window`), so every instance agrees on them — unlike the
//! local store's per-process `Instant` windows. Backend errors bump
//! `gateway_counter_store_errors_total{store}` and surface as
//! [`CounterError`]; the calling plugin decides fail-open vs reject.

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;

use crate::metrics::GatewayMetrics;
use crate::ratelimit::{CounterError, CounterStore, WindowResult};

use super::redis_store::RedisStoreClient;

/// `INCR` + first-increment `PEXPIRE` + `PTTL`, atomically.
const FIXED_WINDOW_SCRIPT: &str = r#"
local current = redis.call('INCR', KEYS[1])
if current == 1 then
  redis.call('PEXPIRE', KEYS[1], ARGV[1])
end
local ttl = redis.call('PTTL', KEYS[1])
return {current, ttl}
"#;

/// Wall-clock window slot: the window index (key component shared by all
/// instances) and the milliseconds from `now_ms` to the window's end (the
/// PEXPIRE argument). Pure, so the boundary math is unit-testable.
fn window_slot(now_ms: u64, window_ms: u64) -> (u64, u64) {
    let window_ms = window_ms.max(1);
    let start = now_ms / window_ms;
    let expire_ms = (start + 1) * window_ms - now_ms;
    (start, expire_ms)
}

pub struct RedisCounterStore {
    client: Arc<RedisStoreClient>,
    store_name: String,
    metrics: Option<Arc<GatewayMetrics>>,
    script: redis::Script,
}

impl RedisCounterStore {
    pub fn new(
        client: Arc<RedisStoreClient>,
        store_name: String,
        metrics: Option<Arc<GatewayMetrics>>,
    ) -> Self {
        Self {
            client,
            store_name,
            metrics,
            script: redis::Script::new(FIXED_WINDOW_SCRIPT),
        }
    }

    fn backend_err(&self, msg: String) -> CounterError {
        if let Some(ref m) = self.metrics {
            m.counter_store_errors
                .with_label_values(&[&self.store_name])
                .inc();
        }
        tracing::warn!(store = %self.store_name, "counter store error: {}", msg);
        CounterError(msg)
    }
}

#[async_trait]
impl CounterStore for RedisCounterStore {
    async fn incr_fixed_window(
        &self,
        key: &str,
        limit: u64,
        window: Duration,
    ) -> Result<WindowResult, CounterError> {
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        let (slot, expire_ms) = window_slot(now_ms, window.as_millis() as u64);
        let redis_key = format!("{}:cnt:{}:{}", self.client.key_prefix(), slot, key);

        let mut conn = self.client.conn().await.map_err(|e| self.backend_err(e))?;
        let (count, pttl): (u64, i64) = self
            .script
            .key(redis_key.as_str())
            .arg(expire_ms)
            .invoke_async(&mut conn)
            .await
            .map_err(|e| self.backend_err(format!("fixed-window script: {}", e)))?;

        Ok(WindowResult {
            allowed: count <= limit,
            remaining: limit.saturating_sub(count),
            reset: Duration::from_millis(pttl.max(0) as u64),
            limit,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Window slots are wall-clock aligned and expiry lands exactly on the
    /// window boundary — the property that makes limits cluster-consistent.
    #[test]
    fn test_window_slot_alignment() {
        // 10s window: 25_000ms is 5s into slot 2, 5s left.
        assert_eq!(window_slot(25_000, 10_000), (2, 5_000));
        // Exactly on a boundary: full window remains.
        assert_eq!(window_slot(30_000, 10_000), (3, 10_000));
        // 1ms before the boundary.
        assert_eq!(window_slot(29_999, 10_000), (2, 1));
        // Degenerate zero window is clamped, never divides by zero.
        assert_eq!(window_slot(5, 0), (5, 1));
    }

    /// Live-backend atomicity test; skipped unless FEATHERBIT_TEST_REDIS_URL
    /// is set. N concurrent tasks never over-admit past the limit.
    #[tokio::test]
    async fn test_concurrent_increments_never_exceed_limit_live() {
        let Ok(url) = std::env::var("FEATHERBIT_TEST_REDIS_URL") else {
            eprintln!(
                "skipping test_concurrent_increments_never_exceed_limit_live: FEATHERBIT_TEST_REDIS_URL not set"
            );
            return;
        };
        let cfg: crate::config::StoreConfig = serde_yaml::from_str(&format!(
            "name: live\ntype: redis\nurl: {url}\nkey_prefix: fbtest{}\n",
            std::process::id()
        ))
        .unwrap();
        let client = Arc::new(RedisStoreClient::build(&cfg).unwrap());
        let store = Arc::new(RedisCounterStore::new(client, "live".to_string(), None));

        let limit = 10u64;
        let window = Duration::from_secs(60);
        let mut handles = Vec::new();
        for _ in 0..40 {
            let store = store.clone();
            handles.push(tokio::spawn(async move {
                store
                    .incr_fixed_window("conc-key", limit, window)
                    .await
                    .unwrap()
                    .allowed
            }));
        }
        let mut admitted = 0;
        for h in handles {
            if h.await.unwrap() {
                admitted += 1;
            }
        }
        assert_eq!(admitted as u64, limit, "exactly `limit` requests admitted");
    }
}
