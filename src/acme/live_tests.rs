//! End-to-end issuance against Pebble (Let's Encrypt's test CA). Gated on
//! `FEATHERBIT_TEST_PEBBLE_URL`; see `dev/pebble/` for running it locally and
//! the `acme-live` CI job. Pebble's validator connects to
//! `<domain>:<FEATHERBIT_TEST_ACME_PORT>` (its `tlsPort`), so the data-plane
//! listener binds that exact port.

use std::sync::Arc;
use std::time::Duration;

use crate::acme::CertState;
use crate::config::{GatewayConfig, SystemConfig};
use crate::config_store::FileConfigStore;
use crate::state::SharedState;

struct Env {
    dir_url: String,
    ca_path: String,
    domain: String,
    port: u16,
}

fn env() -> Option<Env> {
    let Ok(dir_url) = std::env::var("FEATHERBIT_TEST_PEBBLE_URL") else {
        eprintln!("skipping acme live test: FEATHERBIT_TEST_PEBBLE_URL not set");
        return None;
    };
    Some(Env {
        dir_url,
        ca_path: std::env::var("FEATHERBIT_TEST_PEBBLE_CA")
            .expect("FEATHERBIT_TEST_PEBBLE_CA (path to pebble.minica.pem)"),
        domain: std::env::var("FEATHERBIT_TEST_ACME_DOMAIN").unwrap_or_else(|_| "localhost".into()),
        port: std::env::var("FEATHERBIT_TEST_ACME_PORT")
            .ok()
            .and_then(|p| p.parse().ok())
            .unwrap_or(18443),
    })
}

fn system_yaml(e: &Env, storage_dir: &std::path::Path) -> String {
    format!(
        r#"
listener: {{ bind: "127.0.0.1", port: {port} }}
tls:
  acme: {{ domains: ["{domain}"] }}
acme:
  directory_url: "{dir}"
  directory_ca_path: "{ca}"
  terms_of_service_agreed: true
  contact: ["mailto:e2e@example.com"]
  renew_before: 30d
  storage: {{ type: filesystem, dir: "{sdir}" }}
"#,
        port = e.port,
        domain = e.domain,
        dir = e.dir_url,
        ca = e.ca_path.replace('\\', "/"),
        sdir = storage_dir.display().to_string().replace('\\', "/"),
    )
}

async fn wait_for(
    certs: &crate::acme::ManagedCerts,
    id: &str,
    pred: impl Fn(&crate::acme::ManagedCert) -> bool,
    what: &str,
) -> crate::acme::ManagedCert {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(90);
    loop {
        if let Some(c) = certs.load().get(id) {
            if pred(c) {
                return c.clone();
            }
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for {what}"
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

#[tokio::test]
async fn pebble_issues_renews_and_restart_reuses_the_stored_cert() {
    let Some(e) = env() else { return };
    let storage_dir = std::env::temp_dir().join(format!("fb_acme_pebble_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&storage_dir);

    let system: SystemConfig = serde_yaml::from_str(&system_yaml(&e, &storage_dir)).unwrap();
    system.validate().unwrap();
    let gateway: GatewayConfig = serde_yaml::from_str("{}").unwrap();
    let state = Arc::new(
        SharedState::new(
            system.clone(),
            gateway,
            None,
            Arc::new(FileConfigStore::new("unused.yaml".into())),
        )
        .unwrap(),
    );
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let server = {
        let state = state.clone();
        let system = system.clone();
        tokio::spawn(async move {
            crate::server::start_server(&system, state, shutdown_rx)
                .await
                .unwrap()
        })
    };

    // The runtime appears once the listener has bound; it starts as a placeholder.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let rt = loop {
        if let Some(rt) = state.acme.load_full() {
            break rt;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "acme runtime never started"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    let id = e.domain.to_ascii_lowercase();
    assert_eq!(rt.placeholder_ids(), vec![id.clone()]);

    // Issuance completes; the served cert is Pebble's.
    let issued = wait_for(
        &rt.certs,
        &id,
        |c| c.state == CertState::Issued,
        "first issuance",
    )
    .await;
    assert!(
        issued.meta.issuer.to_lowercase().contains("pebble"),
        "{}",
        issued.meta.issuer
    );
    assert!(rt.placeholder_ids().is_empty());

    // Not due ⇒ refused; force ⇒ a new serial.
    assert_eq!(
        rt.manager.renew_now(&id, false),
        crate::acme::manager::RenewOutcome::NotDue
    );
    assert_eq!(
        rt.manager.renew_now(&id, true),
        crate::acme::manager::RenewOutcome::Scheduled
    );
    let renewed = wait_for(
        &rt.certs,
        &id,
        |c| c.state == CertState::Issued && c.meta.serial != issued.meta.serial,
        "forced renewal",
    )
    .await;
    assert_ne!(renewed.meta.serial, issued.meta.serial);

    // A restart (new runtime over the same storage) adopts the stored cert: no
    // placeholder, no new order.
    let rt2 = crate::acme::start(
        system.acme.as_ref().unwrap(),
        system.tls.as_ref().unwrap(),
        &state.resources,
        &crate::metrics::GatewayMetrics::new(),
    )
    .await
    .unwrap();
    let adopted = rt2.certs.load().get(&id).cloned().unwrap();
    assert_eq!(adopted.state, CertState::Issued);
    assert_eq!(adopted.meta.serial, renewed.meta.serial);

    let _ = shutdown_tx.send(true);
    let _ = tokio::time::timeout(Duration::from_secs(5), server).await;
    let _ = std::fs::remove_dir_all(&storage_dir);
}
