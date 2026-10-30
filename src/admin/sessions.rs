//! Admin surface for server-side sessions: list and revoke. Meta only —
//! session payloads are sealed and never leave the store through this API.
//!
//! Every endpoint answers `501` on builds without the `redis-store` feature
//! (the `-headless` image), since server-side sessions need it.

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use super::openapi::{ErrorResponse, StatusResponse};
use crate::sessions::SessionMeta;
use crate::state::SharedState;

/// Builds the router for the `/api/sessions` endpoints.
pub fn router() -> OpenApiRouter<Arc<SharedState>> {
    OpenApiRouter::new()
        .routes(routes!(list_sessions, delete_by_subject))
        .routes(routes!(delete_session))
}

/// Query of `GET /api/sessions`.
#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
#[cfg_attr(not(feature = "redis-store"), allow(dead_code))]
struct SessionListQuery {
    /// Name of the declared store to list (`400` without it).
    #[param(required = true)]
    store: Option<String>,
    /// Only sessions of this authenticated subject.
    subject: Option<String>,
    /// Only sessions established by this node type, e.g. `openid-connect`.
    plugin: Option<String>,
    /// Page size; 50 when absent or not a non-negative integer.
    #[param(value_type = Option<u32>)]
    limit: Option<String>,
    /// Opaque cursor from a previous page's `next_cursor`.
    cursor: Option<String>,
}

/// Query of `DELETE /api/sessions`.
#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
#[cfg_attr(not(feature = "redis-store"), allow(dead_code))]
struct SessionRevokeQuery {
    /// Name of the declared store (`400` without it).
    #[param(required = true)]
    store: Option<String>,
    /// Subject whose sessions to revoke (`400` without it).
    #[param(required = true)]
    subject: Option<String>,
}

/// One page of `GET /api/sessions`.
#[derive(Debug, Serialize, ToSchema)]
#[cfg_attr(not(feature = "redis-store"), allow(dead_code))]
struct SessionList {
    /// Session metadata; payloads never leave the store.
    sessions: Vec<SessionMeta>,
    /// Cursor for the next page; `null` on the last page.
    next_cursor: Option<String>,
}

/// Result of `DELETE /api/sessions`.
#[derive(Debug, Serialize, ToSchema)]
#[cfg_attr(not(feature = "redis-store"), allow(dead_code))]
struct SessionsRevoked {
    /// How many sessions were revoked.
    revoked: u64,
}

/// The `501` every endpoint answers on builds without `redis-store`.
#[cfg(not(feature = "redis-store"))]
fn not_built() -> Response {
    (
        StatusCode::NOT_IMPLEMENTED,
        Json(serde_json::json!({
            "error": "this binary was built without the redis-store feature"
        })),
    )
        .into_response()
}

/// `GET /api/sessions?store=...` — lists the session metadata in a store,
/// optionally filtered by subject and plugin, one page at a time.
#[utoipa::path(
    get,
    path = "/api/sessions",
    tag = "sessions",
    summary = "List sessions",
    params(SessionListQuery),
    responses(
        (status = 200, description = "One page of session metadata.", body = SessionList),
        (status = 400, description = "`store` is missing.", body = ErrorResponse),
        (status = 404, description = "No session-capable store of that name is declared.", body = ErrorResponse),
        (status = 501, description = "This binary was built without the `redis-store` feature.", body = ErrorResponse),
        (status = 502, description = "The store failed.", body = ErrorResponse),
    ),
)]
async fn list_sessions(
    State(state): State<Arc<SharedState>>,
    Query(params): Query<SessionListQuery>,
) -> Response {
    #[cfg(not(feature = "redis-store"))]
    {
        let _ = (state, params);
        not_built()
    }
    #[cfg(feature = "redis-store")]
    {
        let Some(name) = params.store else {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": "store is required"})),
            )
                .into_response();
        };
        let store = match state.resources.stores.load().session_store(&name) {
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
            subject: params.subject,
            plugin: params.plugin,
            limit: params
                .limit
                .and_then(|v| v.parse::<usize>().ok())
                .unwrap_or(50),
            cursor: params.cursor,
        };
        match store.list(&filter).await {
            Ok(page) => Json(SessionList {
                sessions: page.sessions,
                next_cursor: page.next_cursor,
            })
            .into_response(),
            Err(e) => (
                StatusCode::BAD_GATEWAY,
                Json(serde_json::json!({"error": e.to_string()})),
            )
                .into_response(),
        }
    }
}

/// `DELETE /api/sessions/{store}/{id}` — revokes one session.
#[utoipa::path(
    delete,
    path = "/api/sessions/{store}/{id}",
    tag = "sessions",
    summary = "Revoke a session",
    params(
        ("store" = String, Path, description = "Name of the declared store holding the session."),
        ("id" = String, Path, description = "Session id: 32 hex chars."),
    ),
    responses(
        (status = 200, description = "Revoked (`status: deleted`).", body = StatusResponse),
        (status = 400, description = "`id` is not a valid session id.", body = ErrorResponse),
        (status = 404, description = "No session-capable store of that name is declared.", body = ErrorResponse),
        (status = 501, description = "This binary was built without the `redis-store` feature.", body = ErrorResponse),
        (status = 502, description = "The store failed.", body = ErrorResponse),
    ),
)]
async fn delete_session(
    State(state): State<Arc<SharedState>>,
    Path((store_name, id)): Path<(String, String)>,
) -> Response {
    #[cfg(not(feature = "redis-store"))]
    {
        let _ = (state, store_name, id);
        not_built()
    }
    #[cfg(feature = "redis-store")]
    {
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
}

/// `DELETE /api/sessions?store=...&subject=...` — revokes every session of
/// one subject in a store.
#[utoipa::path(
    delete,
    path = "/api/sessions",
    tag = "sessions",
    summary = "Revoke all sessions of a subject",
    operation_id = "delete_sessions_by_subject",
    params(SessionRevokeQuery),
    responses(
        (status = 200, description = "How many sessions were revoked.", body = SessionsRevoked),
        (status = 400, description = "`store` or `subject` is missing.", body = ErrorResponse),
        (status = 404, description = "No session-capable store of that name is declared.", body = ErrorResponse),
        (status = 501, description = "This binary was built without the `redis-store` feature.", body = ErrorResponse),
        (status = 502, description = "The store failed.", body = ErrorResponse),
    ),
)]
async fn delete_by_subject(
    State(state): State<Arc<SharedState>>,
    Query(params): Query<SessionRevokeQuery>,
) -> Response {
    #[cfg(not(feature = "redis-store"))]
    {
        let _ = (state, params);
        not_built()
    }
    #[cfg(feature = "redis-store")]
    {
        let Some(name) = params.store else {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": "store is required"})),
            )
                .into_response();
        };
        let Some(subject) = params.subject else {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": "subject is required"})),
            )
                .into_response();
        };
        let store = match state.resources.stores.load().session_store(&name) {
            Ok(s) => s,
            Err(_) => {
                return (
                    StatusCode::NOT_FOUND,
                    Json(serde_json::json!({"error": "not_found"})),
                )
                    .into_response()
            }
        };
        match store.delete_subject(&subject).await {
            Ok(n) => Json(SessionsRevoked { revoked: n }).into_response(),
            Err(e) => (
                StatusCode::BAD_GATEWAY,
                Json(serde_json::json!({"error": e.to_string()})),
            )
                .into_response(),
        }
    }
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
