//! Shared state for traffic-control plugins whose logic spans the upstream
//! call — concurrency limits, circuit breakers, and response caching.
//!
//! A featherbit node occupies a single graph position, but these behaviors
//! need to act both *before* the upstream (acquire a slot / check the breaker
//! / look up the cache) and *after* it (release / observe the status / store
//! the response). The idiomatic featherbit expression is a **pair of nodes**
//! wired around `upstream`, both configured with the same `id` and sharing an
//! entry in one of these process-wide registries — the same "two phases, one
//! shared key" shape `proxy-rewrite` uses for request/response.
//!
//! All three registries are lock-light concurrent maps keyed by the plugin's
//! configured `id`, so multiple routes can maintain independent state and a
//! pair on one route shares exactly one entry.

use std::sync::atomic::AtomicI64;
use std::sync::Arc;
use std::time::{Duration, Instant};

use dashmap::DashMap;
use tokio::sync::Mutex;

pub mod cache;
// `CacheError` joins the re-export now that the redis backend (`redis_cache.rs`)
// is a real non-test caller that needs to name it — but that caller only
// exists under `redis-store`, so a headless build still has no user for it.
#[cfg(feature = "redis-store")]
pub use cache::CacheError;
pub use cache::{CachedResponse, LocalResponseCache, ResponseCache};

/// Per-key in-flight request counters for `limit-conn`.
///
/// The acquire node increments and the release node decrements the same
/// counter, so concurrency is measured across the whole pipeline between the
/// paired nodes.
#[derive(Default)]
pub struct ConnRegistry {
    counters: DashMap<String, Arc<AtomicI64>>,
}

impl ConnRegistry {
    /// Returns the shared counter for `key`, creating it on first use.
    pub fn counter(&self, key: &str) -> Arc<AtomicI64> {
        self.counters
            .entry(key.to_string())
            .or_insert_with(|| Arc::new(AtomicI64::new(0)))
            .clone()
    }
}

/// State of one circuit breaker (shared by an `api-breaker` check/observe pair).
#[derive(Default)]
pub struct BreakerState {
    /// Consecutive unhealthy responses observed while closed.
    unhealthy_count: u32,
    /// Consecutive healthy responses observed while half-open.
    healthy_count: u32,
    /// How many times the breaker has tripped in the current unhealthy spell,
    /// used to grow the cooldown.
    trip_round: u32,
    /// When the breaker is open, the instant it may move to half-open.
    open_until: Option<Instant>,
}

impl BreakerState {
    /// Whether a request should be allowed through right now. When the open
    /// window has elapsed the breaker becomes half-open (this call returns
    /// `true` to let one probe through).
    pub fn allow(&mut self) -> bool {
        match self.open_until {
            Some(until) if Instant::now() < until => false,
            Some(_) => {
                // Cooldown elapsed → half-open: let a probe through.
                self.open_until = None;
                true
            }
            None => true,
        }
    }

    /// Records a healthy upstream response. After `healthy_threshold`
    /// consecutive healthy responses the breaker fully closes.
    pub fn record_healthy(&mut self, healthy_threshold: u32) {
        self.unhealthy_count = 0;
        self.healthy_count = self.healthy_count.saturating_add(1);
        if self.healthy_count >= healthy_threshold {
            self.healthy_count = 0;
            self.trip_round = 0;
        }
    }

    /// Records an unhealthy upstream response. After `unhealthy_threshold`
    /// consecutive unhealthy responses the breaker opens for
    /// `min(max_breaker_sec, break_base_sec * 2^trip_round)`.
    pub fn record_unhealthy(
        &mut self,
        unhealthy_threshold: u32,
        break_base_sec: u64,
        max_breaker_sec: u64,
    ) {
        self.healthy_count = 0;
        self.unhealthy_count = self.unhealthy_count.saturating_add(1);
        if self.unhealthy_count >= unhealthy_threshold {
            self.unhealthy_count = 0;
            let backoff = break_base_sec
                .saturating_mul(1u64 << self.trip_round.min(16))
                .min(max_breaker_sec.max(break_base_sec));
            self.open_until = Some(Instant::now() + Duration::from_secs(backoff));
            self.trip_round = self.trip_round.saturating_add(1);
        }
    }
}

/// Circuit-breaker registry for `api-breaker` node pairs.
#[derive(Default)]
pub struct BreakerRegistry {
    breakers: DashMap<String, Arc<Mutex<BreakerState>>>,
}

impl BreakerRegistry {
    /// Returns the shared breaker state for `key`, creating it on first use.
    pub fn breaker(&self, key: &str) -> Arc<Mutex<BreakerState>> {
        self.breakers
            .entry(key.to_string())
            .or_insert_with(|| Arc::new(Mutex::new(BreakerState::default())))
            .clone()
    }
}

/// The three registries, held in `PluginResources`.
#[derive(Default)]
pub struct TrafficRegistries {
    pub conn: ConnRegistry,
    pub breakers: BreakerRegistry,
    pub cache: Arc<LocalResponseCache>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::Ordering;

    #[test]
    fn test_conn_counter_shared() {
        let reg = ConnRegistry::default();
        let a = reg.counter("k");
        let b = reg.counter("k");
        a.fetch_add(1, Ordering::Relaxed);
        assert_eq!(b.load(Ordering::Relaxed), 1);
        assert_eq!(reg.counter("other").load(Ordering::Relaxed), 0);
    }

    #[test]
    fn test_breaker_trips_and_recovers() {
        let mut s = BreakerState::default();
        assert!(s.allow());
        // Two unhealthy with threshold 2 → open.
        s.record_unhealthy(2, 3600, 3600);
        assert!(s.allow());
        s.record_unhealthy(2, 3600, 3600);
        assert!(!s.allow(), "breaker should be open after threshold");

        // A healthy response while closed resets the unhealthy streak.
        let mut s = BreakerState::default();
        s.record_unhealthy(3, 10, 100);
        s.record_healthy(1);
        s.record_unhealthy(3, 10, 100);
        assert!(s.allow(), "healthy response should have reset the streak");
    }

    #[test]
    fn test_breaker_backoff_grows() {
        let mut s = BreakerState::default();
        s.record_unhealthy(1, 2, 100); // trip 0 → 2s
        let first = s.open_until.unwrap();
        s.open_until = None; // simulate half-open
        s.record_unhealthy(1, 2, 100); // trip 1 → 4s
        let second = s.open_until.unwrap();
        assert!(second > first, "cooldown should grow across trips");
    }
}
