//! Automatic certificates via ACME (RFC 8555) with the TLS-ALPN-01 challenge
//! (RFC 8737).
//!
//! This module *produces* certificates; `server::tls` only consumes them. The
//! contract between the two is [`ManagedCerts`] — an atomically swappable map
//! from [`CertId`] to the current [`ManagedCert`] — plus the
//! `challenge::ChallengeSolver` the TLS resolver asks when a ClientHello
//! carries ALPN `acme-tls/1`. Renewals swap a map entry; they never rebuild
//! the rustls `ServerConfig`.

use std::collections::HashMap;
use std::sync::Arc;

use arc_swap::ArcSwap;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::sign::CertifiedKey;
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use crate::config::{AcmeConfig, AcmeStorageConfig, TlsConfig};
use crate::metrics::GatewayMetrics;
use crate::stores::StoreRegistry;
use challenge::TlsAlpnSolver;
use manager::{ManagedSlot, Manager, ManagerConfig};
use storage::CertStorage;

pub mod challenge;
pub mod client;
#[cfg(test)]
mod live_tests;
pub mod manager;
pub mod metrics;
pub mod order;
pub mod storage;

#[derive(Debug, thiserror::Error)]
pub enum AcmeError {
    #[error("acme config: {0}")]
    Config(String),
    #[error("acme storage: {0}")]
    Storage(String),
    #[error("acme protocol: {0}")]
    Protocol(String),
    #[error("acme certificate: {0}")]
    Certificate(String),
    #[error("acme crypto: {0}")]
    Crypto(String),
}

/// Stable identity of one managed certificate: its normalized domain set.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CertId(String);

impl CertId {
    /// Normalizes (lowercase, sorted, deduplicated) and validates the domains;
    /// returns the id and the normalized list.
    pub fn from_domains(domains: &[String]) -> Result<(CertId, Vec<String>), AcmeError> {
        let mut norm = domains
            .iter()
            .map(|d| crate::config::normalize_domain(d).map_err(AcmeError::Config))
            .collect::<Result<Vec<_>, _>>()?;
        norm.sort();
        norm.dedup();
        if norm.is_empty() {
            return Err(AcmeError::Config(
                "a certificate needs at least one domain".into(),
            ));
        }
        Ok((CertId(norm.join(",")), norm))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for CertId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum CertState {
    /// Self-signed stand-in served until the first issuance succeeds.
    Placeholder,
    Issued,
    Renewing,
    /// Last attempt failed; the previous cert (or placeholder) keeps serving.
    Failed,
}

impl CertState {
    pub const ALL: [CertState; 4] = [
        CertState::Placeholder,
        CertState::Issued,
        CertState::Renewing,
        CertState::Failed,
    ];

    pub fn as_str(&self) -> &'static str {
        match self {
            CertState::Placeholder => "placeholder",
            CertState::Issued => "issued",
            CertState::Renewing => "renewing",
            CertState::Failed => "failed",
        }
    }
}

/// Operator-facing facts about a managed certificate (never the key).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CertMeta {
    pub not_before: i64,
    pub not_after: i64,
    pub issuer: String,
    pub serial: String,
    #[serde(default)]
    pub next_renewal_at: Option<i64>,
    #[serde(default)]
    pub last_attempt_at: Option<i64>,
    #[serde(default)]
    pub last_error: Option<String>,
}

/// The current certificate for one [`CertId`], as served by the TLS resolver.
#[derive(Debug, Clone)]
pub struct ManagedCert {
    pub key: Arc<CertifiedKey>,
    pub leaf_der: Vec<u8>,
    pub state: CertState,
    pub meta: CertMeta,
    pub domains: Vec<String>,
}

/// Keyed by `CertId::as_str()`.
pub type ManagedCerts = Arc<ArcSwap<HashMap<String, ManagedCert>>>;

pub fn new_managed_certs() -> ManagedCerts {
    Arc::new(ArcSwap::from_pointee(HashMap::new()))
}

/// Replaces (or inserts) the entry for `id`. New TLS connections see it on
/// their next `resolve`; in-flight ones are unaffected.
pub fn publish(certs: &ManagedCerts, id: &CertId, cert: ManagedCert) {
    let mut map: HashMap<String, ManagedCert> = (**certs.load()).clone();
    map.insert(id.as_str().to_string(), cert);
    certs.store(Arc::new(map));
}

/// Mutates the entry for `id` in place (no-op when absent).
pub fn update(certs: &ManagedCerts, id: &CertId, f: impl FnOnce(&mut ManagedCert)) {
    let mut map: HashMap<String, ManagedCert> = (**certs.load()).clone();
    if let Some(c) = map.get_mut(id.as_str()) {
        f(c);
        certs.store(Arc::new(map));
    }
}

/// What storage persists per certificate.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StoredCert {
    pub chain_pem: String,
    pub key_pem: String,
    pub issued_at: i64,
}

pub fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn provider() -> rustls::crypto::CryptoProvider {
    rustls::crypto::ring::default_provider()
}

/// A self-signed, 1-hour placeholder (SAN = all domains, CN = the first) served
/// while no real certificate exists yet. Never persisted.
pub fn placeholder_cert(domains: &[String]) -> Result<(Arc<CertifiedKey>, Vec<u8>), AcmeError> {
    let first = domains
        .first()
        .ok_or_else(|| AcmeError::Config("placeholder needs a domain".into()))?;
    let mut params = rcgen::CertificateParams::new(domains.to_vec())
        .map_err(|e| AcmeError::Crypto(e.to_string()))?;
    params
        .distinguished_name
        .push(rcgen::DnType::CommonName, first.as_str());
    let now = time::OffsetDateTime::now_utc();
    params.not_before = now - time::Duration::minutes(5);
    params.not_after = now + time::Duration::hours(1);
    let key = rcgen::KeyPair::generate().map_err(|e| AcmeError::Crypto(e.to_string()))?;
    let cert = params
        .self_signed(&key)
        .map_err(|e| AcmeError::Crypto(e.to_string()))?;
    load_certified_key(&cert.pem(), &key.serialize_pem())
}

/// Parses a PEM chain + PEM key into a rustls [`CertifiedKey`], verifying the
/// key matches the leaf. Returns the leaf DER alongside for metadata parsing.
pub fn load_certified_key(
    chain_pem: &str,
    key_pem: &str,
) -> Result<(Arc<CertifiedKey>, Vec<u8>), AcmeError> {
    let chain: Vec<CertificateDer<'static>> = CertificateDer::pem_slice_iter(chain_pem.as_bytes())
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| AcmeError::Certificate(format!("chain PEM: {e}")))?;
    if chain.is_empty() {
        return Err(AcmeError::Certificate(
            "chain PEM contains no certificates".into(),
        ));
    }
    let key = PrivateKeyDer::from_pem_slice(key_pem.as_bytes())
        .map_err(|e| AcmeError::Certificate(format!("key PEM: {e}")))?;
    let leaf = chain[0].as_ref().to_vec();
    let ck = CertifiedKey::from_der(chain, key, &provider())
        .map_err(|e| AcmeError::Certificate(format!("certificate/key: {e}")))?;
    ck.keys_match().map_err(|e| {
        AcmeError::Certificate(format!("private key does not match certificate: {e}"))
    })?;
    Ok((Arc::new(ck), leaf))
}

fn parse_leaf(leaf_der: &[u8]) -> Result<x509_parser::prelude::X509Certificate<'_>, AcmeError> {
    use x509_parser::prelude::*;
    X509Certificate::from_der(leaf_der)
        .map(|(_, c)| c)
        .map_err(|e| AcmeError::Certificate(format!("leaf DER: {e}")))
}

pub fn parse_cert_meta(leaf_der: &[u8]) -> Result<CertMeta, AcmeError> {
    let cert = parse_leaf(leaf_der)?;
    Ok(CertMeta {
        not_before: cert.validity().not_before.timestamp(),
        not_after: cert.validity().not_after.timestamp(),
        issuer: cert.issuer().to_string(),
        serial: cert.raw_serial_as_string(),
        next_renewal_at: None,
        last_attempt_at: None,
        last_error: None,
    })
}

/// DNS SANs of the leaf, lowercased.
pub fn leaf_dns_sans(leaf_der: &[u8]) -> Result<Vec<String>, AcmeError> {
    use x509_parser::prelude::*;
    let cert = parse_leaf(leaf_der)?;
    let mut out = Vec::new();
    if let Ok(Some(san)) = cert.subject_alternative_name() {
        for name in &san.value.general_names {
            if let GeneralName::DNSName(d) = name {
                out.push(d.to_ascii_lowercase());
            }
        }
    }
    Ok(out)
}

/// The leaf's SubjectPublicKeyInfo DER (for "does this chain belong to my key").
pub fn leaf_spki(leaf_der: &[u8]) -> Result<Vec<u8>, AcmeError> {
    let cert = parse_leaf(leaf_der)?;
    Ok(cert.tbs_certificate.subject_pki.raw.to_vec())
}

/// Everything the rest of the process needs from ACME once started.
pub struct AcmeRuntime {
    pub certs: ManagedCerts,
    pub solver: Arc<TlsAlpnSolver>,
    pub manager: Arc<Manager>,
    pub storage_label: String,
}

impl AcmeRuntime {
    pub fn hooks(&self) -> crate::server::tls::AcmeHooks {
        crate::server::tls::AcmeHooks {
            certs: self.certs.clone(),
            solver: self.solver.clone(),
        }
    }

    /// Ids of managed certs still serving a placeholder (drives `/readyz`).
    pub fn placeholder_ids(&self) -> Vec<String> {
        let mut v: Vec<String> = self
            .certs
            .load()
            .iter()
            .filter(|(_, c)| c.state == CertState::Placeholder)
            .map(|(id, _)| id.clone())
            .collect();
        v.sort();
        v
    }
}

pub fn build_storage(
    cfg: &AcmeConfig,
    stores: &StoreRegistry,
) -> Result<Arc<dyn CertStorage>, AcmeError> {
    match &cfg.storage {
        AcmeStorageConfig::Filesystem { dir } => Ok(Arc::new(storage::fs::FsCertStorage::new(dir))),
        #[cfg(feature = "redis-store")]
        AcmeStorageConfig::Store {
            store,
            encryption_key,
        } => {
            let client = stores.client(store).map_err(AcmeError::Config)?;
            Ok(Arc::new(storage::redis::RedisCertStorage::new(
                client,
                encryption_key,
            )))
        }
        #[cfg(not(feature = "redis-store"))]
        AcmeStorageConfig::Store { .. } => {
            let _ = stores;
            Err(AcmeError::Config(
                "acme.storage.type: store needs the redis-store feature".into(),
            ))
        }
    }
}

/// Builds storage, seeds [`ManagedCerts`] from storage (or placeholders), and
/// spawns the renewal manager. The CA is **not** contacted here — the manager
/// connects lazily — so a CA outage never blocks the listener from starting.
pub async fn start(
    cfg: &AcmeConfig,
    tls: &TlsConfig,
    stores: &StoreRegistry,
    metrics: &GatewayMetrics,
) -> Result<Arc<AcmeRuntime>, AcmeError> {
    let storage = build_storage(cfg, stores)?;
    let solver = TlsAlpnSolver::new(storage.clone());
    let certs = new_managed_certs();
    let acme_metrics = metrics::AcmeMetrics::register(&metrics.registry)
        .map_err(|e| AcmeError::Config(format!("acme metrics: {e}")))?;
    let now = now_unix();

    let mut slots = Vec::new();
    for domains in tls.managed_domains() {
        let (id, domains) = CertId::from_domains(&domains)?;
        if slots.iter().any(|s: &ManagedSlot| s.id == id) {
            continue; // two slots with the same domain set share one certificate
        }
        let adopted = match storage.load_cert(&id).await {
            Ok(Some(stored)) => {
                match load_certified_key(&stored.chain_pem, &stored.key_pem)
                    .and_then(|(key, leaf)| parse_cert_meta(&leaf).map(|meta| (key, leaf, meta)))
                {
                    Ok((key, leaf, meta)) if meta.not_after > now => {
                        info!(
                            "acme: loaded stored certificate for {} (expires {})",
                            id, meta.not_after
                        );
                        Some(ManagedCert {
                            key,
                            leaf_der: leaf,
                            state: CertState::Issued,
                            meta,
                            domains: domains.clone(),
                        })
                    }
                    Ok(_) => {
                        warn!(
                            "acme: stored certificate for {} is expired; serving a placeholder",
                            id
                        );
                        None
                    }
                    Err(e) => {
                        warn!("acme: stored certificate for {} is unusable ({}); serving a placeholder", id, e);
                        None
                    }
                }
            }
            Ok(None) => None,
            Err(e) => {
                warn!(
                    "acme: reading stored certificate for {} failed ({}); serving a placeholder",
                    id, e
                );
                None
            }
        };
        let cert = match adopted {
            Some(c) => c,
            None => {
                let (key, leaf) = placeholder_cert(&domains)?;
                warn!("acme: {} has no certificate yet — serving a self-signed placeholder until issuance succeeds", id);
                ManagedCert {
                    key,
                    leaf_der: leaf,
                    state: CertState::Placeholder,
                    meta: CertMeta::default(),
                    domains: domains.clone(),
                }
            }
        };
        // Observed synchronously here (rather than left to the manager's
        // spawned per-slot task) so the seeded state is visible to metrics
        // scrapes and `/readyz` the instant `start()` returns, with no
        // dependency on when that task first gets scheduled.
        let seeded_not_after = if cert.state == CertState::Placeholder {
            0
        } else {
            cert.meta.not_after
        };
        acme_metrics.observe(id.as_str(), cert.state, seeded_not_after);
        publish(&certs, &id, cert);
        slots.push(ManagedSlot { id, domains });
    }

    let manager_cfg = ManagerConfig {
        key_type: order::KeyType::parse(&cfg.key_type)?,
        renew_before: cfg.renew_before_duration().map_err(AcmeError::Config)?,
        ..ManagerConfig::default()
    };
    let factory = Arc::new(client::InstantAcmeFactory::new(
        cfg.clone(),
        storage.clone(),
    ));
    let manager = Manager::new(
        manager_cfg,
        factory,
        storage.clone(),
        solver.clone(),
        certs.clone(),
        slots,
        Some(acme_metrics),
    );
    tokio::spawn(manager.clone().run());

    Ok(Arc::new(AcmeRuntime {
        certs,
        solver,
        manager,
        storage_label: storage.label(),
    }))
}

/// Runtimes for admin/readiness unit tests: a manager that never runs, over a
/// mock CA and a temp filesystem store.
#[cfg(test)]
pub(crate) mod testing {
    use super::*;
    use client::mock::{MockAcmeClient, MockBehavior, MockFactory};

    pub fn runtime_with(certs: ManagedCerts, slots: Vec<ManagedSlot>) -> Arc<AcmeRuntime> {
        let dir = std::env::temp_dir().join(format!(
            "fb_acme_rt_{}_{}",
            std::process::id(),
            uuid::Uuid::new_v4().simple()
        ));
        let storage: Arc<dyn CertStorage> = Arc::new(storage::fs::FsCertStorage::new(dir));
        let solver = TlsAlpnSolver::new(storage.clone());
        let factory = MockFactory::new(MockAcmeClient::new(MockBehavior::default()));
        let manager = Manager::new(
            ManagerConfig::default(),
            factory,
            storage,
            solver.clone(),
            certs.clone(),
            slots,
            None,
        );
        Arc::new(AcmeRuntime {
            certs,
            solver,
            manager,
            storage_label: "filesystem".into(),
        })
    }

    fn seeded(domains: &[&str], state: CertState) -> Arc<AcmeRuntime> {
        let domains: Vec<String> = domains.iter().map(|d| d.to_string()).collect();
        let (id, domains) = CertId::from_domains(&domains).unwrap();
        let certs = new_managed_certs();
        let (key, leaf) = if state == CertState::Placeholder {
            placeholder_cert(&domains).unwrap()
        } else {
            let c = rcgen::generate_simple_self_signed(domains.clone()).unwrap();
            load_certified_key(&c.cert.pem(), &c.signing_key.serialize_pem()).unwrap()
        };
        let meta = if state == CertState::Placeholder {
            CertMeta::default()
        } else {
            parse_cert_meta(&leaf).unwrap()
        };
        publish(
            &certs,
            &id,
            ManagedCert {
                key,
                leaf_der: leaf,
                state,
                meta,
                domains: domains.clone(),
            },
        );
        runtime_with(certs, vec![ManagedSlot { id, domains }])
    }

    pub fn placeholder_runtime(domains: &[&str]) -> Arc<AcmeRuntime> {
        seeded(domains, CertState::Placeholder)
    }

    pub fn issued_runtime(domains: &[&str]) -> Arc<AcmeRuntime> {
        seeded(domains, CertState::Issued)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn cert_id_normalizes_sorts_and_dedups() {
        let (id, domains) =
            CertId::from_domains(&s(&["B.example.com", "a.example.com", "a.EXAMPLE.com"])).unwrap();
        assert_eq!(id.as_str(), "a.example.com,b.example.com");
        assert_eq!(domains, s(&["a.example.com", "b.example.com"]));
        assert!(CertId::from_domains(&[]).is_err());
        assert!(CertId::from_domains(&s(&["*.example.com"])).is_err());
    }

    #[test]
    fn placeholder_is_self_signed_for_domains_and_short_lived() {
        let (key, leaf) = placeholder_cert(&s(&["api.example.com", "www.example.com"])).unwrap();
        assert!(key.end_entity_cert().is_ok());
        let sans = leaf_dns_sans(&leaf).unwrap();
        assert!(sans.contains(&"api.example.com".to_string()));
        let meta = parse_cert_meta(&leaf).unwrap();
        let now = now_unix();
        assert!(
            meta.not_after > now && meta.not_after <= now + 3_700,
            "{:?}",
            meta
        );
        assert!(meta.not_before <= now);
    }

    #[test]
    fn load_certified_key_round_trips_and_rejects_foreign_key() {
        let a = rcgen::generate_simple_self_signed(s(&["a.example.com"])).unwrap();
        let b = rcgen::generate_simple_self_signed(s(&["b.example.com"])).unwrap();
        let (ck, leaf) = load_certified_key(&a.cert.pem(), &a.signing_key.serialize_pem()).unwrap();
        assert_eq!(ck.end_entity_cert().unwrap().as_ref(), leaf.as_slice());
        assert!(!leaf_spki(&leaf).unwrap().is_empty());
        let err = load_certified_key(&a.cert.pem(), &b.signing_key.serialize_pem()).unwrap_err();
        assert!(matches!(err, AcmeError::Certificate(_)), "{err}");
        assert!(load_certified_key("not pem", &a.signing_key.serialize_pem()).is_err());
    }

    #[test]
    fn parse_cert_meta_reads_validity_issuer_serial() {
        let a = rcgen::generate_simple_self_signed(s(&["a.example.com"])).unwrap();
        let meta = parse_cert_meta(a.cert.der()).unwrap();
        assert!(meta.not_after > meta.not_before);
        assert!(!meta.serial.is_empty());
        assert!(meta.issuer.contains("rcgen"), "{}", meta.issuer);
    }

    #[tokio::test]
    async fn start_seeds_placeholders_then_reuses_a_stored_cert() {
        let dir = std::env::temp_dir().join(format!("fb_acme_start_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        // directory_url pins to a closed local port: start() spawns the manager,
        // whose placeholder slot is immediately due, so without this the test
        // would have the InstantAcmeFactory dial the real Let's Encrypt
        // production directory in the background. Both start() calls below
        // share this cfg, so one pin covers both.
        let system: crate::config::SystemConfig = serde_yaml::from_str(&format!(
            "acme:\n  terms_of_service_agreed: true\n  directory_url: https://127.0.0.1:1/directory\n  storage:\n    type: filesystem\n    dir: {}\ntls:\n  acme:\n    domains: [s.example.com]\n",
            dir.display().to_string().replace('\\', "/")
        ))
        .unwrap();
        let cfg = system.acme.as_ref().unwrap();
        let tls = system.tls.as_ref().unwrap();
        let stores = crate::stores::StoreRegistry::default();
        let metrics = crate::metrics::GatewayMetrics::new();

        let rt = start(cfg, tls, &stores, &metrics).await.unwrap();
        assert_eq!(rt.placeholder_ids(), vec!["s.example.com".to_string()]);
        assert_eq!(rt.storage_label, "filesystem");
        assert!(metrics.render().contains(
            "featherbit_acme_cert_state{cert_id=\"s.example.com\",state=\"placeholder\"} 1"
        ));

        // A valid stored cert is adopted at start (no placeholder, no order).
        let issued = rcgen::generate_simple_self_signed(vec!["s.example.com".to_string()]).unwrap();
        let storage = build_storage(cfg, &stores).unwrap();
        let (id, _) = CertId::from_domains(&["s.example.com".into()]).unwrap();
        storage
            .save_cert(
                &id,
                &StoredCert {
                    chain_pem: issued.cert.pem(),
                    key_pem: issued.signing_key.serialize_pem(),
                    issued_at: now_unix(),
                },
            )
            .await
            .unwrap();
        let rt2 = start(cfg, tls, &stores, &crate::metrics::GatewayMetrics::new())
            .await
            .unwrap();
        assert!(rt2.placeholder_ids().is_empty());
        assert_eq!(
            rt2.certs.load().get("s.example.com").unwrap().state,
            CertState::Issued
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn publish_and_update_swap_map_entries() {
        let certs = new_managed_certs();
        let (id, domains) = CertId::from_domains(&s(&["a.example.com"])).unwrap();
        let (key, leaf) = placeholder_cert(&domains).unwrap();
        publish(
            &certs,
            &id,
            ManagedCert {
                key,
                leaf_der: leaf,
                state: CertState::Placeholder,
                meta: CertMeta::default(),
                domains,
            },
        );
        assert_eq!(
            certs.load().get(id.as_str()).unwrap().state,
            CertState::Placeholder
        );
        update(&certs, &id, |c| c.state = CertState::Failed);
        assert_eq!(
            certs.load().get(id.as_str()).unwrap().state,
            CertState::Failed
        );
        assert_eq!(CertState::Failed.as_str(), "failed");
    }
}
