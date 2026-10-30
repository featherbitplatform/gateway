//! Admin API endpoints for supernode CRUD. Mutations rewrite the in-memory
//! gateway config and trigger validation + recompilation of every policy
//! (supernodes are inlined at compile time), so a breaking edit or a
//! delete-while-referenced is rejected with 400 before anything changes.

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use super::openapi::{ErrorResponse, StatusResponse};
use crate::config::SupernodeConfig;
use crate::state::SharedState;

/// Builds the router for `/api/supernodes`.
pub fn router() -> OpenApiRouter<Arc<SharedState>> {
    OpenApiRouter::new()
        .routes(routes!(list_supernodes))
        .routes(routes!(get_supernode, update_supernode, delete_supernode))
}

/// `GET /api/supernodes` — returns all supernode definitions as a JSON array.
#[utoipa::path(
    get,
    path = "/api/supernodes",
    tag = "supernodes",
    summary = "List supernodes",
    responses((status = 200, description = "All supernode definitions.", body = Vec<SupernodeConfig>)),
)]
async fn list_supernodes(State(state): State<Arc<SharedState>>) -> impl IntoResponse {
    let gw = state.gateway.read().await;
    Json(&gw.supernodes).into_response()
}

/// `GET /api/supernodes/{name}` — returns the named definition as JSON.
///
/// Errors: `404 Not Found` if no supernode with that name exists.
#[utoipa::path(
    get,
    path = "/api/supernodes/{name}",
    tag = "supernodes",
    summary = "Get a supernode",
    params(("name" = String, Path, description = "Supernode name.")),
    responses(
        (status = 200, description = "The supernode definition.", body = SupernodeConfig),
        (status = 404, description = "No such supernode.", body = ErrorResponse),
    ),
)]
async fn get_supernode(
    State(state): State<Arc<SharedState>>,
    Path(name): Path<String>,
) -> impl IntoResponse {
    let gw = state.gateway.read().await;
    match gw.supernodes.iter().find(|s| s.name == name) {
        Some(sn) => Json(sn).into_response(),
        None => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "not_found"})),
        )
            .into_response(),
    }
}

/// `PUT /api/supernodes/{name}` — upserts a definition from the JSON body
/// (the path name overrides any name in the body), then revalidates and
/// recompiles all route graphs — every policy using this supernode picks up
/// the change atomically. Returns `{"status": "updated"}` on success.
///
/// Errors: `400 Bad Request` if the definition is invalid or any consuming
/// policy stops compiling (the previous compiled routes stay active).
#[utoipa::path(
    put,
    path = "/api/supernodes/{name}",
    tag = "supernodes",
    summary = "Create or replace a supernode",
    params(("name" = String, Path, description = "Supernode name; overrides any `name` in the body.")),
    request_body = SupernodeConfig,
    responses(
        (status = 200, description = "Created or replaced (`status: updated`).", body = StatusResponse),
        (status = 400, description = "The definition is invalid, a consuming policy stopped compiling, or the config store rejected the write.", body = ErrorResponse),
    ),
)]
async fn update_supernode(
    State(state): State<Arc<SharedState>>,
    Path(name): Path<String>,
    Json(mut sn): Json<SupernodeConfig>,
) -> impl IntoResponse {
    sn.name = name.clone();
    let candidate = {
        let gw = state.gateway.read().await;
        let mut candidate = gw.clone();
        if let Some(existing) = candidate.supernodes.iter_mut().find(|s| s.name == name) {
            *existing = sn;
        } else {
            candidate.supernodes.push(sn);
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

/// `DELETE /api/supernodes/{name}` — removes the named definition, then
/// revalidates and recompiles. Returns `{"status": "deleted"}` on success.
///
/// Errors: `404 Not Found` if it does not exist; `400 Bad Request` if a
/// policy still references it (recompilation fails, nothing changes).
#[utoipa::path(
    delete,
    path = "/api/supernodes/{name}",
    tag = "supernodes",
    summary = "Delete a supernode",
    params(("name" = String, Path, description = "Supernode name.")),
    responses(
        (status = 200, description = "Deleted (`status: deleted`).", body = StatusResponse),
        (status = 400, description = "A policy still references the supernode, or the config store rejected the write.", body = ErrorResponse),
        (status = 404, description = "No such supernode.", body = ErrorResponse),
    ),
)]
async fn delete_supernode(
    State(state): State<Arc<SharedState>>,
    Path(name): Path<String>,
) -> impl IntoResponse {
    let candidate = {
        let gw = state.gateway.read().await;
        let mut candidate = gw.clone();
        let before = candidate.supernodes.len();
        candidate.supernodes.retain(|s| s.name != name);
        if candidate.supernodes.len() == before {
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

    fn test_state(gateway_yaml: &str) -> Arc<SharedState> {
        let system: SystemConfig = crate::yaml::from_str("{}").unwrap();
        let gateway: GatewayConfig = crate::yaml::from_str(gateway_yaml).unwrap();
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

    fn app(state: Arc<SharedState>) -> axum::Router {
        axum::Router::from(router()).with_state(state)
    }

    const VALID_SN: &str = r#"{
        "name": "secured-call",
        "nodes": [
            { "id": "input",  "type": "input",  "config": {} },
            { "id": "output", "type": "output", "config": {} },
            { "id": "error",  "type": "error",  "config": {} },
            { "id": "up", "type": "upstream", "config": { "targets": [ { "host": "127.0.0.1", "port": 9 } ] } }
        ],
        "edges": [
            { "from": "input.out",  "to": "up.in" },
            { "from": "up.success", "to": "output.in" }
        ]
    }"#;

    async fn put_supernode(state: &Arc<SharedState>, name: &str, body: &str) -> StatusCode {
        app(state.clone())
            .oneshot(
                Request::put(format!("/api/supernodes/{name}"))
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap()
            .status()
    }

    #[tokio::test]
    async fn test_put_list_get_delete_roundtrip() {
        let state = test_state("{}");
        assert_eq!(
            put_supernode(&state, "secured-call", VALID_SN).await,
            StatusCode::OK
        );

        let resp = app(state.clone())
            .oneshot(
                Request::get("/api/supernodes/secured-call")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let resp = app(state.clone())
            .oneshot(
                Request::delete("/api/supernodes/secured-call")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let resp = app(state)
            .oneshot(
                Request::get("/api/supernodes/secured-call")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn test_put_invalid_definition_is_400() {
        let state = test_state("{}");
        // Missing boundary nodes entirely.
        let bad = r#"{ "name": "x", "nodes": [], "edges": [] }"#;
        assert_eq!(
            put_supernode(&state, "x", bad).await,
            StatusCode::BAD_REQUEST
        );
    }

    #[tokio::test]
    async fn test_delete_while_referenced_is_400() {
        let state = test_state("{}");
        assert_eq!(
            put_supernode(&state, "secured-call", VALID_SN).await,
            StatusCode::OK
        );

        // Reference it from a policy by committing a candidate directly.
        let candidate = {
            let gw = state.gateway.read().await;
            let mut c = gw.clone();
            c.policies.push(
                crate::yaml::from_str(
                    r#"
name: p
nodes:
  - { id: listener, type: listener }
  - { id: sec, type: supernode, config: { name: secured-call } }
  - { id: client, type: client }
edges:
  - { from: listener.out, to: sec.in }
  - { from: sec.success, to: client.in }
"#,
                )
                .unwrap(),
            );
            c
        };
        state
            .config_store
            .clone()
            .commit(&state, candidate)
            .await
            .unwrap();

        let resp = app(state.clone())
            .oneshot(
                Request::delete("/api/supernodes/secured-call")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn test_delete_missing_is_404() {
        let state = test_state("{}");
        let resp = app(state)
            .oneshot(
                Request::delete("/api/supernodes/nope")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }
}
