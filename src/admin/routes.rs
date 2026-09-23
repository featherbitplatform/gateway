//! Admin API endpoints for route CRUD. Mutations rewrite the in-memory
//! gateway config and trigger validation + graph recompilation via
//! `SharedState::reload`.

use std::collections::HashSet;
use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;

use crate::config::RouteConfig;
use crate::state::SharedState;

/// Builds the router for the `/api/routes` endpoints.
pub fn router() -> Router<Arc<SharedState>> {
    Router::new()
        .route(
            "/api/routes",
            get(list_routes).post(create_route).put(reorder_routes),
        )
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

/// Body of `PUT /api/routes`: every existing route name, exactly once, in the
/// new evaluation order.
#[derive(Debug, Deserialize)]
struct RouteOrder {
    order: Vec<String>,
}

/// Reorders `routes` to follow `order`, which must name every route exactly
/// once. Routes are matched in declaration order (first match wins), so this
/// is how a route's priority changes. Shared with the MCP `put_route_order`
/// tool. On error `routes` is left untouched.
pub(crate) fn apply_route_order(
    routes: &mut Vec<RouteConfig>,
    order: &[String],
) -> Result<(), String> {
    let mut seen = HashSet::new();
    for name in order {
        if !seen.insert(name.as_str()) {
            return Err(format!("route '{name}' is listed more than once"));
        }
        if !routes.iter().any(|r| &r.name == name) {
            return Err(format!("route '{name}' does not exist"));
        }
    }
    if let Some(missing) = routes.iter().find(|r| !seen.contains(r.name.as_str())) {
        return Err(format!(
            "route '{}' is missing from the order; list every route exactly once",
            missing.name
        ));
    }
    let mut remaining = std::mem::take(routes);
    for name in order {
        let idx = remaining
            .iter()
            .position(|r| &r.name == name)
            .expect("checked above");
        routes.push(remaining.swap_remove(idx));
    }
    Ok(())
}

/// `PUT /api/routes` — reorders the routes (their match priority) to the
/// `{"order": [names...]}` body, then revalidates and recompiles. Returns
/// `{"status": "reordered"}`.
///
/// Errors: `400 Bad Request` if `order` is not a permutation of the existing
/// route names, or if recompilation fails.
async fn reorder_routes(
    State(state): State<Arc<SharedState>>,
    Json(body): Json<RouteOrder>,
) -> impl IntoResponse {
    let candidate = {
        let gw = state.gateway.read().await;
        let mut candidate = gw.clone();
        if let Err(e) = apply_route_order(&mut candidate.routes, &body.order) {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": e})),
            )
                .into_response();
        }
        candidate
    };

    match state.config_store.clone().commit(&state, candidate).await {
        Ok(_) => Json(serde_json::json!({"status": "reordered"})).into_response(),
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": e})),
        )
            .into_response(),
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{GatewayConfig, SystemConfig};
    use crate::config_store::FileConfigStore;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    const GATEWAY: &str = r#"
routes:
  - { name: a, match: { path: /a }, policy: p }
  - { name: b, match: { path: /b }, policy: p }
  - { name: c, match: { path: /c }, policy: p }
policies:
  - name: p
    nodes:
      - { id: l, type: listener }
      - { id: e, type: echo, config: { body: hi } }
      - { id: c, type: client }
    edges:
      - { from: l.out, to: e.in }
      - { from: e.out, to: c.in }
"#;

    fn test_state() -> Arc<SharedState> {
        let system: SystemConfig = serde_yaml::from_str("{}").unwrap();
        let gateway: GatewayConfig = serde_yaml::from_str(GATEWAY).unwrap();
        Arc::new(
            SharedState::new(
                system,
                gateway,
                None,
                Arc::new(FileConfigStore::new(std::path::PathBuf::from(
                    "gateway.yaml",
                ))),
            )
            .unwrap(),
        )
    }

    fn names(routes: &[RouteConfig]) -> Vec<&str> {
        routes.iter().map(|r| r.name.as_str()).collect()
    }

    async fn put_order(state: &Arc<SharedState>, body: &str) -> StatusCode {
        router()
            .with_state(state.clone())
            .oneshot(
                Request::put("/api/routes")
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap()
            .status()
    }

    #[test]
    fn test_apply_route_order_rejects_non_permutations() {
        let gw: GatewayConfig = serde_yaml::from_str(GATEWAY).unwrap();
        let o = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        for bad in [
            o(&["a", "b"]),
            o(&["a", "b", "c", "a"]),
            o(&["a", "b", "x"]),
        ] {
            let mut routes = gw.routes.clone();
            assert!(apply_route_order(&mut routes, &bad).is_err(), "{bad:?}");
            assert_eq!(names(&routes), ["a", "b", "c"], "left untouched on error");
        }
        let mut routes = gw.routes.clone();
        apply_route_order(&mut routes, &o(&["c", "a", "b"])).unwrap();
        assert_eq!(names(&routes), ["c", "a", "b"]);
    }

    #[tokio::test]
    async fn test_put_routes_reorders_live_config() {
        let state = test_state();
        assert_eq!(
            put_order(&state, r#"{"order": ["c", "a", "b"]}"#).await,
            StatusCode::OK
        );
        assert_eq!(names(&state.gateway.read().await.routes), ["c", "a", "b"]);

        assert_eq!(
            put_order(&state, r#"{"order": ["a"]}"#).await,
            StatusCode::BAD_REQUEST
        );
        assert_eq!(names(&state.gateway.read().await.routes), ["c", "a", "b"]);
    }
}
