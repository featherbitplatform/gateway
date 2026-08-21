//! Admin CRUD for the `stores:` resource (named redis/valkey connections).
//!
//! Responses always carry the RAW stored config — `${ENV}` placeholders are
//! never resolved here (they resolve only when a client is built). Deleting
//! a store still referenced by any plugin config is rejected with
//! `409 {"error":"in_use","referrers":[...]}` — a new envelope, since the
//! reference is not discoverable via graph recompilation (a store used only
//! by a shared plugin_config compiles fine without the node being wired).

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router};

use crate::config::{GatewayConfig, NodeConfig, StoreConfig};
use crate::state::SharedState;

pub fn router() -> Router<Arc<SharedState>> {
    Router::new()
        .route("/api/stores", get(list_stores).post(create_store))
        .route(
            "/api/stores/{name}",
            get(get_store).put(update_store).delete(delete_store),
        )
}

async fn list_stores(State(state): State<Arc<SharedState>>) -> impl IntoResponse {
    let gw = state.gateway.read().await;
    Json(gw.stores.clone()).into_response()
}

async fn get_store(
    State(state): State<Arc<SharedState>>,
    Path(name): Path<String>,
) -> impl IntoResponse {
    let gw = state.gateway.read().await;
    match gw.stores.iter().find(|s| s.name == name) {
        Some(s) => Json(s.clone()).into_response(),
        None => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "not_found"})),
        )
            .into_response(),
    }
}

async fn create_store(
    State(state): State<Arc<SharedState>>,
    Json(store): Json<StoreConfig>,
) -> impl IntoResponse {
    let candidate = {
        let gw = state.gateway.read().await;
        if gw.stores.iter().any(|s| s.name == store.name) {
            return (
                StatusCode::CONFLICT,
                Json(serde_json::json!({"error": "store already exists"})),
            )
                .into_response();
        }
        let mut candidate = gw.clone();
        candidate.stores.push(store);
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

async fn update_store(
    State(state): State<Arc<SharedState>>,
    Path(name): Path<String>,
    Json(mut store): Json<StoreConfig>,
) -> impl IntoResponse {
    store.name = name.clone();
    let candidate = {
        let gw = state.gateway.read().await;
        let mut candidate = gw.clone();
        if let Some(existing) = candidate.stores.iter_mut().find(|s| s.name == name) {
            *existing = store;
        } else {
            candidate.stores.push(store);
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

async fn delete_store(
    State(state): State<Arc<SharedState>>,
    Path(name): Path<String>,
) -> impl IntoResponse {
    let candidate = {
        let gw = state.gateway.read().await;
        let referrers = store_referrers(&gw, &name);
        if !referrers.is_empty() {
            return (
                StatusCode::CONFLICT,
                Json(serde_json::json!({"error": "in_use", "referrers": referrers})),
            )
                .into_response();
        }
        let mut candidate = gw.clone();
        let before = candidate.stores.len();
        candidate.stores.retain(|s| s.name != name);
        if candidate.stores.len() == before {
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

/// Everything that references store `name`: node config `store:` keys (flat,
/// for limit-count/workflow) and nested `session.store` (session plugins —
/// wired in a later plan, scanned now so the guard never lags the feature).
fn store_referrers(gw: &GatewayConfig, name: &str) -> Vec<String> {
    fn config_references(
        config: &std::collections::HashMap<String, serde_json::Value>,
        name: &str,
    ) -> bool {
        config.get("store").and_then(|v| v.as_str()) == Some(name)
            || config
                .get("session")
                .and_then(|v| v.get("store"))
                .and_then(|v| v.as_str())
                == Some(name)
    }
    fn scan_nodes(nodes: &[NodeConfig], owner: &str, name: &str, out: &mut Vec<String>) {
        for n in nodes {
            if config_references(&n.config, name) {
                out.push(format!("{} node '{}'", owner, n.id));
            }
        }
    }
    let mut refs = Vec::new();
    for p in &gw.policies {
        scan_nodes(&p.nodes, &format!("policy '{}'", p.name), name, &mut refs);
    }
    for s in &gw.supernodes {
        scan_nodes(
            &s.nodes,
            &format!("supernode '{}'", s.name),
            name,
            &mut refs,
        );
    }
    for pc in &gw.plugin_configs {
        if config_references(&pc.config, name) {
            refs.push(format!("plugin_config '{}'", pc.name));
        }
    }
    refs
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{GatewayConfig, SystemConfig};
    use crate::config_store::FileConfigStore;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
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

    const VALID_STORE: &str = r#"{
        "name": "s1",
        "type": "redis",
        "url": "${TEST_STORES_URL:-redis://127.0.0.1:6379}"
    }"#;

    async fn send(state: &Arc<SharedState>, req: Request<Body>) -> (StatusCode, serde_json::Value) {
        let resp = app(state.clone()).oneshot(req).await.unwrap();
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let v = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
        (status, v)
    }

    #[tokio::test]
    async fn test_crud_roundtrip_keeps_placeholders_raw() {
        let state = test_state("{}");
        let (status, _) = send(
            &state,
            Request::post("/api/stores")
                .header("content-type", "application/json")
                .body(Body::from(VALID_STORE))
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);

        // Duplicate create → 409.
        let (status, _) = send(
            &state,
            Request::post("/api/stores")
                .header("content-type", "application/json")
                .body(Body::from(VALID_STORE))
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);

        // GET serves the RAW placeholder, never a resolved value.
        let (status, body) = send(
            &state,
            Request::get("/api/stores/s1").body(Body::empty()).unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body["url"].as_str().unwrap(),
            "${TEST_STORES_URL:-redis://127.0.0.1:6379}"
        );

        // PUT upserts; invalid type is rejected by commit-time validation.
        let (status, body) = send(
            &state,
            Request::put("/api/stores/s1")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"name":"s1","type":"memcached","url":"redis://x"}"#,
                ))
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(
            body["error"].as_str().unwrap().contains("unknown type"),
            "{body}"
        );

        // DELETE, then 404.
        let (status, _) = send(
            &state,
            Request::delete("/api/stores/s1")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let (status, _) = send(
            &state,
            Request::get("/api/stores/s1").body(Body::empty()).unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn test_delete_referenced_store_is_409_with_referrers() {
        let state = test_state(
            r#"
stores:
  - name: s1
    type: redis
    url: redis://127.0.0.1:6379
plugin_configs:
  - name: shared-lc
    type: limit-count
    config: { count: 1, time_window: 60, policy: redis, store: s1 }
"#,
        );
        let (status, body) = send(
            &state,
            Request::delete("/api/stores/s1")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(body["error"], "in_use");
        let refs: Vec<String> = body["referrers"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect();
        assert!(
            refs.contains(&"plugin_config 'shared-lc'".to_string()),
            "{refs:?}"
        );
    }
}
