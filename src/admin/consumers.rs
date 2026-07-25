//! Admin API endpoints for consumer CRUD. Mutations rewrite the in-memory
//! gateway config and trigger `SharedState::reload`, which rebuilds and
//! atomically swaps the consumer store (no graph recompile is strictly
//! needed, but reload keeps the semantics uniform with routes/policies).

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router};

use crate::consumers::ConsumerConfig;
use crate::state::SharedState;

/// Builds the router for the `/api/consumers` endpoints.
pub fn router() -> Router<Arc<SharedState>> {
    Router::new()
        .route("/api/consumers", get(list_consumers).post(create_consumer))
        .route(
            "/api/consumers/{name}",
            get(get_consumer)
                .put(upsert_consumer)
                .delete(delete_consumer),
        )
}

/// `GET /api/consumers` — returns all declared consumers as a JSON array.
///
/// Note: responses include credentials verbatim — the admin API is the
/// management plane and is Basic-Auth protected.
async fn list_consumers(State(state): State<Arc<SharedState>>) -> impl IntoResponse {
    let gw = state.gateway.read().await;
    Json(&gw.consumers).into_response()
}

/// `GET /api/consumers/{name}` — returns the named consumer as JSON.
///
/// Errors: `404 Not Found` if no consumer with that name exists.
async fn get_consumer(
    State(state): State<Arc<SharedState>>,
    Path(name): Path<String>,
) -> impl IntoResponse {
    let gw = state.gateway.read().await;
    match gw.consumers.iter().find(|c| c.name == name) {
        Some(consumer) => Json(consumer).into_response(),
        None => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "not_found"})),
        )
            .into_response(),
    }
}

/// `POST /api/consumers` — creates a new consumer from the JSON body, then
/// rebuilds the consumer store. Returns `201 Created` on success.
///
/// Errors: `409 Conflict` if the name is taken; `400 Bad Request` if the
/// store rebuild rejects the config (duplicate credentials, malformed
/// credential objects) — the previous store stays active.
async fn create_consumer(
    State(state): State<Arc<SharedState>>,
    Json(consumer): Json<ConsumerConfig>,
) -> impl IntoResponse {
    let candidate = {
        let gw = state.gateway.read().await;
        if gw.consumers.iter().any(|c| c.name == consumer.name) {
            return (
                StatusCode::CONFLICT,
                Json(serde_json::json!({"error": "consumer already exists"})),
            )
                .into_response();
        }
        let mut candidate = gw.clone();
        candidate.consumers.push(consumer);
        candidate
    };

    match state.config_store.clone().commit(&state, candidate).await {
        Ok(_) => (
            StatusCode::CREATED,
            Json(serde_json::json!({"status": "created"})),
        )
            .into_response(),
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": e})),
        )
            .into_response(),
    }
}

/// `PUT /api/consumers/{name}` — creates or replaces the named consumer (the
/// path name overrides any name in the body), matching the policies
/// endpoint's upsert semantics. Returns `{"status": "updated"}`.
///
/// Errors: `400 Bad Request` if the store rebuild rejects the config.
async fn upsert_consumer(
    State(state): State<Arc<SharedState>>,
    Path(name): Path<String>,
    Json(mut consumer): Json<ConsumerConfig>,
) -> impl IntoResponse {
    consumer.name = name.clone();
    let candidate = {
        let gw = state.gateway.read().await;
        let mut candidate = gw.clone();
        if let Some(existing) = candidate.consumers.iter_mut().find(|c| c.name == name) {
            *existing = consumer;
        } else {
            candidate.consumers.push(consumer);
        }
        candidate
    };

    match state.config_store.clone().commit(&state, candidate).await {
        Ok(_) => Json(serde_json::json!({"status": "updated"})).into_response(),
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": e})),
        )
            .into_response(),
    }
}

/// `DELETE /api/consumers/{name}` — removes the named consumer and rebuilds
/// the store. Returns `{"status": "deleted"}`.
///
/// Errors: `404 Not Found` if the consumer does not exist.
async fn delete_consumer(
    State(state): State<Arc<SharedState>>,
    Path(name): Path<String>,
) -> impl IntoResponse {
    let candidate = {
        let gw = state.gateway.read().await;
        let mut candidate = gw.clone();
        let before = candidate.consumers.len();
        candidate.consumers.retain(|c| c.name != name);
        if candidate.consumers.len() == before {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({"error": "not_found"})),
            )
                .into_response();
        }
        candidate
    };

    match state.config_store.clone().commit(&state, candidate).await {
        Ok(_) => Json(serde_json::json!({"status": "deleted"})).into_response(),
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": e})),
        )
            .into_response(),
    }
}
