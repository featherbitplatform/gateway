//! Finding and purging every backend that holds entries for a cache pair.
//!
//! Shared by all three triggers -- the Admin API, the MCP tool and the
//! `phase: purge` node -- so they cannot drift on what "purge `products`"
//! means.

use std::sync::Arc;

use crate::graph::CompiledGraph;
use crate::traffic::cache::{CacheError, ResponseCache};

/// One `proxy-cache` half's backend, as it described itself at compile time.
#[derive(Clone)]
pub struct CacheTarget {
    /// The pair id this half belongs to.
    pub id: String,
    pub backend: Arc<dyn ResponseCache>,
    /// `"local"` or `"redis"`, for the response and the metric label.
    pub backend_label: &'static str,
    /// The declared store's name for `redis`; empty for `local`.
    pub store: String,
}

/// What one backend reported after a purge.
#[derive(Debug, Clone, serde::Serialize)]
pub struct PurgeOutcome {
    #[serde(rename = "backend")]
    pub backend_label: &'static str,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub store: String,
    pub removed: u64,
}

/// Every distinct backend holding entries for `id`, across all compiled graphs.
///
/// Deduplicated by `(backend_label, store)`: every `policy: local` half in the
/// process shares one `LocalResponseCache`, and two policies can point at the
/// same redis store. Purging a shared backend once per half would repeat the
/// work and report a count that means nothing.
pub fn collect_targets(graphs: &[Arc<CompiledGraph>], id: &str) -> Vec<CacheTarget> {
    let mut out: Vec<CacheTarget> = Vec::new();
    for g in graphs {
        for t in g.cache_targets().iter().filter(|t| t.id == id) {
            let dup = out
                .iter()
                .any(|o| o.backend_label == t.backend_label && o.store == t.store);
            if !dup {
                out.push(t.clone());
            }
        }
    }
    out
}

/// Purges each target in turn.
///
/// On failure, returns what succeeded before it alongside the error, so the
/// caller can report both -- a half-completed purge is worth knowing about.
pub async fn purge_targets(
    targets: &[CacheTarget],
) -> Result<Vec<PurgeOutcome>, (Vec<PurgeOutcome>, CacheError)> {
    let mut done = Vec::with_capacity(targets.len());
    for t in targets {
        match t.backend.purge(&t.id).await {
            Ok(removed) => done.push(PurgeOutcome {
                backend_label: t.backend_label,
                store: t.store.clone(),
                removed,
            }),
            Err(e) => return Err((done, e)),
        }
    }
    Ok(done)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::compile_policy;
    use crate::plugins::resources::PluginResources;

    fn graph(json: serde_json::Value) -> Arc<CompiledGraph> {
        let mut value = json;
        if let serde_json::Value::Object(ref mut map) = value {
            map.entry("name").or_insert_with(|| serde_json::json!("p"));
        }
        let policy = serde_json::from_value(value).unwrap();
        Arc::new(compile_policy(&policy, PluginResources::empty()).unwrap())
    }

    fn local_pair_policy(cache_id: &str) -> serde_json::Value {
        serde_json::json!({
            "nodes": [
                { "id": "listener", "type": "listener", "config": {} },
                { "id": "look", "type": "proxy-cache",
                  "config": { "phase": "lookup", "id": cache_id, "policy": "local" } },
                { "id": "up", "type": "upstream",
                  "config": { "targets": [{ "host": "h", "port": 80 }] } },
                { "id": "keep", "type": "proxy-cache",
                  "config": { "phase": "store", "id": cache_id, "policy": "local" } },
                { "id": "client", "type": "client", "config": {} }
            ],
            "edges": [
                { "from": "listener.out", "to": "look.in" },
                { "from": "look.success", "to": "up.in" },
                { "from": "look.hit", "to": "client.in" },
                { "from": "up.success", "to": "keep.in" },
                { "from": "keep.success", "to": "client.in" },
                { "from": "keep.hit", "to": "client.in" }
            ]
        })
    }

    /// Both halves of a local pair, and two policies with the same pair,
    /// all share ONE LocalResponseCache -- so a purge must hit it once.
    /// Without deduplication the shared cache would be purged per half, and
    /// the removed count would be nonsense.
    #[tokio::test]
    async fn test_collect_targets_deduplicates_the_shared_local_cache() {
        let a = graph(local_pair_policy("products"));
        let b = graph(local_pair_policy("products"));
        let targets = collect_targets(&[a, b], "products");
        assert_eq!(
            targets.len(),
            1,
            "two policies, four halves, one local cache"
        );
        assert_eq!(targets[0].backend_label, "local");
    }

    /// An id no pair uses yields nothing -- which the API turns into a 404
    /// rather than a successful flush of nothing.
    #[tokio::test]
    async fn test_collect_targets_for_an_unknown_id_is_empty() {
        let g = graph(local_pair_policy("products"));
        assert!(collect_targets(&[g], "prodcuts").is_empty());
    }

    /// A backend that never answers -- every call fails. Stands in for an
    /// outage so the partial-failure branch is testable without a live store.
    struct BrokenCache;

    #[async_trait::async_trait]
    impl ResponseCache for BrokenCache {
        async fn get(
            &self,
            _key: &str,
        ) -> Result<Option<crate::traffic::CachedResponse>, CacheError> {
            Err(CacheError("backend down".to_string()))
        }
        async fn put(
            &self,
            _key: &str,
            _entry: &crate::traffic::CachedResponse,
            _ttl: std::time::Duration,
        ) -> Result<(), CacheError> {
            Err(CacheError("backend down".to_string()))
        }
        async fn purge(&self, _id: &str) -> Result<u64, CacheError> {
            Err(CacheError("backend down".to_string()))
        }
    }

    /// A half-completed purge is worth knowing about: when a later target
    /// fails, `purge_targets` must return what succeeded before it, not just
    /// the error.
    #[tokio::test]
    async fn test_purge_targets_returns_what_succeeded_before_a_failure() {
        let local = Arc::new(crate::traffic::LocalResponseCache::default());
        local
            .put(
                "products\u{1}/x",
                &crate::traffic::CachedResponse {
                    status: 200,
                    headers: Default::default(),
                    body: bytes::Bytes::from_static(b"x"),
                },
                std::time::Duration::from_secs(60),
            )
            .await
            .unwrap();

        let targets = vec![
            CacheTarget {
                id: "products".to_string(),
                backend: local.clone(),
                backend_label: "local",
                store: String::new(),
            },
            CacheTarget {
                id: "products".to_string(),
                backend: Arc::new(BrokenCache),
                backend_label: "redis",
                store: "s".to_string(),
            },
        ];

        let Err((done, e)) = purge_targets(&targets).await else {
            panic!("a failing second target must return Err, not Ok");
        };

        assert_eq!(done.len(), 1, "the first target's success must be reported");
        assert_eq!(done[0].removed, 1);
        assert!(e.to_string().contains("backend down"), "{e}");
    }

    /// The whole point: a purge through the collected target removes what
    /// the pair cached.
    #[tokio::test]
    async fn test_purge_targets_removes_the_pairs_entries() {
        let g = graph(local_pair_policy("products"));
        let targets = collect_targets(&[g], "products");
        let cache = targets[0].backend.clone();
        cache
            .put(
                "products\u{1}/x",
                &crate::traffic::CachedResponse {
                    status: 200,
                    headers: Default::default(),
                    body: bytes::Bytes::from_static(b"x"),
                },
                std::time::Duration::from_secs(60),
            )
            .await
            .unwrap();

        let outcomes = purge_targets(&targets).await.unwrap();

        assert_eq!(outcomes.len(), 1);
        assert_eq!(outcomes[0].removed, 1);
        assert!(cache.get("products\u{1}/x").await.unwrap().is_none());
    }
}
