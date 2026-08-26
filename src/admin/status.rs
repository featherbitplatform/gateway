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
/// Ready means the route table is loaded **and** no ACME-managed certificate
/// is still serving a placeholder — renewal failures never affect readiness,
/// only a cert that has never successfully issued does. Returns `200 OK` with
/// the compiled route count (and the empty `acme.placeholder` list) once
/// both hold, or `503 Service Unavailable` while either does not.
async fn readyz(State(state): State<Arc<SharedState>>) -> impl IntoResponse {
    let routes = state.routes.read().await;
    if routes.is_empty() {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({"status": "not_ready", "reason": "no routes loaded"})),
        );
    }
    let placeholders = state
        .acme
        .load()
        .as_ref()
        .map(|rt| rt.placeholder_ids())
        .unwrap_or_default();
    if !placeholders.is_empty() {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({
                "status": "not_ready",
                "reason": "acme placeholder certs",
                "acme": {"placeholder": placeholders},
            })),
        );
    }
    (
        StatusCode::OK,
        Json(serde_json::json!({
            "status": "ready",
            "routes": routes.len(),
            "acme": {"placeholder": placeholders},
        })),
    )
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

#[cfg(test)]
mod acme_readyz_tests {
    use super::*;
    use crate::config::{GatewayConfig, SystemConfig};
    use crate::config_store::FileConfigStore;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    fn state() -> Arc<SharedState> {
        let system: SystemConfig = serde_yaml::from_str("{}").unwrap();
        let gateway: GatewayConfig = serde_yaml::from_str(
            "routes:\n  - name: r\n    match:\n      path: /x\n    policy: p\npolicies:\n  - name: p\n    nodes:\n      - id: in\n        type: listener\n      - id: out\n        type: client\n    edges:\n      - { from: in.out, to: out.in }\n",
        )
        .unwrap();
        Arc::new(
            SharedState::new(
                system,
                gateway,
                None,
                Arc::new(FileConfigStore::new("g.yaml".into())),
            )
            .unwrap(),
        )
    }

    async fn readyz_status(state: Arc<SharedState>) -> (StatusCode, serde_json::Value) {
        let resp = router()
            .with_state(state)
            .oneshot(
                Request::builder()
                    .uri("/readyz")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, serde_json::from_slice(&bytes).unwrap())
    }

    #[tokio::test]
    async fn readyz_is_503_while_a_managed_cert_is_a_placeholder() {
        let s = state();
        let (status, _) = readyz_status(s.clone()).await;
        assert_eq!(status, StatusCode::OK, "no acme ⇒ ready");

        s.acme
            .store(Some(crate::acme::testing::placeholder_runtime(&[
                "p.example.com",
            ])));
        let (status, body) = readyz_status(s.clone()).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body["acme"]["placeholder"][0], "p.example.com");

        s.acme.store(Some(crate::acme::testing::issued_runtime(&[
            "p.example.com",
        ])));
        let (status, body) = readyz_status(s).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["acme"]["placeholder"].as_array().unwrap().len(), 0);
    }
}
