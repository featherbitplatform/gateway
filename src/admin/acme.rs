//! Admin surface for ACME-managed certificates: read-only status plus a
//! "renew now" nudge. Configuration stays in `system.yaml` (restart-gated like
//! every TLS setting), so there is deliberately no CRUD here. Responses never
//! include key material — only what `CertMeta` records.

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use super::openapi::ErrorResponse;
use crate::acme::manager::RenewOutcome;
use crate::acme::CertState;
use crate::state::SharedState;

/// Builds the router for the `/api/acme` endpoints.
pub fn router() -> OpenApiRouter<Arc<SharedState>> {
    OpenApiRouter::new()
        .routes(routes!(list_certs))
        .routes(routes!(renew_cert))
}

/// Body of `GET /api/acme/certs`.
#[derive(Debug, Serialize, ToSchema)]
struct AcmeCertList {
    /// Whether `acme:` is configured in `system.yaml`.
    enabled: bool,
    /// Where certificates are persisted, e.g. `filesystem`; omitted when
    /// `enabled` is false.
    #[serde(skip_serializing_if = "Option::is_none")]
    storage: Option<String>,
    /// Every managed certificate, sorted by id; empty when disabled.
    certs: Vec<AcmeCertStatus>,
}

/// One managed certificate. Never includes key material.
#[derive(Debug, Serialize, ToSchema)]
struct AcmeCertStatus {
    /// Certificate id: its normalized (lowercased, sorted, deduplicated)
    /// domain list, comma-separated.
    id: String,
    /// Domains the certificate covers.
    domains: Vec<String>,
    /// Lifecycle state.
    state: CertState,
    /// Validity start, Unix seconds; `0` for a placeholder.
    not_before: i64,
    /// Validity end, Unix seconds; `0` for a placeholder.
    not_after: i64,
    /// Issuer distinguished name.
    issuer: String,
    /// Serial number, colon-separated hex.
    serial: String,
    /// When the next renewal is due, Unix seconds.
    next_renewal_at: Option<i64>,
    /// When issuance/renewal was last attempted, Unix seconds.
    last_attempt_at: Option<i64>,
    /// Why the last attempt failed, if it did.
    last_error: Option<String>,
}

/// Query of `POST /api/acme/certs/{id}/renew`.
#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
struct AcmeRenewQuery {
    /// `true` or `1` renews even when the certificate is not due yet.
    force: Option<String>,
}

/// Outcome of `POST /api/acme/certs/{id}/renew`.
#[derive(Debug, Serialize, ToSchema)]
struct AcmeRenewResult {
    /// Whether a renewal was scheduled.
    scheduled: bool,
    /// Why not (`not_due`); omitted when scheduled.
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<String>,
}

/// `GET /api/acme/certs` — every managed certificate with state and metadata.
/// `{"enabled": false, "certs": []}` when `acme:` is not configured, so the UI
/// can tell "not configured" from "nothing managed" without a status-code guess.
#[utoipa::path(
    get,
    path = "/api/acme/certs",
    tag = "acme",
    summary = "List managed certificates",
    responses((status = 200, description = "Managed certificates (no key material); `enabled: false` with no certs when ACME is not configured.", body = AcmeCertList)),
)]
async fn list_certs(State(state): State<Arc<SharedState>>) -> impl IntoResponse {
    let Some(rt) = state.acme.load_full() else {
        return Json(AcmeCertList {
            enabled: false,
            storage: None,
            certs: Vec::new(),
        });
    };
    let map = rt.certs.load();
    let mut certs: Vec<AcmeCertStatus> = rt
        .manager
        .slots()
        .iter()
        .filter_map(|slot| map.get(slot.id.as_str()).map(|c| (slot, c)))
        .map(|(slot, c)| {
            // A placeholder's `meta` carries the real validity of the
            // self-signed stand-in (the manager needs it to know when to
            // re-mint it), but the API contract is that a placeholder has no
            // certificate dates: `0`, matching the expiry gauge and letting
            // the UI render "—" instead of an hour-away expiry in red.
            let placeholder = c.state == crate::acme::CertState::Placeholder;
            let (not_before, not_after) = if placeholder {
                (0, 0)
            } else {
                (c.meta.not_before, c.meta.not_after)
            };
            AcmeCertStatus {
                id: slot.id.as_str().to_string(),
                domains: c.domains.clone(),
                state: if rt.manager.in_flight(slot.id.as_str())
                    && c.state != CertState::Placeholder
                {
                    CertState::Renewing
                } else {
                    c.state
                },
                not_before,
                not_after,
                issuer: c.meta.issuer.clone(),
                serial: c.meta.serial.clone(),
                next_renewal_at: c.meta.next_renewal_at,
                last_attempt_at: c.meta.last_attempt_at,
                last_error: c.meta.last_error.clone(),
            }
        })
        .collect();
    certs.sort_by(|a, b| a.id.cmp(&b.id));
    Json(AcmeCertList {
        enabled: true,
        storage: Some(rt.storage_label.clone()),
        certs,
    })
}

/// `POST /api/acme/certs/{id}/renew[?force=true]` — nudges the renewal manager.
///
/// Any spelling of the certificate's domain set is accepted as `id`
/// (case, order and duplicates do not matter).
#[utoipa::path(
    post,
    path = "/api/acme/certs/{id}/renew",
    tag = "acme",
    summary = "Renew a managed certificate",
    params(
        ("id" = String, Path, description = "Certificate id: its comma-separated domain list, in any order or case."),
        AcmeRenewQuery,
    ),
    responses(
        (status = 200, description = "Not due, nothing scheduled (`scheduled: false`, `reason: not_due`); pass `force=true` to renew anyway.", body = AcmeRenewResult),
        (status = 202, description = "Renewal scheduled (`scheduled: true`).", body = AcmeRenewResult),
        (status = 404, description = "No managed certificate with that id.", body = ErrorResponse),
        (status = 409, description = "A renewal is already in progress (`error: in_progress`).", body = ErrorResponse),
        (status = 501, description = "ACME is not configured in `system.yaml`.", body = ErrorResponse),
    ),
)]
async fn renew_cert(
    State(state): State<Arc<SharedState>>,
    Path(id): Path<String>,
    Query(params): Query<AcmeRenewQuery>,
) -> impl IntoResponse {
    let Some(rt) = state.acme.load_full() else {
        return (
            StatusCode::NOT_IMPLEMENTED,
            Json(serde_json::json!({"error": "acme is not configured"})),
        )
            .into_response();
    };
    let force = params.force.is_some_and(|v| v == "true" || v == "1");
    // A cert id *is* its normalized domain set, so accept any spelling of it:
    // `CertId::from_domains` lowercases, sorts and dedups, matching what the
    // manager keyed the slot under. An id that is not a valid domain list can
    // never name a managed certificate.
    let Ok((id, _)) =
        crate::acme::CertId::from_domains(&id.split(',').map(String::from).collect::<Vec<_>>())
    else {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "not_found"})),
        )
            .into_response();
    };
    match rt.manager.renew_now(id.as_str(), force) {
        RenewOutcome::Scheduled => (
            StatusCode::ACCEPTED,
            Json(AcmeRenewResult {
                scheduled: true,
                reason: None,
            }),
        )
            .into_response(),
        RenewOutcome::NotDue => (
            StatusCode::OK,
            Json(AcmeRenewResult {
                scheduled: false,
                reason: Some("not_due".to_string()),
            }),
        )
            .into_response(),
        RenewOutcome::InProgress => (
            StatusCode::CONFLICT,
            Json(serde_json::json!({"error": "in_progress"})),
        )
            .into_response(),
        RenewOutcome::Unknown => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "not_found"})),
        )
            .into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{GatewayConfig, SystemConfig};
    use crate::config_store::FileConfigStore;
    use axum::body::Body;
    use axum::http::{Method, Request};
    use tower::ServiceExt;

    fn state() -> Arc<SharedState> {
        let system: SystemConfig = crate::yaml::from_str("{}").unwrap();
        let gateway: GatewayConfig = crate::yaml::from_str("{}").unwrap();
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

    async fn call(
        state: Arc<SharedState>,
        method: Method,
        uri: &str,
    ) -> (StatusCode, serde_json::Value) {
        let resp = axum::Router::from(router())
            .with_state(state)
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(uri)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
        )
    }

    #[tokio::test]
    async fn list_reports_disabled_when_acme_is_absent() {
        let (status, body) = call(state(), Method::GET, "/api/acme/certs").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["enabled"], false);
        assert_eq!(body["certs"].as_array().unwrap().len(), 0);
        let (status, _) = call(state(), Method::POST, "/api/acme/certs/x/renew").await;
        assert_eq!(status, StatusCode::NOT_IMPLEMENTED);
    }

    #[tokio::test]
    async fn list_shows_managed_certs_without_key_material() {
        let s = state();
        s.acme.store(Some(crate::acme::testing::issued_runtime(&[
            "B.example.com",
            "a.example.com",
        ])));
        let (status, body) = call(s, Method::GET, "/api/acme/certs").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["enabled"], true);
        assert_eq!(body["storage"], "filesystem");
        let cert = &body["certs"][0];
        assert_eq!(cert["id"], "a.example.com,b.example.com");
        assert_eq!(
            cert["domains"],
            serde_json::json!(["a.example.com", "b.example.com"])
        );
        assert_eq!(cert["state"], "issued");
        assert!(cert["not_after"].as_i64().unwrap() > 0);
        assert!(cert.get("key_pem").is_none() && cert.get("chain_pem").is_none());
        assert!(!body.to_string().contains("PRIVATE KEY"));
    }

    #[tokio::test]
    async fn renew_semantics() {
        let s = state();
        s.acme.store(Some(crate::acme::testing::issued_runtime(&[
            "a.example.com",
        ])));
        let (status, _) = call(s.clone(), Method::POST, "/api/acme/certs/nope/renew").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        // Freshly issued, long-lived: not due.
        let (status, body) = call(
            s.clone(),
            Method::POST,
            "/api/acme/certs/a.example.com/renew",
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["scheduled"], false);
        assert_eq!(body["reason"], "not_due");
        let (status, body) = call(
            s.clone(),
            Method::POST,
            "/api/acme/certs/a.example.com/renew?force=true",
        )
        .await;
        assert_eq!(status, StatusCode::ACCEPTED);
        assert_eq!(body["scheduled"], true);
        // A placeholder is always due.
        let p = state();
        p.acme
            .store(Some(crate::acme::testing::placeholder_runtime(&[
                "p.example.com",
            ])));
        let (status, _) = call(p, Method::POST, "/api/acme/certs/p.example.com/renew").await;
        assert_eq!(status, StatusCode::ACCEPTED);
    }

    /// The documented contract (`ui/src/types/index.ts`, the TLS guide, and the
    /// `not_after` gauge) is that a placeholder has no certificate dates. The
    /// manager needs a real `meta.not_after` internally to know when to re-mint
    /// the stand-in, so the zeroing happens here at the API boundary.
    #[tokio::test]
    async fn placeholder_certs_report_zero_validity_dates() {
        let s = state();
        s.acme
            .store(Some(crate::acme::testing::placeholder_runtime(&[
                "p.example.com",
            ])));
        let (status, body) = call(s.clone(), Method::GET, "/api/acme/certs").await;
        assert_eq!(status, StatusCode::OK);
        let cert = &body["certs"][0];
        assert_eq!(cert["state"], "placeholder");
        assert_eq!(cert["not_after"], 0);
        assert_eq!(cert["not_before"], 0);
        // The runtime itself still holds real dates — that is what drives the
        // hourly re-mint.
        let rt = s.acme.load_full().unwrap();
        assert!(rt.certs.load().get("p.example.com").unwrap().meta.not_after > 0);
    }

    /// A cert id is its normalized domain set, so any spelling of that set has
    /// to reach the same certificate — the UI and hand-written curl calls both
    /// pass ids around verbatim from wherever the domains were typed.
    #[tokio::test]
    async fn renew_normalizes_the_path_id_to_the_cert_id() {
        let s = state();
        s.acme.store(Some(crate::acme::testing::issued_runtime(&[
            "a.example.com",
            "b.example.com",
        ])));
        // Reversed order, mixed case, a duplicate: still the same certificate.
        let (status, body) = call(
            s.clone(),
            Method::POST,
            "/api/acme/certs/B.example.com,a.example.com,A.EXAMPLE.com/renew?force=true",
        )
        .await;
        assert_eq!(status, StatusCode::ACCEPTED);
        assert_eq!(body["scheduled"], true);
        // Not a domain list at all ⇒ still a 404, not a 500.
        let (status, _) = call(s, Method::POST, "/api/acme/certs/*.example.com/renew").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }
}
