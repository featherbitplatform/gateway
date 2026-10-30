//! Operational endpoints for the admin API: liveness/readiness probes,
//! a status summary, Prometheus metrics, and manual config reload.

use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use serde::Serialize;
use utoipa::ToSchema;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use super::openapi::{ErrorResponse, StatusResponse};
use crate::config::GatewayConfig;
use crate::state::SharedState;

/// Builds the router for `/healthz`, `/readyz`, `/api/status`,
/// `/api/config/export`, `/api/config/reload`, and `/metrics`.
pub fn router() -> OpenApiRouter<Arc<SharedState>> {
    OpenApiRouter::new()
        .routes(routes!(healthz))
        .routes(routes!(readyz))
        .routes(routes!(status))
        .routes(routes!(export_config))
        .routes(routes!(reload_config))
        .routes(routes!(metrics))
}

/// `GET /healthz` — liveness probe. Always `200 OK` with
/// `{"status": "healthy"}` while the process is running. Exempt from auth.
#[utoipa::path(
    get,
    path = "/healthz",
    tag = "operations",
    summary = "Liveness probe",
    security(()),
    responses((status = 200, description = "The process is running (`status: healthy`).", body = StatusResponse)),
)]
async fn healthz() -> impl IntoResponse {
    (
        StatusCode::OK,
        Json(StatusResponse {
            status: "healthy".to_string(),
        }),
    )
}

/// Body of `GET /readyz`, ready or not.
#[derive(Debug, Serialize, ToSchema)]
struct Readiness {
    /// `ready` or `not_ready`.
    status: &'static str,
    /// Why the gateway is not ready (`no routes loaded`, `acme starting` or
    /// `acme placeholder certs`); absent when ready.
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<&'static str>,
    /// Number of compiled routes; present only when ready.
    #[serde(skip_serializing_if = "Option::is_none")]
    routes: Option<usize>,
    /// ACME certificate readiness; present in every branch.
    acme: ReadinessAcme,
}

/// The `acme` part of a [`Readiness`] body.
#[derive(Debug, Serialize, ToSchema)]
struct ReadinessAcme {
    /// Ids of ACME-managed certificates still serving a placeholder (never
    /// successfully issued). Empty when ready.
    placeholder: Vec<String>,
}

impl Readiness {
    /// A `503` not-ready answer.
    fn not_ready(reason: &'static str, placeholder: Vec<String>) -> (StatusCode, Json<Self>) {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(Self {
                status: "not_ready",
                reason: Some(reason),
                routes: None,
                acme: ReadinessAcme { placeholder },
            }),
        )
    }
}

/// `GET /readyz` — readiness probe. Exempt from auth.
///
/// Ready means the route table is loaded, the ACME runtime exists when
/// `system.yaml` asks for managed certificates, **and** no ACME-managed
/// certificate is still serving a placeholder — renewal failures never affect
/// readiness, only a cert that has never successfully issued does. Returns `200 OK` with
/// the compiled route count (and the empty `acme.placeholder` list) once
/// both hold, or `503 Service Unavailable` while either does not. Every
/// branch carries the same `acme.placeholder` shape, even "no routes loaded"
/// (where the acme runtime hasn't necessarily been consulted), so callers
/// can rely on one JSON shape regardless of why readiness failed.
#[utoipa::path(
    get,
    path = "/readyz",
    tag = "operations",
    summary = "Readiness probe",
    security(()),
    responses(
        (status = 200, description = "Ready (`status: ready`, with the compiled route count).", body = Readiness),
        (status = 503, description = "Not ready (`status: not_ready`, with a `reason`): no routes loaded, the ACME runtime is still starting, or a managed certificate is still a placeholder.", body = Readiness),
    ),
)]
async fn readyz(State(state): State<Arc<SharedState>>) -> impl IntoResponse {
    let routes = state.routes.read().await;
    if routes.is_empty() {
        return Readiness::not_ready("no routes loaded", Vec::new());
    }
    // ACME is configured but `server::start_server` has not seeded the runtime
    // yet: the admin listener comes up first, so without this the probe would
    // answer 200 in the window before any managed certificate — placeholder or
    // real — exists.
    if state.acme_expected && state.acme.load().is_none() {
        return Readiness::not_ready("acme starting", Vec::new());
    }
    let placeholders = state
        .acme
        .load()
        .as_ref()
        .map(|rt| rt.placeholder_ids())
        .unwrap_or_default();
    if !placeholders.is_empty() {
        return Readiness::not_ready("acme placeholder certs", placeholders);
    }
    (
        StatusCode::OK,
        Json(Readiness {
            status: "ready",
            reason: None,
            routes: Some(routes.len()),
            acme: ReadinessAcme {
                placeholder: placeholders,
            },
        }),
    )
}

/// Body of `GET /api/status`.
#[derive(Debug, Serialize, ToSchema)]
struct StatusSummary {
    /// Gateway version (the crate version).
    version: &'static str,
    /// Number of compiled routes.
    routes: usize,
    /// Number of configured policies.
    policies: usize,
    /// Whether the admin API still accepts the shipped default credentials
    /// (the UI warns while it does).
    default_credentials: bool,
}

/// `GET /api/status` — gateway version plus route and policy counts.
///
/// ```json
/// { "version": "0.1.0", "routes": 3, "policies": 2, "default_credentials": false }
/// ```
#[utoipa::path(
    get,
    path = "/api/status",
    tag = "operations",
    operation_id = "get_status",
    summary = "Get gateway status",
    responses((status = 200, description = "Version plus route and policy counts.", body = StatusSummary)),
)]
async fn status(State(state): State<Arc<SharedState>>) -> impl IntoResponse {
    let routes = state.routes.read().await;
    let gw = state.gateway.read().await;
    Json(StatusSummary {
        version: env!("CARGO_PKG_VERSION"),
        routes: routes.len(),
        policies: gw.policies.len(),
        // The UI warns while the guessable shipped default is in use.
        default_credentials: state
            .system
            .admin
            .as_ref()
            .is_some_and(|a| a.uses_default_credentials()),
    })
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
#[utoipa::path(
    get,
    path = "/api/config/export",
    tag = "operations",
    summary = "Export the gateway config as YAML",
    responses(
        (status = 200, description = "The live gateway configuration as `gateway.yaml`, `${ENV}` placeholders unresolved.", content_type = "text/yaml; charset=utf-8", body = GatewayConfig),
        (status = 500, description = "The configuration could not be serialized.", body = ErrorResponse),
    ),
)]
async fn export_config(State(state): State<Arc<SharedState>>) -> impl IntoResponse {
    let gw = state.gateway.read().await;
    match crate::yaml::to_string(&*gw) {
        Ok(yaml) => (
            StatusCode::OK,
            [("content-type", "text/yaml; charset=utf-8")],
            yaml,
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                error: format!("failed to serialize config: {}", e),
            }),
        )
            .into_response(),
    }
}

/// `GET /metrics` — renders the shared gateway registry (per-route and
/// per-node counters/histograms recorded by the data plane) in Prometheus
/// text exposition format, served as `text/plain; charset=utf-8`.
#[utoipa::path(
    get,
    path = "/metrics",
    tag = "operations",
    summary = "Get Prometheus metrics",
    responses((status = 200, description = "Per-route and per-node counters/histograms in Prometheus text exposition format.", content_type = "text/plain; charset=utf-8", body = String)),
)]
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
#[utoipa::path(
    post,
    path = "/api/config/reload",
    tag = "operations",
    summary = "Reload the gateway config from disk",
    responses(
        (status = 200, description = "Reloaded (`status: reloaded`).", body = StatusResponse),
        (status = 500, description = "No config path is set, or the file failed to parse/validate/compile; the running config is unchanged.", body = ErrorResponse),
    ),
)]
async fn reload_config(State(state): State<Arc<SharedState>>) -> impl IntoResponse {
    match state.reload_from_disk().await {
        Ok(_) => (
            StatusCode::OK,
            Json(StatusResponse {
                status: "reloaded".to_string(),
            }),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse { error: e }),
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
        state_with("{}")
    }

    fn state_with(system_yaml: &str) -> Arc<SharedState> {
        let system: SystemConfig = crate::yaml::from_str(system_yaml).unwrap();
        let gateway: GatewayConfig = crate::yaml::from_str(
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
        let resp = axum::Router::from(router())
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

    /// The admin listener starts before `server::start_server` seeds
    /// `state.acme`, so a config that asks for managed certificates must read
    /// as not-ready until the runtime exists — otherwise the probe reports
    /// ready while nothing has been issued and no listener has bound.
    #[tokio::test]
    async fn readyz_is_503_before_the_acme_runtime_is_seeded() {
        let s = state_with(
            "acme:
  terms_of_service_agreed: true
  directory_url: https://127.0.0.1:1/directory
tls:
  acme:
    domains: [p.example.com]
",
        );
        assert!(s.acme_expected);
        let (status, body) = readyz_status(s.clone()).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body["reason"], "acme starting");
        assert_eq!(body["acme"]["placeholder"].as_array().unwrap().len(), 0);

        s.acme.store(Some(crate::acme::testing::issued_runtime(&[
            "p.example.com",
        ])));
        let (status, _) = readyz_status(s).await;
        assert_eq!(status, StatusCode::OK);

        // A file-only TLS config never expects ACME.
        let file_tls = state_with(
            "tls:
  cert_path: /tmp/c.pem
  key_path: /tmp/k.pem
",
        );
        assert!(!file_tls.acme_expected);
        assert_eq!(readyz_status(file_tls).await.0, StatusCode::OK);
    }
}

#[cfg(test)]
mod chart_version_tests {
    /// The Helm chart is versioned in lockstep with the crate: Chart.yaml's
    /// `version` and `appVersion` must equal CARGO_PKG_VERSION, and the
    /// release chore commit bumps all three (helm.yml refuses to publish a
    /// chart whose version differs from the tag).
    #[test]
    fn helm_chart_version_tracks_the_crate_version() {
        let chart = include_str!("../../charts/featherbit-gateway/Chart.yaml");
        let version = env!("CARGO_PKG_VERSION");
        let has = |line: &str| chart.lines().any(|l| l.trim_end() == line);
        assert!(
            has(&format!("version: {version}")),
            "charts/featherbit-gateway/Chart.yaml `version` must be {version}"
        );
        assert!(
            has(&format!("appVersion: \"{version}\"")),
            "charts/featherbit-gateway/Chart.yaml `appVersion` must be \"{version}\""
        );
    }
}
