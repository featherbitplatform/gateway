//! Response-cache backends for the `proxy-cache` node.
//!
//! One trait, two implementations: a process-local map and (from the redis
//! backend onwards) a shared store. `proxy-cache` holds whichever it was
//! configured with and never learns which.

use std::collections::HashMap;
use std::sync::Arc;
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

/// The key prefix shared by every entry a `proxy-cache` pair writes.
///
/// `proxy-cache` derives keys as `{id}\u{1}{component}\u{1}…`, so this prefix
/// selects a pair exactly: `products\u{1}` matches nothing of `products-v2`.
pub(crate) fn pair_prefix(id: &str) -> String {
    format!("{id}\u{1}")
}

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

    /// Removes every entry belonging to the pair `id` and returns how many.
    ///
    /// "Belonging to" means the key begins with [`pair_prefix`]. `Ok(0)` is a
    /// normal answer: the pair had nothing cached.
    async fn purge(&self, id: &str) -> Result<u64, CacheError>;
}

/// Entries the local cache keeps before it starts evicting.
const DEFAULT_MAX_ENTRIES: usize = 10_000;

/// Fraction of capacity a sweep reclaims in one pass.
///
/// Evicting down to a low-water mark is what keeps the O(n) scan off the hot
/// path: a saturated cache would otherwise pay a full scan on every write,
/// forever. Reclaiming a tenth means the next ~capacity/10 inserts find room
/// already waiting and return immediately.
const RECLAIM_FRACTION: usize = 10;

/// Process-local cache. Shared by every `policy: local` node in the gateway.
pub struct LocalResponseCache {
    entries: DashMap<String, (CachedResponse, Instant)>,
    /// Read on every insert; set once at startup from `cache.max_entries`.
    capacity: std::sync::atomic::AtomicUsize,
    /// Set once at construction (`PluginResources::new`); `None` disables
    /// recording (unit tests). Used only to count evictions — hits, misses
    /// and errors are metered by the `proxy-cache` plugin, which knows the
    /// `store` label this backend does not have.
    metrics: Option<Arc<crate::metrics::GatewayMetrics>>,
}

impl Default for LocalResponseCache {
    fn default() -> Self {
        Self::new(None)
    }
}

impl LocalResponseCache {
    /// Creates the process-local cache, optionally wired to the metrics
    /// registry so `make_room` can count evictions.
    pub fn new(metrics: Option<Arc<crate::metrics::GatewayMetrics>>) -> Self {
        Self {
            entries: DashMap::new(),
            capacity: std::sync::atomic::AtomicUsize::new(DEFAULT_MAX_ENTRIES),
            metrics,
        }
    }

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
    /// not enough, the entries expiring soonest go next — deliberately not an
    /// LRU (no LRU crate is in the dependency tree, and adding one for this is
    /// not worth the supply-chain surface). For a cache whose entries all
    /// carry TTLs this discards what was about to become useless anyway; the
    /// cost is that a hot short-TTL entry loses to a cold long-TTL one.
    ///
    /// The live-entry eviction reclaims down to a low-water mark
    /// (`capacity - capacity / RECLAIM_FRACTION`) in one sorted pass, rather
    /// than removing exactly one entry via a fresh per-entry `min_by_key`
    /// scan. Once the cache is saturated, a per-entry scan would make every
    /// `put` pay an O(n) cost for the life of the process; amortising the
    /// sort over a batch of evictions means most inserts, most of the time,
    /// find room already waiting and skip straight to the insert.
    fn make_room(&self, capacity: usize) {
        if self.entries.len() < capacity {
            return;
        }

        // Expired entries first: they are already worthless, and clearing them in
        // bulk is the one thing a per-entry eviction cannot do cheaply.
        let now = Instant::now();
        self.entries.retain(|_, (_, expires_at)| now < *expires_at);
        if self.entries.len() < capacity {
            return;
        }

        // Still full, so live entries have to go. Sort once and remove a batch,
        // rather than rescanning for the minimum per entry.
        let target = capacity.saturating_sub((capacity / RECLAIM_FRACTION).max(1));
        let excess = self.entries.len().saturating_sub(target);
        if excess == 0 {
            return;
        }

        let mut by_expiry: Vec<(String, Instant)> = self
            .entries
            .iter()
            .map(|e| (e.key().clone(), e.value().1))
            .collect();
        by_expiry.sort_unstable_by_key(|(_, expires_at)| *expires_at);
        // Counted separately from the expired sweep above: a live entry
        // being evicted here — not merely swept for already being dead — is
        // the signal that `max_entries` is too small for the working set.
        let mut evicted: u64 = 0;
        for (key, _) in by_expiry.into_iter().take(excess) {
            if self.entries.remove(&key).is_some() {
                evicted += 1;
            }
        }
        if evicted > 0 {
            if let Some(metrics) = &self.metrics {
                metrics
                    .cache_events
                    .with_label_values(&["local", "", "eviction"])
                    .inc_by(evicted);
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

    async fn purge(&self, id: &str) -> Result<u64, CacheError> {
        let prefix = pair_prefix(id);
        let mut removed = 0u64;
        self.entries.retain(|key, _| {
            if key.starts_with(&prefix) {
                removed += 1;
                false
            } else {
                true
            }
        });
        Ok(removed)
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
        // An already-expired entry always has the earliest `Instant` of any
        // entry in the map, so a single expired entry can't tell a two-phase
        // sweep (expire all, then batch-evict live ones) apart from a
        // one-phase "always evict the minimum" implementation: either would
        // remove it. Several expired entries do distinguish the two: a
        // per-entry min-eviction would stop after removing just one (as soon
        // as it's back under capacity), while the sweep clears all of them
        // in one pass.
        let cache = LocalResponseCache::default();
        cache.set_capacity(5);

        for i in 0..4 {
            cache
                .put(
                    &format!("dead{i}"),
                    &response("d"),
                    Duration::from_millis(1),
                )
                .await
                .unwrap();
        }
        cache
            .put("live", &response("l"), Duration::from_secs(60))
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;

        // At capacity: a single put must trigger the sweep and clear every
        // expired entry, not just one of them.
        cache
            .put("new", &response("n"), Duration::from_secs(60))
            .await
            .unwrap();

        for i in 0..4 {
            assert!(
                cache.get(&format!("dead{i}")).await.unwrap().is_none(),
                "all expired entries must be swept, not just one"
            );
        }
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

        // `long` is inserted first and `short` second, so insertion order and
        // expiry order disagree: an "evict oldest-inserted" implementation
        // would evict `long`, not `short`. Only a policy that actually looks
        // at expiry time evicts `short` here.
        cache
            .put("long", &response("l"), Duration::from_secs(600))
            .await
            .unwrap();
        cache
            .put("short", &response("s"), Duration::from_secs(1))
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

    /// Spec §8: an eviction driven by a full cache (not a merely-expired
    /// sweep) must be visible, since it is the only signal that `max_entries`
    /// is too small for the working set.
    #[tokio::test]
    async fn test_an_eviction_at_capacity_increments_the_eviction_counter() {
        let metrics = Arc::new(crate::metrics::GatewayMetrics::new());
        let cache = LocalResponseCache::new(Some(metrics.clone()));
        cache.set_capacity(2);

        // None of these expire during the test, so every entry beyond
        // capacity is evicted live, not swept as already-dead.
        cache
            .put("a", &response("a"), Duration::from_secs(600))
            .await
            .unwrap();
        cache
            .put("b", &response("b"), Duration::from_secs(600))
            .await
            .unwrap();
        cache
            .put("c", &response("c"), Duration::from_secs(600))
            .await
            .unwrap();

        assert!(
            metrics
                .cache_events
                .with_label_values(&["local", "", "eviction"])
                .get()
                >= 1,
            "a live-entry eviction at capacity must be counted"
        );
    }

    /// The prefix boundary is the whole safety argument: purging `products`
    /// must not touch `products-v2`, whose keys share every byte up to the
    /// separator.
    #[tokio::test]
    async fn test_local_purge_removes_only_the_named_pair() {
        let cache = LocalResponseCache::default();
        let ttl = Duration::from_secs(60);
        cache
            .put("products\u{1}/a", &response("a"), ttl)
            .await
            .unwrap();
        cache
            .put("products\u{1}/b", &response("b"), ttl)
            .await
            .unwrap();
        cache
            .put("products-v2\u{1}/a", &response("v2"), ttl)
            .await
            .unwrap();

        let removed = cache.purge("products").await.unwrap();

        assert_eq!(removed, 2);
        assert!(cache.get("products\u{1}/a").await.unwrap().is_none());
        assert!(cache.get("products\u{1}/b").await.unwrap().is_none());
        assert!(
            cache.get("products-v2\u{1}/a").await.unwrap().is_some(),
            "a sibling pair sharing a textual prefix must survive"
        );
    }

    /// The count must reflect exactly the entries removed, not a
    /// `before - after` delta taken around the retain: a concurrent `put`
    /// landing in a shard the shard-by-shard retain has not yet visited grows
    /// `len()` mid-purge, so a before/after diff undercounts (or, given
    /// enough concurrent insertions, underflows the `usize` subtraction
    /// outright, panicking in debug and wrapping to a huge garbage count in
    /// release). Counting inside the retain closure itself is immune: it only
    /// ever sees the entries it actually drops.
    #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
    async fn test_local_purge_counts_removed_entries_not_a_before_after_delta() {
        let cache = std::sync::Arc::new(LocalResponseCache::default());
        let ttl = Duration::from_secs(60);

        // A large filler dataset -- inserted directly, bypassing `put`'s
        // capacity bookkeeping, which is irrelevant here -- makes the
        // retain's shard-by-shard scan below take long enough in real wall
        // time for a concurrent writer to land insertions in shards it has
        // not yet visited: the exact window the old before/after diff got
        // wrong.
        for i in 0..300_000u64 {
            cache.entries.insert(
                format!("filler\u{1}/{i}"),
                (response("f"), Instant::now() + ttl),
            );
        }
        for i in 0..3 {
            cache.entries.insert(
                format!("products\u{1}/{i}"),
                (response("p"), Instant::now() + ttl),
            );
        }
        for i in 0..2 {
            cache.entries.insert(
                format!("other\u{1}/{i}"),
                (response("o"), Instant::now() + ttl),
            );
        }

        // A real OS thread, writing straight into the map (bypassing the
        // async `put` wrapper, which adds only overhead here), released at
        // the same instant as the purge below via a barrier so it genuinely
        // races the retain rather than merely interleaving at `.await`
        // points on a cooperative scheduler.
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let writer = {
            let cache = cache.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                let entry = (response("c"), Instant::now() + Duration::from_secs(600));
                for i in 0..100_000u64 {
                    cache
                        .entries
                        .insert(format!("concurrent\u{1}/{i}"), entry.clone());
                }
            })
        };

        barrier.wait();
        let removed = cache.purge("products").await.unwrap();
        writer.join().unwrap();

        assert_eq!(
            removed, 3,
            "must count exactly the 3 pair entries removed, regardless of \
             concurrent unrelated writes racing the purge"
        );
        assert!(cache.get("other\u{1}/0").await.unwrap().is_some());
        assert!(cache.get("other\u{1}/1").await.unwrap().is_some());
    }

    /// Purging a pair that cached nothing is a normal answer, not a failure.
    #[tokio::test]
    async fn test_local_purge_of_an_empty_pair_is_zero_not_an_error() {
        let cache = LocalResponseCache::default();
        assert_eq!(cache.purge("nothing-here").await.unwrap(), 0);
    }

    #[tokio::test]
    async fn test_local_never_exceeds_its_capacity() {
        // Sequential writes are close to structurally guaranteed to respect
        // the bound, since `make_room` runs before every insert. The
        // interesting case — and the one the assertion message below has
        // always claimed to cover — is concurrent writers racing `make_room`
        // and `insert` against each other.
        let cache = std::sync::Arc::new(LocalResponseCache::default());
        cache.set_capacity(4);

        let mut tasks = Vec::new();
        for task in 0..8 {
            let cache = cache.clone();
            tasks.push(tokio::spawn(async move {
                for i in 0..50 {
                    cache
                        .put(
                            &format!("k{task}-{i}"),
                            &response("x"),
                            Duration::from_secs(600),
                        )
                        .await
                        .unwrap();
                }
            }));
        }
        for task in tasks {
            task.await.unwrap();
        }

        assert!(
            cache.len() <= 4,
            "the bound must hold under sustained writes: {}",
            cache.len()
        );
    }
}
