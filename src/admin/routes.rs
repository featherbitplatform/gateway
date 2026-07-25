//! Admin API endpoints for route CRUD. Mutations rewrite the in-memory
//! gateway config and trigger validation + graph recompilation via
//! `SharedState::reload`.

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router};

use crate::config::RouteConfig;
use crate::state::SharedState;

/// Builds the router for the `/api/routes` endpoints.
pub fn router() -> Router<Arc<SharedState>> {
    Router::new()
        .route("/api/routes", get(list_routes).post(create_route))
        .route(
            "/api/routes/{name}",
            get(get_route).put(update_route).delete(delete_route),
        )
}

/// `GET /api/routes` — returns all configured routes as a JSON array.
async fn list_routes(State(state): State<Arc<SharedState>>) -> impl IntoResponse {
    let gw = state.gateway.read().await;
    Json(&gw.routes).into_response()
}

/// `GET /api/routes/{name}` — returns the named route as JSON.
///
/// Errors: `404 Not Found` if no route with that name exists.
async fn get_route(
    State(state): State<Arc<SharedState>>,
    Path(name): Path<String>,
) -> impl IntoResponse {
    let gw = state.gateway.read().await;
    match gw.routes.iter().find(|r| r.name == name) {
        Some(route) => Json(route).into_response(),
        None => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "not_found"})),
        )
            .into_response(),
    }
}

/// `POST /api/routes` — creates a new route from the JSON body, then
/// revalidates and recompiles all route graphs. Returns `201 Created` with
/// `{"status": "created"}` on success.
///
/// Errors: `409 Conflict` if a route with the same name already exists;
/// `400 Bad Request` if the resulting configuration fails
/// validation/recompilation (the previous compiled routes stay active).
async fn create_route(
    State(state): State<Arc<SharedState>>,
    Json(route): Json<RouteConfig>,
) -> impl IntoResponse {
    let candidate = {
        let gw = state.gateway.read().await;
        if gw.routes.iter().any(|r| r.name == route.name) {
            return (
                StatusCode::CONFLICT,
                Json(serde_json::json!({"error": "route already exists"})),
            )
                .into_response();
        }
        let mut candidate = gw.clone();
        candidate.routes.push(route);
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

/// `PUT /api/routes/{name}` — replaces the named route with the JSON body
/// (the path name overrides any name in the body), then revalidates and
/// recompiles. Returns `{"status": "updated"}` on success.
///
/// Errors: `404 Not Found` if the route does not exist (unlike policies,
/// routes are not upserted); `400 Bad Request` if recompilation fails.
async fn update_route(
    State(state): State<Arc<SharedState>>,
    Path(name): Path<String>,
    Json(mut route): Json<RouteConfig>,
) -> impl IntoResponse {
    route.name = name.clone();
    let candidate = {
        let gw = state.gateway.read().await;
        let mut candidate = gw.clone();
        if let Some(existing) = candidate.routes.iter_mut().find(|r| r.name == name) {
            *existing = route;
        } else {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({"error": "not_found"})),
            )
                .into_response();
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

/// `DELETE /api/routes/{name}` — removes the named route, then revalidates
/// and recompiles. Returns `{"status": "deleted"}` on success.
///
/// Errors: `404 Not Found` if the route does not exist; `400 Bad Request`
/// if recompilation fails.
async fn delete_route(
    State(state): State<Arc<SharedState>>,
    Path(name): Path<String>,
) -> impl IntoResponse {
    let candidate = {
        let gw = state.gateway.read().await;
        let mut candidate = gw.clone();
        let before = candidate.routes.len();
        candidate.routes.retain(|r| r.name != name);
        if candidate.routes.len() == before {
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
