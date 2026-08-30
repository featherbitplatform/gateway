//! Model Context Protocol server for AI agents.
//!
//! Layout: [`auth`] (bearer tokens → scope), [`tools`] (the typed tool
//! functions over [`crate::state::SharedState`]), [`docs`] (documentation
//! pages embedded in the binary), [`prompts`] (precompiled debugging and
//! authoring prompts). Everything here compiles in every build — the Admin
//! API's `/api/mcp/prompts` uses the renderer even without a transport. Only
//! [`server`] (the `rmcp` adapter and the mounted Streamable HTTP service)
//! sits behind the `mcp` cargo feature.

// The tool/resource/prompt layer is transport-agnostic and compiles in every
// build, but until the Admin API's `/api/mcp/*` endpoints land it is reached
// only through the `mcp` transport below.
#[cfg_attr(not(feature = "mcp"), allow(dead_code))]
pub mod auth;
#[cfg_attr(not(feature = "mcp"), allow(dead_code))]
pub mod docs;
#[cfg_attr(not(feature = "mcp"), allow(dead_code))]
pub mod prompts;
#[cfg_attr(not(feature = "mcp"), allow(dead_code))]
pub mod tools;

#[cfg(feature = "mcp")]
pub mod server;

#[cfg(feature = "mcp")]
use std::sync::Arc;

use axum::response::IntoResponse;
use axum::routing::any;
use axum::Router;

/// Router fragment answering the MCP path with `404` when the feature is off
/// or `admin.mcp.enabled` is false (the `/api/debug/*` convention: never
/// advertise a disabled surface).
///
/// The warning naming the key fires **once per process**, not once per
/// request: this route answers before any credential is checked, so a
/// per-request log would let anyone who can reach the admin listener drive
/// unbounded WARN volume. The first hit carries the whole diagnosis (and
/// `build_router` already logs the disabled state at startup); the rest is
/// noise.
pub fn disabled_router(path: &str) -> Router {
    static WARNED: std::sync::Once = std::sync::Once::new();
    async fn not_found() -> axum::response::Response {
        WARNED.call_once(|| {
            tracing::warn!(
                "MCP endpoint was requested but is disabled; set `admin.mcp.enabled: true` \
                 (FEATHERBIT_MCP_ENABLED=true) with at least one token in system.yaml and restart"
            );
        });
        (
            axum::http::StatusCode::NOT_FOUND,
            axum::Json(serde_json::json!({"error": "not_found"})),
        )
            .into_response()
    }
    Router::new().route(path, any(not_found))
}

/// The live MCP endpoint: bearer auth → rmcp Streamable HTTP service.
#[cfg(feature = "mcp")]
pub fn router(cfg: &crate::config::McpConfig, state: Arc<crate::state::SharedState>) -> Router {
    let auth_state = Arc::new(auth::McpAuthState::from_config(cfg));
    Router::new()
        .route_service(&cfg.path, server::build_service(state))
        .route_layer(axum::middleware::from_fn_with_state(
            auth_state,
            auth::bearer_middleware,
        ))
}
