//! Pluggable counter backends for rate-limiting plugins.
//!
//! The featherbit analogue of APISIX's `policy: local | redis` counter
//! abstraction. Rate-limit plugins (limit-count, api-breaker, ...) resolve a
//! per-client key, then ask a [`CounterStore`] to count it within a fixed
//! window. `local` (in-memory, per gateway instance) is always available; a
//! Redis-backed store for cross-instance limits is planned behind a feature
//! flag and slots in as another [`CounterStore`] implementation.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use dashmap::DashMap;

/// Outcome of counting one request against a fixed window.
#[derive(Debug, Clone)]
pub struct WindowResult {
    /// Whether this request fits within the limit.
    pub allowed: bool,
    /// Requests remaining in the current window (0 when rejected).
    pub remaining: u64,
    /// Time until the current window resets.
    pub reset: Duration,
    /// The configured limit (echoed for `X-RateLimit-Limit`).
    pub limit: u64,
}

/// Counter backend failure (e.g. an unreachable Redis). Plugins decide
/// whether to fail open (`allow_degradation`) or reject.
#[derive(Debug)]
pub struct CounterError(pub String);

impl std::fmt::Display for CounterError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "counter store error: {}", self.0)
    }
}

/// A backend that counts requests per key within fixed time windows.
#[async_trait]
pub trait CounterStore: Send + Sync {
    /// Atomically counts one request for `key` in the current fixed window
    /// of length `window`, allowing up to `limit` requests per window.
    async fn incr_fixed_window(
        &self,
        key: &str,
        limit: u64,
        window: Duration,
    ) -> Result<WindowResult, CounterError>;
}

/// In-memory fixed-window counters (per gateway instance).
///
/// Windows are tracked per key as `(window_start, count)`; a request past
/// the window's end lazily starts a fresh window. Entries are never actively
/// expired — stale keys are reset on their next request; memory is bounded
/// by key cardinality, same as the existing token-bucket plugin.
#[derive(Default)]
pub struct LocalCounterStore {
    windows: DashMap<String, (Instant, u64)>,
}

#[async_trait]
impl CounterStore for LocalCounterStore {
    async fn incr_fixed_window(
        &self,
        key: &str,
        limit: u64,
        window: Duration,
    ) -> Result<WindowResult, CounterError> {
        let now = Instant::now();
        let mut entry = self
            .windows
            .entry(key.to_string())
            .or_insert_with(|| (now, 0));

        let (start, count) = *entry;
        let (start, count) = if now.duration_since(start) >= window {
            (now, 0)
        } else {
            (start, count)
        };

        let allowed = count < limit;
        let new_count = if allowed { count + 1 } else { count };
        *entry = (start, new_count);

        let elapsed = now.duration_since(start);
        Ok(WindowResult {
            allowed,
            remaining: limit.saturating_sub(new_count),
            reset: window.saturating_sub(elapsed),
            limit,
        })
    }
}

/// Resolves a `policy` config value to a counter backend.
///
/// `local` is always available. Unknown policies (including `redis` until
/// the feature lands) fail at config load with a descriptive error.
pub struct CounterStoreRegistry {
    stores: HashMap<String, Arc<dyn CounterStore>>,
}

impl Default for CounterStoreRegistry {
    fn default() -> Self {
        let mut stores: HashMap<String, Arc<dyn CounterStore>> = HashMap::new();
        stores.insert("local".to_string(), Arc::new(LocalCounterStore::default()));
        Self { stores }
    }
}

impl CounterStoreRegistry {
    /// Returns the backend for a policy name.
    pub fn get(&self, policy: &str) -> Result<Arc<dyn CounterStore>, String> {
        self.stores.get(policy).cloned().ok_or_else(|| {
            format!("unknown rate-limit policy '{}' — supported: {}", policy, {
                let mut names: Vec<&str> = self.stores.keys().map(String::as_str).collect();
                names.sort_unstable();
                names.join(", ")
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_fixed_window_counts_and_rejects() {
        let store = LocalCounterStore::default();
        let window = Duration::from_secs(60);

        for i in 0..3 {
            let r = store.incr_fixed_window("k", 3, window).await.unwrap();
            assert!(r.allowed, "request {} should pass", i);
            assert_eq!(r.remaining, 2 - i);
            assert_eq!(r.limit, 3);
        }
        let r = store.incr_fixed_window("k", 3, window).await.unwrap();
        assert!(!r.allowed);
        assert_eq!(r.remaining, 0);
        assert!(r.reset <= window);

        // separate keys have separate windows
        let r = store.incr_fixed_window("other", 3, window).await.unwrap();
        assert!(r.allowed);
    }

    #[tokio::test]
    async fn test_window_resets_after_expiry() {
        let store = LocalCounterStore::default();
        let window = Duration::from_millis(30);

        let r = store.incr_fixed_window("k", 1, window).await.unwrap();
        assert!(r.allowed);
        let r = store.incr_fixed_window("k", 1, window).await.unwrap();
        assert!(!r.allowed);

        tokio::time::sleep(Duration::from_millis(40)).await;
        let r = store.incr_fixed_window("k", 1, window).await.unwrap();
        assert!(r.allowed, "window should have reset");
    }

    #[tokio::test]
    async fn test_registry_lookup() {
        let registry = CounterStoreRegistry::default();
        assert!(registry.get("local").is_ok());
        let err = match registry.get("redis") {
            Ok(_) => panic!("'redis' should not resolve yet"),
            Err(e) => e,
        };
        assert!(err.contains("unknown rate-limit policy"), "{err}");
    }
}
