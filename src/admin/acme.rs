//! Admin surface for ACME-managed certificates: read-only status plus a
//! "renew now" nudge. Configuration stays in `system.yaml` (restart-gated like
//! every TLS setting), so there is deliberately no CRUD here. Responses never
//! include key material — only what `CertMeta` records.

use std::collections::HashMap;
use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};

use crate::acme::manager::RenewOutcome;
use crate::state::SharedState;

pub fn router() -> Router<Arc<SharedState>> {
    Router::new()
        .route("/api/acme/certs", get(list_certs))
        .route("/api/acme/certs/{id}/renew", post(renew_cert))
}

/// `GET /api/acme/certs` — every managed certificate with state and metadata.
/// `{"enabled": false, "certs": []}` when `acme:` is not configured, so the UI
/// can tell "not configured" from "nothing managed" without a status-code guess.
async fn list_certs(State(state): State<Arc<SharedState>>) -> impl IntoResponse {
    let Some(rt) = state.acme.load_full() else {
        return Json(serde_json::json!({"enabled": false, "certs": []}));
    };
    let map = rt.certs.load();
    let mut certs: Vec<serde_json::Value> = rt
        .manager
        .slots()
        .iter()
        .filter_map(|slot| map.get(slot.id.as_str()).map(|c| (slot, c)))
        .map(|(slot, c)| {
            serde_json::json!({
                "id": slot.id.as_str(),
                "domains": c.domains,
                "state": if rt.manager.in_flight(slot.id.as_str()) && c.state != crate::acme::CertState::Placeholder {
                    crate::acme::CertState::Renewing
                } else {
                    c.state
                },
                "not_before": c.meta.not_before,
                "not_after": c.meta.not_after,
                "issuer": c.meta.issuer,
                "serial": c.meta.serial,
                "next_renewal_at": c.meta.next_renewal_at,
                "last_attempt_at": c.meta.last_attempt_at,
                "last_error": c.meta.last_error,
            })
        })
        .collect();
    certs.sort_by(|a, b| a["id"].as_str().cmp(&b["id"].as_str()));
    Json(serde_json::json!({
        "enabled": true,
        "storage": rt.storage_label,
        "certs": certs,
    }))
}

/// `POST /api/acme/certs/{id}/renew[?force=true]` — nudges the renewal manager.
async fn renew_cert(
    State(state): State<Arc<SharedState>>,
    Path(id): Path<String>,
    Query(params): Query<HashMap<String, String>>,
) -> impl IntoResponse {
    let Some(rt) = state.acme.load_full() else {
        return (
            StatusCode::NOT_IMPLEMENTED,
            Json(serde_json::json!({"error": "acme is not configured"})),
        );
    };
    let force = params.get("force").is_some_and(|v| v == "true" || v == "1");
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
        );
    };
    match rt.manager.renew_now(id.as_str(), force) {
        RenewOutcome::Scheduled => (
            StatusCode::ACCEPTED,
            Json(serde_json::json!({"scheduled": true})),
        ),
        RenewOutcome::NotDue => (
            StatusCode::OK,
            Json(serde_json::json!({"scheduled": false, "reason": "not_due"})),
        ),
        RenewOutcome::InProgress => (
            StatusCode::CONFLICT,
            Json(serde_json::json!({"error": "in_progress"})),
        ),
        RenewOutcome::Unknown => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "not_found"})),
        ),
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
        let system: SystemConfig = serde_yaml::from_str("{}").unwrap();
        let gateway: GatewayConfig = serde_yaml::from_str("{}").unwrap();
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
        let resp = router()
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
