//! Admin API endpoints for plugin config CRUD. Mutations rewrite the in-memory
//! gateway config and trigger validation + recompilation of every policy
//! (plugin configs are resolved at compile time), so a breaking edit or a
//! delete-while-referenced is rejected with 400 before anything changes.

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router};

use crate::config::PluginConfigDef;
use crate::state::SharedState;

/// Builds the router for `/api/plugin-configs`.
pub fn router() -> Router<Arc<SharedState>> {
    Router::new()
        .route("/api/plugin-configs", get(list_plugin_configs))
        .route(
            "/api/plugin-configs/{name}",
            get(get_plugin_config)
                .put(update_plugin_config)
                .delete(delete_plugin_config),
        )
}

/// `GET /api/plugin-configs` — returns all plugin config definitions as a JSON array.
async fn list_plugin_configs(State(state): State<Arc<SharedState>>) -> impl IntoResponse {
    let gw = state.gateway.read().await;
    Json(&gw.plugin_configs).into_response()
}

/// `GET /api/plugin-configs/{name}` — returns the named definition as JSON.
///
/// Errors: `404 Not Found` if no plugin config with that name exists.
async fn get_plugin_config(
    State(state): State<Arc<SharedState>>,
    Path(name): Path<String>,
) -> impl IntoResponse {
    let gw = state.gateway.read().await;
    match gw.plugin_configs.iter().find(|p| p.name == name) {
        Some(pc) => Json(pc).into_response(),
        None => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "not_found"})),
        )
            .into_response(),
    }
}

/// `PUT /api/plugin-configs/{name}` — upserts a definition from the JSON body
/// (the path name overrides any name in the body), then revalidates and
/// recompiles all route graphs — every policy using this plugin config picks up
/// the change atomically. Returns `{"status": "updated"}` on success.
///
/// Errors: `400 Bad Request` if the definition is invalid or any consuming
/// policy stops compiling (the previous compiled routes stay active).
async fn update_plugin_config(
    State(state): State<Arc<SharedState>>,
    Path(name): Path<String>,
    Json(mut pc): Json<PluginConfigDef>,
) -> impl IntoResponse {
    pc.name = name.clone();
    let candidate = {
        let gw = state.gateway.read().await;
        let mut candidate = gw.clone();
        if let Some(existing) = candidate.plugin_configs.iter_mut().find(|p| p.name == name) {
            *existing = pc;
        } else {
            candidate.plugin_configs.push(pc);
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

/// `DELETE /api/plugin-configs/{name}` — removes the named definition, then
/// revalidates and recompiles. Returns `{"status": "deleted"}` on success.
///
/// Errors: `404 Not Found` if it does not exist; `400 Bad Request` if a
/// policy still references it (recompilation fails, nothing changes).
async fn delete_plugin_config(
    State(state): State<Arc<SharedState>>,
    Path(name): Path<String>,
) -> impl IntoResponse {
    let candidate = {
        let gw = state.gateway.read().await;
        let mut candidate = gw.clone();
        let before = candidate.plugin_configs.len();
        candidate.plugin_configs.retain(|p| p.name != name);
        if candidate.plugin_configs.len() == before {
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
        let system: SystemConfig = serde_yaml::from_str("{}").unwrap();
        let gateway: GatewayConfig = serde_yaml::from_str(gateway_yaml).unwrap();
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

    fn app(state: Arc<SharedState>) -> Router {
        router().with_state(state)
    }

    const VALID_DEF: &str = r#"{
        "name": "shared-mock",
        "type": "mocking",
        "config": { "response_status": 200, "response_example": "hi", "content_type": "text/plain" }
    }"#;

    async fn put_def(state: &Arc<SharedState>, name: &str, body: &str) -> StatusCode {
        app(state.clone())
            .oneshot(
                Request::put(format!("/api/plugin-configs/{name}"))
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
            put_def(&state, "shared-mock", VALID_DEF).await,
            StatusCode::OK
        );

        let resp = app(state.clone())
            .oneshot(
                Request::get("/api/plugin-configs/shared-mock")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let resp = app(state.clone())
            .oneshot(
                Request::delete("/api/plugin-configs/shared-mock")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let resp = app(state)
            .oneshot(
                Request::get("/api/plugin-configs/shared-mock")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn test_put_unknown_type_is_400() {
        let state = test_state("{}");
        let bad = r#"{ "name": "x", "type": "openid-conect", "config": {} }"#;
        assert_eq!(put_def(&state, "x", bad).await, StatusCode::BAD_REQUEST);
    }

    /// Referenced from a SUPERNODE DEFINITION (the harder case): delete must 400.
    #[tokio::test]
    async fn test_delete_while_referenced_by_supernode_is_400() {
        let state = test_state(
            r#"
plugin_configs:
  - name: shared-mock
    type: mocking
    config: { response_status: 200, response_example: "hi", content_type: "text/plain" }
supernodes:
  - name: wrapped
    nodes:
      - { id: input,  type: input }
      - { id: output, type: output }
      - { id: error,  type: error }
      - { id: mock, type: mocking, config_ref: shared-mock }
    edges:
      - { from: input.out,    to: mock.in }
      - { from: mock.success, to: output.in }
"#,
        );
        let resp = app(state)
            .oneshot(
                Request::delete("/api/plugin-configs/shared-mock")
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
                Request::delete("/api/plugin-configs/nope")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }
}
