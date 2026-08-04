//! Admin API endpoint serving environment variable names.
//! Consumed by the UI for dynamic configuration and secrets management.

use std::sync::Arc;

use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router};

use crate::state::SharedState;

/// Builds the router for `/api/env-vars`.
pub fn router() -> Router<Arc<SharedState>> {
    Router::new().route("/api/env-vars", get(list_env_vars))
}

/// `GET /api/env-vars` — the names of all environment variables available
/// to the current process. Returns them sorted, with no values exposed.
///
/// Uses `vars_os` + `to_string_lossy` rather than `std::env::vars()`, which
/// panics on a non-Unicode name or value — an environment the gateway
/// process didn't choose (inherited from its container/host) must not be
/// able to take down this handler.
async fn list_env_vars() -> impl IntoResponse {
    let mut names: Vec<String> = std::env::vars_os()
        .map(|(k, _)| k.to_string_lossy().into_owned())
        .collect();
    names.sort();
    Json(serde_json::json!({ "names": names }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    #[tokio::test]
    async fn test_list_env_vars_shape_and_secret_safety() {
        // Set a test env var with a secret value
        std::env::set_var("FB_TEST_SECRET_VALUE_CANARY", "s3cr3t");

        // Stateless handler; a state-free router instance suffices.
        let app: Router = Router::new().route("/api/env-vars", get(list_env_vars));
        let resp = app
            .oneshot(Request::get("/api/env-vars").body(Body::empty()).unwrap())
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);

        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let body_str = String::from_utf8(bytes.to_vec()).unwrap();

        // Verify the body is valid JSON
        let v: serde_json::Value = serde_json::from_str(&body_str).unwrap();
        let names = v["names"].as_array().unwrap();

        // Verify names are present and sorted
        assert!(!names.is_empty());
        let names_vec: Vec<String> = names
            .iter()
            .filter_map(|n| n.as_str().map(String::from))
            .collect();
        let mut sorted_names = names_vec.clone();
        sorted_names.sort();
        assert_eq!(names_vec, sorted_names, "env var names not sorted");

        // Verify the secret value never appears in the response body
        assert!(
            !body_str.contains("s3cr3t"),
            "secret value should not appear in response body"
        );

        // Verify the test env var name itself IS present
        assert!(
            names_vec.iter().any(|n| n == "FB_TEST_SECRET_VALUE_CANARY"),
            "test env var name should be in response"
        );
    }
}
