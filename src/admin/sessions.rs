//! Admin surface for server-side sessions: list and revoke. Meta only —
//! session payloads are sealed and never leave the store through this API.

use std::collections::HashMap;
use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router};

use crate::state::SharedState;

pub fn router() -> Router<Arc<SharedState>> {
    Router::new()
        .route(
            "/api/sessions",
            get(list_sessions).delete(delete_by_subject),
        )
        .route(
            "/api/sessions/{store}/{id}",
            axum::routing::delete(delete_session),
        )
}

#[cfg(feature = "redis-store")]
async fn list_sessions(
    State(state): State<Arc<SharedState>>,
    Query(params): Query<HashMap<String, String>>,
) -> impl IntoResponse {
    let Some(name) = params.get("store") else {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "store is required"})),
        )
            .into_response();
    };
    let store = match state.resources.stores.load().session_store(name) {
        Ok(s) => s,
        Err(_) => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({"error": "not_found"})),
            )
                .into_response()
        }
    };
    let filter = crate::sessions::SessionFilter {
        subject: params.get("subject").cloned(),
        plugin: params.get("plugin").cloned(),
        limit: params
            .get("limit")
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(50),
        cursor: params.get("cursor").cloned(),
    };
    match store.list(&filter).await {
        Ok(page) => Json(serde_json::json!({
            "sessions": page.sessions,
            "next_cursor": page.next_cursor,
        }))
        .into_response(),
        Err(e) => (
            StatusCode::BAD_GATEWAY,
            Json(serde_json::json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

#[cfg(not(feature = "redis-store"))]
async fn list_sessions(
    State(_state): State<Arc<SharedState>>,
    Query(_params): Query<HashMap<String, String>>,
) -> impl IntoResponse {
    (
        StatusCode::NOT_IMPLEMENTED,
        Json(serde_json::json!({
            "error": "this binary was built without the redis-store feature"
        })),
    )
        .into_response()
}

#[cfg(feature = "redis-store")]
async fn delete_session(
    State(state): State<Arc<SharedState>>,
    Path((store_name, id)): Path<(String, String)>,
) -> impl IntoResponse {
    let Some(session_id) = crate::sessions::SessionId::parse(&id) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "invalid session id"})),
        )
            .into_response();
    };
    let store = match state.resources.stores.load().session_store(&store_name) {
        Ok(s) => s,
        Err(_) => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({"error": "not_found"})),
            )
                .into_response()
        }
    };
    match store.delete(&session_id).await {
        Ok(()) => Json(serde_json::json!({"status": "deleted"})).into_response(),
        Err(e) => (
            StatusCode::BAD_GATEWAY,
            Json(serde_json::json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

#[cfg(not(feature = "redis-store"))]
async fn delete_session(
    State(_state): State<Arc<SharedState>>,
    Path((_store_name, _id)): Path<(String, String)>,
) -> impl IntoResponse {
    (
        StatusCode::NOT_IMPLEMENTED,
        Json(serde_json::json!({
            "error": "this binary was built without the redis-store feature"
        })),
    )
        .into_response()
}

#[cfg(feature = "redis-store")]
async fn delete_by_subject(
    State(state): State<Arc<SharedState>>,
    Query(params): Query<HashMap<String, String>>,
) -> impl IntoResponse {
    let Some(name) = params.get("store") else {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "store is required"})),
        )
            .into_response();
    };
    let Some(subject) = params.get("subject") else {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "subject is required"})),
        )
            .into_response();
    };
    let store = match state.resources.stores.load().session_store(name) {
        Ok(s) => s,
        Err(_) => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({"error": "not_found"})),
            )
                .into_response()
        }
    };
    match store.delete_subject(subject).await {
        Ok(n) => Json(serde_json::json!({"revoked": n})).into_response(),
        Err(e) => (
            StatusCode::BAD_GATEWAY,
            Json(serde_json::json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

#[cfg(not(feature = "redis-store"))]
async fn delete_by_subject(
    State(_state): State<Arc<SharedState>>,
    Query(_params): Query<HashMap<String, String>>,
) -> impl IntoResponse {
    (
        StatusCode::NOT_IMPLEMENTED,
        Json(serde_json::json!({
            "error": "this binary was built without the redis-store feature"
        })),
    )
        .into_response()
}

#[cfg(all(test, feature = "redis-store"))]
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

    async fn send(state: &Arc<SharedState>, req: Request<Body>) -> (StatusCode, serde_json::Value) {
        let resp = app(state.clone()).oneshot(req).await.unwrap();
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let v = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
        (status, v)
    }

    const GATEWAY_WITH_S1: &str = r#"
stores:
  - name: s1
    type: redis
    url: redis://127.0.0.1:6379
"#;

    #[cfg(feature = "redis-store")]
    fn state_with_fake_store(fake: Arc<crate::sessions::FakeSessionStore>) -> Arc<SharedState> {
        let state = test_state(GATEWAY_WITH_S1);
        state.resources.stores.store(Arc::new(
            crate::stores::StoreRegistry::with_fake_session_store("s1", fake.clone()),
        ));
        state
    }

    #[cfg(feature = "redis-store")]
    #[tokio::test]
    async fn test_sessions_list_revoke_roundtrip() {
        use crate::sessions::SessionStore as _;

        let fake = Arc::new(crate::sessions::FakeSessionStore::default());
        let state = state_with_fake_store(fake.clone());

        let alice_id = crate::sessions::SessionId::random();
        let alice_meta = crate::sessions::SessionMeta {
            id: String::new(),
            subject: "alice".to_string(),
            plugin: "openid-connect".to_string(),
            policy: "p1".to_string(),
            route: "r1".to_string(),
            created_at: 1,
            expires_at: 2,
        };
        fake.put(
            &alice_id,
            b"sealed-alice",
            std::time::Duration::from_secs(60),
            &alice_meta,
        )
        .await
        .unwrap();

        let bob_id = crate::sessions::SessionId::random();
        let bob_meta = crate::sessions::SessionMeta {
            id: String::new(),
            subject: "bob".to_string(),
            plugin: "openid-connect".to_string(),
            policy: "p1".to_string(),
            route: "r1".to_string(),
            created_at: 1,
            expires_at: 2,
        };
        fake.put(
            &bob_id,
            b"sealed-bob",
            std::time::Duration::from_secs(60),
            &bob_meta,
        )
        .await
        .unwrap();

        // GET /api/sessions?store=s1 -> 200, 2 sessions, meta fields present,
        // no payload-like field.
        let (status, body) = send(
            &state,
            Request::get("/api/sessions?store=s1")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let sessions = body["sessions"].as_array().unwrap();
        assert_eq!(sessions.len(), 2);
        for s in sessions {
            assert!(s.get("id").is_some());
            assert!(s.get("subject").is_some());
            assert!(s.get("plugin").is_some());
            assert!(s.get("created_at").is_some());
            assert!(s.get("expires_at").is_some());
            assert!(s.get("sealed").is_none());
            assert!(s.get("payload").is_none());
        }

        // filter subject=alice -> 1
        let (status, body) = send(
            &state,
            Request::get("/api/sessions?store=s1&subject=alice")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let sessions = body["sessions"].as_array().unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0]["subject"], "alice");

        // DELETE /api/sessions/s1/{alice's id} -> 200 deleted
        let (status, body) = send(
            &state,
            Request::delete(format!("/api/sessions/s1/{}", alice_id.as_str()))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["status"], "deleted");

        // DELETE /api/sessions?store=s1&subject=bob -> 200 {"revoked": 1}
        let (status, body) = send(
            &state,
            Request::delete("/api/sessions?store=s1&subject=bob")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["revoked"], 1);

        // GET -> 0 sessions
        let (status, body) = send(
            &state,
            Request::get("/api/sessions?store=s1")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["sessions"].as_array().unwrap().len(), 0);
    }

    #[cfg(feature = "redis-store")]
    #[tokio::test]
    async fn test_sessions_param_validation() {
        let fake = Arc::new(crate::sessions::FakeSessionStore::default());
        let state = state_with_fake_store(fake);

        // GET /api/sessions (no store) -> 400
        let (status, _) = send(
            &state,
            Request::get("/api/sessions").body(Body::empty()).unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);

        // GET /api/sessions?store=unknown -> 404
        let (status, _) = send(
            &state,
            Request::get("/api/sessions?store=unknown")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);

        // DELETE /api/sessions/s1/not-a-valid-id -> 400
        let (status, _) = send(
            &state,
            Request::delete("/api/sessions/s1/not-a-valid-id")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);

        // DELETE /api/sessions?store=s1 (no subject) -> 400
        let (status, _) = send(
            &state,
            Request::delete("/api/sessions?store=s1")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }
}
