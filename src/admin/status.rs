//! Operational endpoints for the admin API: liveness/readiness probes,
//! a status summary, Prometheus metrics, and manual config reload.

use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};

use crate::state::SharedState;

/// Builds the router for `/healthz`, `/readyz`, `/api/status`,
/// `/api/config/export`, `/api/config/reload`, and `/metrics`.
pub fn router() -> Router<Arc<SharedState>> {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz))
        .route("/api/status", get(status))
        .route("/api/config/export", get(export_config))
        .route("/api/config/reload", post(reload_config))
        .route("/metrics", get(metrics))
}

/// `GET /healthz` — liveness probe. Always `200 OK` with
/// `{"status": "healthy"}` while the process is running. Exempt from auth.
async fn healthz() -> impl IntoResponse {
    (
        StatusCode::OK,
        Json(serde_json::json!({"status": "healthy"})),
    )
}

/// `GET /readyz` — readiness probe. Exempt from auth.
///
/// Returns `200 OK` with the compiled route count once at least one route is
/// loaded, or `503 Service Unavailable` while the route table is empty.
async fn readyz(State(state): State<Arc<SharedState>>) -> impl IntoResponse {
    let routes = state.routes.read().await;
    if routes.is_empty() {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({"status": "not_ready", "reason": "no routes loaded"})),
        )
    } else {
        (
            StatusCode::OK,
            Json(serde_json::json!({"status": "ready", "routes": routes.len()})),
        )
    }
}

/// `GET /api/status` — gateway version plus route and policy counts.
///
/// ```json
/// { "version": "0.1.0", "routes": 3, "policies": 2 }
/// ```
async fn status(State(state): State<Arc<SharedState>>) -> impl IntoResponse {
    let routes = state.routes.read().await;
    let gw = state.gateway.read().await;
    Json(serde_json::json!({
        "version": env!("CARGO_PKG_VERSION"),
        "routes": routes.len(),
        "policies": gw.policies.len(),
    }))
}

/// `GET /api/config/export` — renders the live in-memory gateway
/// configuration (routes + policies, exactly what the data plane is running)
/// as YAML, served as `text/yaml; charset=utf-8`. This is the `gateway.yaml`
/// equivalent of whatever has been built through the UI / Admin API.
///
/// Values keep their `${ENV_VAR}` templates: env interpolation happens when a
/// policy is compiled, not in the stored config, so the export mirrors the
/// source you would write by hand rather than the resolved secrets.
///
/// Errors: `500 Internal Server Error` if the config cannot be serialized.
async fn export_config(State(state): State<Arc<SharedState>>) -> impl IntoResponse {
    let gw = state.gateway.read().await;
    match serde_yaml::to_string(&*gw) {
        Ok(yaml) => (
            StatusCode::OK,
            [("content-type", "text/yaml; charset=utf-8")],
            yaml,
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": format!("failed to serialize config: {}", e)})),
        )
            .into_response(),
    }
}

/// `GET /metrics` — renders the shared gateway registry (per-route and
/// per-node counters/histograms recorded by the data plane) in Prometheus
/// text exposition format, served as `text/plain; charset=utf-8`.
async fn metrics(State(state): State<Arc<SharedState>>) -> impl IntoResponse {
    (
        StatusCode::OK,
        [("content-type", "text/plain; charset=utf-8")],
        state.metrics.render(),
    )
}

/// `POST /api/config/reload` — re-reads `gateway.yaml` from disk (with env
/// interpolation), recompiles all route graphs, and swaps them in. Returns
/// `{"status": "reloaded"}` on success.
///
/// Errors: `500 Internal Server Error` if no config path is set or the file
/// fails to parse/validate/compile; the running config is left unchanged.
async fn reload_config(State(state): State<Arc<SharedState>>) -> impl IntoResponse {
    match state.reload_from_disk().await {
        Ok(_) => (
            StatusCode::OK,
            Json(serde_json::json!({"status": "reloaded"})),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e})),
        )
            .into_response(),
    }
}
