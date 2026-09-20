//! `purge_cache` -- the Admin API's DELETE /api/cache/{id}, for agents.

use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

use super::ToolError;
use crate::state::SharedState;
use crate::traffic::{collect_targets, purge_targets};

#[derive(Debug, Deserialize, JsonSchema)]
pub struct PurgeCacheArgs {
    /// The `proxy-cache` pair id (the `id` config key shared by its halves).
    pub id: String,
    /// List the backends a purge would hit without deleting anything. Default false.
    #[serde(default)]
    pub dry_run: bool,
}

/// Purges every backend holding entries for `id`, or -- with `dry_run` --
/// just lists them. `404` (`not_found`) when no compiled policy has a pair
/// with that id, exactly like the Admin API's DELETE. On a backend failure,
/// reports what was purged before it as a `cache_purge_failed` error.
pub async fn purge_cache(state: &SharedState, a: PurgeCacheArgs) -> Result<Value, ToolError> {
    let graphs: Vec<_> = state
        .routes
        .read()
        .await
        .iter()
        .map(|(_, g)| g.clone())
        .collect();
    let targets = collect_targets(&graphs, &a.id);
    if targets.is_empty() {
        return Err(ToolError::not_found("proxy-cache pair", &a.id));
    }

    if a.dry_run {
        let would: Vec<Value> = targets
            .iter()
            .map(|t| {
                let mut v = serde_json::json!({ "backend": t.backend_label });
                if !t.store.is_empty() {
                    v["store"] = Value::String(t.store.clone());
                }
                v
            })
            .collect();
        return Ok(serde_json::json!({ "id": a.id, "dry_run": true, "purged": would }));
    }

    match purge_targets(&targets).await {
        Ok(purged) => Ok(serde_json::json!({ "id": a.id, "purged": purged })),
        Err((purged, e)) => {
            let mut err = ToolError::new(
                "cache_purge_failed",
                format!("purging pair '{}' failed on a backend", a.id),
            );
            err.errors = vec![e.to_string()];
            err.hint = Some(format!(
                "{} backend(s) were purged before the failure",
                purged.len()
            ));
            Err(err)
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crate::mcp::tools::call;
    use crate::mcp::tools::test_support::{obj, state};
    use crate::state::SharedState;

    const LOCAL_PAIR_GATEWAY: &str = r#"
routes:
  - name: products
    match: { path: /products }
    policy: products-policy
policies:
  - name: products-policy
    nodes:
      - { id: listener, type: listener, config: {} }
      - { id: look, type: proxy-cache, config: { phase: lookup, id: products, policy: local } }
      - { id: up, type: upstream, config: { targets: [{ host: h, port: 80 }] } }
      - { id: keep, type: proxy-cache, config: { phase: store, id: products, policy: local } }
      - { id: client, type: client, config: {} }
    edges:
      - { from: listener.out, to: look.in }
      - { from: look.success, to: up.in }
      - { from: look.hit, to: client.in }
      - { from: up.success, to: keep.in }
      - { from: keep.success, to: client.in }
      - { from: keep.hit, to: client.in }
"#;

    async fn state_with_local_cache_pair() -> Arc<SharedState> {
        state("{}", LOCAL_PAIR_GATEWAY)
    }

    /// The backend the compiled pair actually uses -- found the same way the
    /// tool finds it, so the test seeds exactly where the purge will look.
    async fn backend(s: &SharedState) -> Arc<dyn crate::traffic::ResponseCache> {
        let graphs: Vec<_> = s
            .routes
            .read()
            .await
            .iter()
            .map(|(_, g)| g.clone())
            .collect();
        crate::traffic::collect_targets(&graphs, "products")
            .remove(0)
            .backend
    }

    async fn seed(s: &SharedState, key: &str) {
        let entry = crate::traffic::CachedResponse {
            status: 200,
            headers: Default::default(),
            body: bytes::Bytes::from_static(b"x"),
        };
        backend(s)
            .await
            .put(key, &entry, std::time::Duration::from_secs(60))
            .await
            .unwrap();
    }

    async fn still_cached(s: &SharedState, key: &str) -> bool {
        backend(s).await.get(key).await.unwrap().is_some()
    }

    /// dry_run shows what a purge would hit and removes nothing -- an agent
    /// should be able to see the blast radius before committing to it.
    #[tokio::test]
    async fn purge_cache_dry_run_lists_targets_and_removes_nothing() {
        let s = state_with_local_cache_pair().await;
        seed(&s, "products\u{1}/x").await;

        let v = call(
            &s,
            "purge_cache",
            obj(serde_json::json!({ "id": "products", "dry_run": true })),
        )
        .await
        .unwrap();

        assert_eq!(v["purged"].as_array().unwrap().len(), 1);
        assert!(
            v["purged"][0].get("removed").is_none(),
            "dry_run must not report a count: {v}"
        );
        assert!(
            still_cached(&s, "products\u{1}/x").await,
            "dry_run must not delete"
        );
    }

    #[tokio::test]
    async fn purge_cache_removes_and_counts() {
        let s = state_with_local_cache_pair().await;
        seed(&s, "products\u{1}/x").await;

        let v = call(
            &s,
            "purge_cache",
            obj(serde_json::json!({ "id": "products" })),
        )
        .await
        .unwrap();

        assert_eq!(v["purged"][0]["removed"], 1);
        assert!(!still_cached(&s, "products\u{1}/x").await);
    }

    /// The same 404 the Admin API gives: a typo is not a successful flush.
    #[tokio::test]
    async fn purge_cache_unknown_id_is_not_found() {
        let s = state_with_local_cache_pair().await;
        let err = call(
            &s,
            "purge_cache",
            obj(serde_json::json!({ "id": "prodcuts" })),
        )
        .await
        .unwrap_err();
        assert_eq!(err.code, "not_found");
        assert!(err.message.contains("prodcuts"), "{}", err.message);
    }
}
