//! `DELETE /api/cache/{id}` -- purge everything a `proxy-cache` pair cached.
//!
//! A runtime action, like `DELETE /api/debug/traces` and
//! `POST /api/stores/{name}/ping`: it changes no configuration.

use std::sync::Arc;

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
    routing::delete,
    Json, Router,
};

use crate::state::SharedState;
use crate::traffic::{collect_targets, purge_targets, PurgeOutcome};

pub fn router() -> Router<Arc<SharedState>> {
    Router::new().route("/api/cache/{id}", delete(purge_cache))
}

/// Successful response body: the purged id and what each backend reported.
#[derive(serde::Serialize)]
struct PurgeResponse {
    id: String,
    purged: Vec<PurgeOutcome>,
}

/// Purges the pair `id` on every backend that holds it.
///
/// `404` when no compiled policy has a pair with that id: a typo must not
/// read as a successful flush of nothing. `502` when a backend could not be
/// reached, naming it and listing what did succeed first.
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
            Json(serde_json::json!({
                "error": "not_found",
                "message": format!("no proxy-cache pair with id '{id}' in any policy"),
            })),
        )
            .into_response();
    }

    match purge_targets(&targets).await {
        Ok(purged) => Json(PurgeResponse { id, purged }).into_response(),
        Err((purged, e)) => (
            StatusCode::BAD_GATEWAY,
            Json(serde_json::json!({
                "error": "cache_purge_failed",
                "message": e.to_string(),
                "id": id,
                "purged": purged,
            })),
        )
            .into_response(),
    }
}
