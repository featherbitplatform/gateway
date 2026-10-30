//! Admin CRUD for the `stores:` resource (named redis/valkey connections).
//!
//! Responses always carry the RAW stored config — `${ENV}` placeholders are
//! never resolved here (they resolve only when a client is built). Deleting
//! a store still referenced by any plugin config is rejected with
//! `409 {"error":"in_use","referrers":[...]}` — a new envelope, since the
//! reference is not discoverable via graph recompilation (a store used only
//! by a shared plugin_config compiles fine without the node being wired).
//! The same guard also covers `acme.storage` (`system.yaml`), since an ACME
//! `type: store` storage config references a store outside `gateway.yaml`
//! entirely and would otherwise go undetected.

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Serialize;
use utoipa::ToSchema;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use super::openapi::{ErrorResponse, StatusResponse};
use crate::config::{GatewayConfig, NodeConfig, StoreConfig};
use crate::state::SharedState;

/// Builds the router for the `/api/stores` endpoints.
pub fn router() -> OpenApiRouter<Arc<SharedState>> {
    OpenApiRouter::new()
        .routes(routes!(list_stores, create_store))
        .routes(routes!(get_store, update_store, delete_store))
        .routes(routes!(ping_store))
}

/// `409` body of `DELETE /api/stores/{name}` when the store is still
/// referenced.
#[derive(Debug, Serialize, ToSchema)]
struct StoreInUse {
    /// Always `in_use`.
    error: String,
    /// Every referrer, human-readable: e.g. `plugin_config 'shared-lc'`,
    /// `policy 'p' node 'lc'`, `acme.storage (system.yaml)`.
    referrers: Vec<String>,
}

/// `200` body of `POST /api/stores/{name}/ping`. Never echoes the resolved
/// connection details.
#[derive(Debug, Serialize, ToSchema)]
#[cfg_attr(not(feature = "redis-store"), allow(dead_code))]
struct StorePingResult {
    /// Always `ok`.
    status: String,
    /// Round-trip time of the `PING`, in milliseconds.
    latency_ms: u64,
    /// Server version: `valkey_version` on Valkey, else `redis_version`.
    version: String,
}

/// `504` body of `POST /api/stores/{name}/ping`.
#[derive(Debug, Serialize, ToSchema)]
#[cfg_attr(not(feature = "redis-store"), allow(dead_code))]
struct StorePingTimeout {
    /// Always `ping_timeout`.
    error: String,
    /// Which bound elapsed, e.g. `no reply within connect_timeout_ms (2000ms)`.
    message: String,
}

/// `GET /api/stores` — returns every declared store with its raw config.
#[utoipa::path(
    get,
    path = "/api/stores",
    tag = "stores",
    summary = "List stores",
    responses((status = 200, description = "All stores, raw: `${ENV}` placeholders are returned unresolved.", body = Vec<StoreConfig>)),
)]
async fn list_stores(State(state): State<Arc<SharedState>>) -> impl IntoResponse {
    let gw = state.gateway.read().await;
    Json(gw.stores.clone()).into_response()
}

/// `GET /api/stores/{name}` — returns the named store's raw config.
///
/// Errors: `404 Not Found` if no store with that name exists.
#[utoipa::path(
    get,
    path = "/api/stores/{name}",
    tag = "stores",
    summary = "Get a store",
    params(("name" = String, Path, description = "Store name.")),
    responses(
        (status = 200, description = "The store, raw: `${ENV}` placeholders are returned unresolved.", body = StoreConfig),
        (status = 404, description = "No such store.", body = ErrorResponse),
    ),
)]
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

/// `POST /api/stores` — declares a new store from the JSON body, then
/// revalidates the configuration and rebuilds the store clients. Returns
/// `201 Created` with `{"status": "created"}`.
///
/// Errors: `409 Conflict` if a store with the same name already exists;
/// `400 Bad Request` if the resulting configuration fails validation.
#[utoipa::path(
    post,
    path = "/api/stores",
    tag = "stores",
    summary = "Create a store",
    request_body = StoreConfig,
    responses(
        (status = 201, description = "Created (`status: created`).", body = StatusResponse),
        (status = 400, description = "The resulting configuration failed validation.", body = ErrorResponse),
        (status = 409, description = "A store with that name already exists.", body = ErrorResponse),
    ),
)]
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

/// `PUT /api/stores/{name}` — creates or replaces the named store (the path
/// name overrides any name in the body), then revalidates. Returns
/// `{"status": "updated"}` either way.
///
/// Errors: `400 Bad Request` if the resulting configuration fails validation.
#[utoipa::path(
    put,
    path = "/api/stores/{name}",
    tag = "stores",
    summary = "Create or replace a store",
    params(("name" = String, Path, description = "Store name; overrides any `name` in the body.")),
    request_body = StoreConfig,
    responses(
        (status = 200, description = "Upserted (`status: updated`, also when the store was new).", body = StatusResponse),
        (status = 400, description = "The resulting configuration failed validation.", body = ErrorResponse),
    ),
)]
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

/// `DELETE /api/stores/{name}` — removes the named store, then revalidates.
/// Returns `{"status": "deleted"}`.
///
/// Errors: `409 Conflict` with the list of referrers while any node, shared
/// plugin config or `acme.storage` still references the store; `404 Not
/// Found` if it does not exist; `400 Bad Request` if validation fails.
#[utoipa::path(
    delete,
    path = "/api/stores/{name}",
    tag = "stores",
    summary = "Delete a store",
    params(("name" = String, Path, description = "Store name.")),
    responses(
        (status = 200, description = "Deleted (`status: deleted`).", body = StatusResponse),
        (status = 400, description = "The resulting configuration failed validation.", body = ErrorResponse),
        (status = 404, description = "No such store.", body = ErrorResponse),
        (status = 409, description = "The store is still referenced (`error: in_use`); `referrers` lists every referrer.", body = StoreInUse),
    ),
)]
async fn delete_store(
    State(state): State<Arc<SharedState>>,
    Path(name): Path<String>,
) -> impl IntoResponse {
    let candidate = {
        let gw = state.gateway.read().await;
        let mut referrers = store_referrers(&gw, &name);
        if let Some(crate::config::AcmeConfig {
            storage: crate::config::AcmeStorageConfig::Store { store, .. },
            ..
        }) = &state.system.acme
        {
            if store == &name {
                referrers.push("acme.storage (system.yaml)".to_string());
            }
        }
        if !referrers.is_empty() {
            return (
                StatusCode::CONFLICT,
                Json(StoreInUse {
                    error: "in_use".to_string(),
                    referrers,
                }),
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

/// `POST /api/stores/{name}/ping` — connectivity check.
///
/// Resolves the store's config (env placeholders included) and PINGs it,
/// bounded by the store's own `connect_timeout_ms`. The response never
/// echoes resolved connection details — only latency and version.
#[utoipa::path(
    post,
    path = "/api/stores/{name}/ping",
    tag = "stores",
    summary = "Ping a store",
    params(("name" = String, Path, description = "Store name.")),
    responses(
        (status = 200, description = "The store answered (`status: ok`).", body = StorePingResult),
        (status = 400, description = "The store's config could not be turned into a client (e.g. an invalid URL after `${ENV}` resolution).", body = ErrorResponse),
        (status = 404, description = "No such store.", body = ErrorResponse),
        (status = 501, description = "This binary was built without the `redis-store` feature.", body = ErrorResponse),
        (status = 502, description = "The store could not be reached or rejected the `PING`.", body = ErrorResponse),
        (status = 504, description = "No reply within the store's `connect_timeout_ms` (`error: ping_timeout`).", body = StorePingTimeout),
    ),
)]
async fn ping_store(State(state): State<Arc<SharedState>>, Path(name): Path<String>) -> Response {
    ping(state, name).await
}

#[cfg(feature = "redis-store")]
async fn ping(state: Arc<SharedState>, name: String) -> Response {
    let cfg = {
        let gw = state.gateway.read().await;
        match gw.stores.iter().find(|s| s.name == name) {
            Some(s) => s.clone(),
            None => {
                return (
                    StatusCode::NOT_FOUND,
                    Json(serde_json::json!({"error": "not_found"})),
                )
                    .into_response()
            }
        }
    };
    let client = match crate::stores::redis_store::RedisStoreClient::build(&cfg) {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": e})),
            )
                .into_response()
        }
    };
    match tokio::time::timeout(client.connect_timeout(), client.ping()).await {
        Err(_) => (
            StatusCode::GATEWAY_TIMEOUT,
            Json(StorePingTimeout {
                error: "ping_timeout".to_string(),
                message: format!(
                    "no reply within connect_timeout_ms ({}ms)",
                    cfg.connect_timeout_ms
                ),
            }),
        )
            .into_response(),
        Ok(Err(e)) => (
            StatusCode::BAD_GATEWAY,
            Json(serde_json::json!({"error": e})),
        )
            .into_response(),
        Ok(Ok(info)) => Json(StorePingResult {
            status: "ok".to_string(),
            latency_ms: info.latency_ms,
            version: info.version,
        })
        .into_response(),
    }
}

#[cfg(not(feature = "redis-store"))]
async fn ping(_state: Arc<SharedState>, _name: String) -> Response {
    (
        StatusCode::NOT_IMPLEMENTED,
        Json(serde_json::json!({
            "error": "this binary was built without the redis-store feature"
        })),
    )
        .into_response()
}

/// Everything that references store `name`: node config `store:` keys (flat,
/// for limit-count/workflow), nested `session.store` (session plugins —
/// wired in a later plan, scanned now so the guard never lags the feature),
/// and `workflow`'s per-rule action params (`config.rules[].actions[][1].store`
/// — actions are `[name, params]` pairs; see `plugins/native/workflow.rs`).
pub(crate) fn store_referrers(gw: &GatewayConfig, name: &str) -> Vec<String> {
    fn config_references(
        config: &std::collections::HashMap<String, serde_json::Value>,
        name: &str,
    ) -> bool {
        config.get("store").and_then(|v| v.as_str()) == Some(name)
            || config.get("session_store").and_then(|v| v.as_str()) == Some(name)
            || config
                .get("session")
                .and_then(|v| v.get("store"))
                .and_then(|v| v.as_str())
                == Some(name)
            || config
                .get("rules")
                .and_then(|v| v.as_array())
                .is_some_and(|rules| rules.iter().any(|rule| rule_references(rule, name)))
    }

    fn rule_references(rule: &serde_json::Value, name: &str) -> bool {
        rule.get("actions")
            .and_then(|v| v.as_array())
            .is_some_and(|actions| actions.iter().any(|action| action_references(action, name)))
    }

    fn action_references(action: &serde_json::Value, name: &str) -> bool {
        action
            .as_array()
            .and_then(|a| a.get(1))
            .and_then(|params| params.as_object())
            .and_then(|params| params.get("store"))
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

    #[tokio::test]
    async fn test_delete_referenced_by_workflow_action_is_409_with_referrers() {
        let state = test_state(
            r#"
stores:
  - name: s1
    type: redis
    url: redis://127.0.0.1:6379
plugin_configs:
  - name: shared-wf
    type: workflow
    config:
      rules:
        - case: [["uri", "==", "/x"]]
          actions:
            - ["limit-count", { count: 1, time_window: 1, policy: redis, store: s1 }]
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
            refs.contains(&"plugin_config 'shared-wf'".to_string()),
            "{refs:?}"
        );
    }

    /// The UI's SchemaForm writes the FLAT `session_store` key (the nested
    /// `session.store` form is hand-written YAML); the delete guard must see
    /// both, or a store referenced only by a UI-authored session plugin
    /// config could be deleted out from under it.
    #[tokio::test]
    async fn test_delete_store_referenced_by_flat_session_store_is_409() {
        let state = test_state(
            r#"
stores:
  - name: s1
    type: redis
    url: redis://127.0.0.1:6379
plugin_configs:
  - name: ui-oidc
    type: openid-connect
    config: { session_storage: redis, session_store: s1 }
"#,
        );
        let (status, body) = send(
            &state,
            Request::delete("/api/stores/s1")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        assert_eq!(body["error"], "in_use");
        let refs: Vec<String> = body["referrers"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect();
        assert!(
            refs.contains(&"plugin_config 'ui-oidc'".to_string()),
            "{refs:?}"
        );
    }

    #[tokio::test]
    #[cfg(feature = "redis-store")]
    async fn test_delete_store_used_by_acme_storage_is_409() {
        let system: SystemConfig = crate::yaml::from_str(
            "acme:\n  terms_of_service_agreed: true\n  storage:\n    type: store\n    store: s1\n    encryption_key: k\n",
        )
        .unwrap();
        let gateway: GatewayConfig = crate::yaml::from_str(
            "stores:\n  - name: s1\n    type: redis\n    url: redis://127.0.0.1:6379\n",
        )
        .unwrap();
        let state = Arc::new(
            SharedState::new(
                system,
                gateway,
                None,
                Arc::new(FileConfigStore::new("gateway.yaml".into())),
            )
            .unwrap(),
        );
        let req = Request::builder()
            .method("DELETE")
            .uri("/api/stores/s1")
            .body(Body::empty())
            .unwrap();
        let resp = app(state).oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::CONFLICT);
        let body: serde_json::Value = serde_json::from_slice(
            &axum::body::to_bytes(resp.into_body(), usize::MAX)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(body["error"], "in_use");
        assert!(body["referrers"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r.as_str().unwrap().contains("acme.storage")));
    }

    /// Ping on an unknown store is 404; on an unreachable store it is a 502
    /// or 504 within the configured timeout — never a hang, never a 200.
    #[tokio::test]
    #[cfg(feature = "redis-store")]
    async fn test_ping_unknown_and_unreachable() {
        // Port 1 is reserved/closed; 300ms timeout keeps the test fast.
        let state = test_state(
            "stores:\n  - name: dead\n    type: redis\n    url: redis://127.0.0.1:1\n    connect_timeout_ms: 300\n",
        );
        let (status, _) = send(
            &state,
            Request::post("/api/stores/nope/ping")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);

        let (status, body) = send(
            &state,
            Request::post("/api/stores/dead/ping")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert!(
            status == StatusCode::BAD_GATEWAY || status == StatusCode::GATEWAY_TIMEOUT,
            "{status} {body}"
        );
    }

    /// Live ping; skipped unless FEATHERBIT_TEST_REDIS_URL is set.
    #[tokio::test]
    #[cfg(feature = "redis-store")]
    async fn test_ping_live() {
        let Ok(url) = std::env::var("FEATHERBIT_TEST_REDIS_URL") else {
            eprintln!("skipping test_ping_live: FEATHERBIT_TEST_REDIS_URL not set");
            return;
        };
        let state = test_state(&format!(
            "stores:\n  - name: live\n    type: redis\n    url: {url}\n"
        ));
        let (status, body) = send(
            &state,
            Request::post("/api/stores/live/ping")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["status"], "ok");
        assert!(body["latency_ms"].is_u64(), "{body}");
        assert!(body["version"].is_string(), "{body}");
    }
}
