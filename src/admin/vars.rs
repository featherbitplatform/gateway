//! Admin API endpoint serving the context-variable catalog
//! (src/vars/catalog.rs) — consumed by the UI's autocomplete and var legend.

use std::sync::Arc;

use axum::response::IntoResponse;
use axum::Json;
use serde::Serialize;
use utoipa::ToSchema;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use crate::state::SharedState;
use crate::vars::catalog::VarEntry;

/// Builds the router for `/api/vars`.
pub fn router() -> OpenApiRouter<Arc<SharedState>> {
    OpenApiRouter::new().routes(routes!(list_vars))
}

/// Body of `GET /api/vars`.
#[derive(Serialize, ToSchema)]
struct VarCatalog {
    /// Every variable the resolver accepts, statics first, then families.
    vars: Vec<VarEntry>,
}

/// `GET /api/vars` — the static catalog of `$var` names plugins can
/// interpolate, with kinds, family sources, and descriptions. Always `200 OK`.
#[utoipa::path(
    get,
    path = "/api/vars",
    tag = "authoring",
    summary = "List context variables",
    responses((status = 200, description = "The `$var` catalog.", body = VarCatalog)),
)]
async fn list_vars() -> impl IntoResponse {
    Json(VarCatalog {
        vars: crate::vars::catalog::var_catalog(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use axum::routing::get;
    use axum::Router;
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
