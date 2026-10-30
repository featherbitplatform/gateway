//! `DELETE /api/cache/{id}` -- purge everything a `proxy-cache` pair cached.
//!
//! A runtime action, like `DELETE /api/debug/traces` and
//! `POST /api/stores/{name}/ping`: it changes no configuration.

use std::sync::Arc;

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
    Json,
};
use utoipa::ToSchema;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use crate::state::SharedState;
use crate::traffic::{collect_targets, purge_targets, PurgeOutcome};

/// Builds the router for the `/api/cache` endpoints.
pub fn router() -> OpenApiRouter<Arc<SharedState>> {
    OpenApiRouter::new().routes(routes!(purge_cache))
}

/// Successful response body: the purged id and what each backend reported.
#[derive(serde::Serialize, ToSchema)]
struct CachePurgeResult {
    /// The purged `proxy-cache` pair id.
    id: String,
    /// One entry per distinct backend holding the pair.
    purged: Vec<PurgeOutcome>,
}

/// Error body of `DELETE /api/cache/{id}`.
#[derive(serde::Serialize, ToSchema)]
struct CachePurgeError {
    /// `not_found` or `cache_purge_failed`.
    error: String,
    /// Human-readable detail.
    message: String,
    /// The pair id (`cache_purge_failed` only).
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<String>,
    /// What the backends purged before the failure (`cache_purge_failed`
    /// only).
    #[serde(skip_serializing_if = "Option::is_none")]
    purged: Option<Vec<PurgeOutcome>>,
}

/// `DELETE /api/cache/{id}` — purges the pair `id` on every backend that
/// holds it.
///
/// `404` when no compiled policy has a pair with that id: a typo must not
/// read as a successful flush of nothing. `502` when a backend could not be
/// reached, naming it and listing what did succeed first.
#[utoipa::path(
    delete,
    path = "/api/cache/{id}",
    tag = "cache",
    summary = "Purge a proxy-cache pair",
    params(("id" = String, Path, description = "The `id` shared by a `proxy-cache` lookup/store pair.")),
    responses(
        (status = 200, description = "Purged; what each backend removed.", body = CachePurgeResult),
        (status = 404, description = "No compiled policy has a pair with that id (`error: not_found`).", body = CachePurgeError),
        (status = 502, description = "A backend could not be reached (`error: cache_purge_failed`); `purged` lists what succeeded first.", body = CachePurgeError),
    ),
)]
async fn purge_cache(
    State(state): State<Arc<SharedState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let graphs: Vec<_> = state
        .routes
        .read()
        .await
        .iter()
        .map(|(_, g)| g.clone())
        .collect();
    let targets = collect_targets(&graphs, &id);

    if targets.is_empty() {
        return (
            StatusCode::NOT_FOUND,
            Json(CachePurgeError {
                error: "not_found".to_string(),
                message: format!("no proxy-cache pair with id '{id}' in any policy"),
                id: None,
                purged: None,
            }),
        )
            .into_response();
    }

    match purge_targets(&targets).await {
        Ok(purged) => Json(CachePurgeResult { id, purged }).into_response(),
        Err((purged, e)) => (
            StatusCode::BAD_GATEWAY,
            Json(CachePurgeError {
                error: "cache_purge_failed".to_string(),
                message: e.to_string(),
                id: Some(id),
                purged: Some(purged),
            }),
        )
            .into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::tools::test_support::state;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    /// The same `local` pair fixture the MCP `purge_cache` tests use, so
    /// this handler and that tool are proven to find the same backend.
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

    fn app(s: Arc<SharedState>) -> axum::Router {
        axum::Router::from(router()).with_state(s)
    }

    async fn send(state: &Arc<SharedState>, req: Request<Body>) -> (StatusCode, serde_json::Value) {
        let resp = app(state.clone()).oneshot(req).await.unwrap();
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let v = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
        (status, v)
    }

    /// A typo'd id must not read as a successful flush of nothing.
    #[tokio::test]
    async fn test_purge_unknown_id_is_404_not_found() {
        let s = state("{}", LOCAL_PAIR_GATEWAY);

        let (status, body) = send(
            &s,
            Request::delete("/api/cache/no-such-pair")
                .body(Body::empty())
                .unwrap(),
        )
        .await;

        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(body["error"], "not_found");
    }

    /// Seeding directly into the backend the compiled pair actually uses
    /// (found the same way the handler finds it), then purging by id,
    /// reports exactly what was removed on the `local` backend and omits
    /// `store` -- there is no named store for a `local` entry to report.
    #[tokio::test]
    async fn test_purge_known_id_removes_and_reports_local_backend() {
        let s = state("{}", LOCAL_PAIR_GATEWAY);
        let graphs: Vec<_> = s
            .routes
            .read()
            .await
            .iter()
            .map(|(_, g)| g.clone())
            .collect();
        let backend = crate::traffic::collect_targets(&graphs, "products")
            .remove(0)
            .backend;
        backend
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

        let (status, body) = send(
            &s,
            Request::delete("/api/cache/products")
                .body(Body::empty())
                .unwrap(),
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["purged"][0]["removed"], 1);
        assert_eq!(body["purged"][0]["backend"], "local");
        assert!(
            body["purged"][0].get("store").is_none(),
            "a local backend entry must not carry a store key: {body}"
        );
    }
}
