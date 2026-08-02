//! Admin API endpoint serving the context-variable catalog
//! (src/vars/catalog.rs) — consumed by the UI's autocomplete and var legend.

use std::sync::Arc;

use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router};

use crate::state::SharedState;

/// Builds the router for `/api/vars`.
pub fn router() -> Router<Arc<SharedState>> {
    Router::new().route("/api/vars", get(list_vars))
}

/// `GET /api/vars` — the static catalog of `$var` names plugins can
/// interpolate, with kinds, family sources, and descriptions. Always `200 OK`.
async fn list_vars() -> impl IntoResponse {
    Json(serde_json::json!({ "vars": crate::vars::catalog::var_catalog() }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    #[tokio::test]
    async fn test_list_vars_shape() {
        // Stateless handler; a state-free router instance suffices.
        let app: Router = Router::new().route("/api/vars", get(list_vars));
        let resp = app
            .oneshot(Request::get("/api/vars").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let vars = v["vars"].as_array().unwrap();
        assert!(vars
            .iter()
            .any(|e| e["name"] == "uri" && e["kind"] == "static"));
        assert!(vars.iter().any(|e| e["name"] == "http_*"
            && e["kind"] == "family"
            && e["family_source"] == "request_headers"));
        assert!(vars.iter().any(|e| e["name"] == "sent_http_*"));
        assert!(vars.iter().any(|e| e["name"] == "request_body"));
    }
}
