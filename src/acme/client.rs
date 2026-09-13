//! The ACME protocol boundary.
//!
//! [`AcmeClient`]/[`AcmeOrder`] are the *only* surface the order state machine
//! (`order.rs`) and the scheduler (`manager.rs`) see, so both are unit-tested
//! against [`mock::MockAcmeClient`] with no network. [`InstantAcmeClient`]
//! implements them over `instant-acme` (JWS, directory, account/EAB, orders,
//! ARI). [`AcmeClientFactory`] exists because the CA may be unreachable at
//! startup: the manager connects lazily and retries.

use std::sync::Arc;

use async_trait::async_trait;
use base64::Engine;
use instant_acme::{
    Account, AccountCredentials, AuthorizationStatus, ChallengeType, ExternalAccountKey,
    Identifier, NewAccount, NewOrder, OrderStatus, RetryPolicy,
};
use tracing::{debug, info};

use super::storage::CertStorage;
use super::AcmeError;
use crate::config::AcmeConfig;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingChallenge {
    pub domain: String,
    /// `<token>.<account-key-thumbprint>`; the solver hashes it.
    pub key_auth: String,
}

#[async_trait]
pub trait AcmeOrder: Send {
    /// One entry per authorization still `pending` (already-valid ones are skipped).
    async fn pending_challenges(&mut self) -> Result<Vec<PendingChallenge>, AcmeError>;
    /// Tells the CA the TLS-ALPN-01 challenge for `domain` may be validated now.
    async fn mark_ready(&mut self, domain: &str) -> Result<(), AcmeError>;
    /// Polls until the order is `ready`; `Err` on `invalid` or timeout.
    async fn wait_ready(&mut self) -> Result<(), AcmeError>;
    /// Submits the CSR and downloads the PEM chain.
    async fn finalize(&mut self, csr_der: &[u8]) -> Result<String, AcmeError>;
}

#[async_trait]
pub trait AcmeClient: Send + Sync {
    async fn new_order(&self, domains: &[String]) -> Result<Box<dyn AcmeOrder>, AcmeError>;
    /// RFC 9773 renewal-info window for the certificate, when the CA offers it.
    async fn renewal_window(&self, leaf_der: &[u8]) -> Result<Option<(i64, i64)>, AcmeError>;
}

#[async_trait]
pub trait AcmeClientFactory: Send + Sync {
    async fn connect(&self) -> Result<Arc<dyn AcmeClient>, AcmeError>;
}

/// EAB HMAC keys are handed out base64url; some CAs paste standard base64.
pub fn decode_hmac_key(s: &str) -> Result<Vec<u8>, AcmeError> {
    let s = s.trim();
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(s)
        .or_else(|_| base64::engine::general_purpose::URL_SAFE.decode(s))
        .or_else(|_| base64::engine::general_purpose::STANDARD.decode(s))
        .map_err(|e| AcmeError::Config(format!("acme.eab.hmac_key is not base64: {e}")))
}

fn proto(e: instant_acme::Error) -> AcmeError {
    AcmeError::Protocol(e.to_string())
}

pub struct InstantAcmeFactory {
    cfg: AcmeConfig,
    storage: Arc<dyn CertStorage>,
}

impl InstantAcmeFactory {
    pub fn new(cfg: AcmeConfig, storage: Arc<dyn CertStorage>) -> Self {
        Self { cfg, storage }
    }

    fn builder(&self) -> Result<instant_acme::AccountBuilder, AcmeError> {
        match &self.cfg.directory_ca_path {
            Some(path) => Account::builder_with_root(path).map_err(proto),
            None => Account::builder().map_err(proto),
        }
    }
}

#[async_trait]
impl AcmeClientFactory for InstantAcmeFactory {
    /// Loads the stored account or registers a new one (persisting its credentials).
    async fn connect(&self) -> Result<Arc<dyn AcmeClient>, AcmeError> {
        if let Some(bytes) = self.storage.load_account().await? {
            let creds: AccountCredentials = serde_json::from_slice(&bytes).map_err(|e| {
                AcmeError::Storage(format!("stored account credentials are corrupt: {e}"))
            })?;
            let account = self
                .builder()?
                .from_credentials(creds)
                .await
                .map_err(proto)?;
            debug!("acme: using stored account {}", account.id());
            return Ok(Arc::new(InstantAcmeClient { account }));
        }
        let contacts: Vec<&str> = self.cfg.contact.iter().map(String::as_str).collect();
        let eab = match &self.cfg.eab {
            Some(e) => Some(ExternalAccountKey::new(
                e.key_id.clone(),
                &decode_hmac_key(&e.hmac_key)?,
            )),
            None => None,
        };
        let (account, creds) = self
            .builder()?
            .create(
                &NewAccount {
                    contact: &contacts,
                    terms_of_service_agreed: self.cfg.terms_of_service_agreed,
                    only_return_existing: false,
                },
                self.cfg.directory_url.clone(),
                eab.as_ref(),
            )
            .await
            .map_err(proto)?;
        let bytes = serde_json::to_vec(&creds)
            .map_err(|e| AcmeError::Storage(format!("serialize account credentials: {e}")))?;
        self.storage.save_account(&bytes).await?;
        info!(
            "acme: registered account {} at {}",
            account.id(),
            self.cfg.directory_url
        );
        Ok(Arc::new(InstantAcmeClient { account }))
    }
}

pub struct InstantAcmeClient {
    account: Account,
}

#[async_trait]
impl AcmeClient for InstantAcmeClient {
    async fn new_order(&self, domains: &[String]) -> Result<Box<dyn AcmeOrder>, AcmeError> {
        let identifiers: Vec<Identifier> = domains.iter().cloned().map(Identifier::Dns).collect();
        let order = self
            .account
            .new_order(&NewOrder::new(&identifiers))
            .await
            .map_err(proto)?;
        Ok(Box::new(InstantAcmeOrder { order }))
    }

    async fn renewal_window(&self, leaf_der: &[u8]) -> Result<Option<(i64, i64)>, AcmeError> {
        let der = rustls::pki_types::CertificateDer::from(leaf_der.to_vec());
        let id = match instant_acme::CertificateIdentifier::try_from(&der) {
            Ok(id) => id,
            Err(e) => {
                debug!("acme: no ARI identifier for certificate: {e}");
                return Ok(None);
            }
        };
        match self.account.renewal_info(&id).await {
            Ok((info, _retry_after)) => Ok(Some((
                info.suggested_window.start.unix_timestamp(),
                info.suggested_window.end.unix_timestamp(),
            ))),
            // ARI is advisory: a CA without it (or a transient error) just means
            // "use renew_before".
            Err(e) => {
                debug!("acme: renewal_info unavailable: {e}");
                Ok(None)
            }
        }
    }
}

struct InstantAcmeOrder {
    order: instant_acme::Order,
}

fn dns_name(ident: &Identifier) -> Result<String, AcmeError> {
    match ident {
        Identifier::Dns(d) => Ok(d.clone()),
        other => Err(AcmeError::Protocol(format!(
            "unsupported identifier {other:?}"
        ))),
    }
}

#[async_trait]
impl AcmeOrder for InstantAcmeOrder {
    async fn pending_challenges(&mut self) -> Result<Vec<PendingChallenge>, AcmeError> {
        let mut out = Vec::new();
        let mut authorizations = self.order.authorizations();
        while let Some(result) = authorizations.next().await {
            let mut authz = result.map_err(proto)?;
            let domain = dns_name(authz.identifier().identifier)?;
            match authz.status {
                AuthorizationStatus::Pending => {}
                AuthorizationStatus::Valid => continue,
                other => {
                    return Err(AcmeError::Protocol(format!(
                        "authorization for {domain} is {other:?}"
                    )))
                }
            }
            let challenge = authz.challenge(ChallengeType::TlsAlpn01).ok_or_else(|| {
                AcmeError::Protocol(format!("CA offers no tls-alpn-01 challenge for {domain}"))
            })?;
            out.push(PendingChallenge {
                domain,
                key_auth: challenge.key_authorization().as_str().to_string(),
            });
        }
        Ok(out)
    }

    async fn mark_ready(&mut self, domain: &str) -> Result<(), AcmeError> {
        let mut authorizations = self.order.authorizations();
        while let Some(result) = authorizations.next().await {
            let mut authz = result.map_err(proto)?;
            if dns_name(authz.identifier().identifier)? != domain {
                continue;
            }
            let mut challenge = authz.challenge(ChallengeType::TlsAlpn01).ok_or_else(|| {
                AcmeError::Protocol(format!("CA offers no tls-alpn-01 challenge for {domain}"))
            })?;
            return challenge.set_ready().await.map_err(proto);
        }
        Err(AcmeError::Protocol(format!(
            "order has no authorization for {domain}"
        )))
    }

    async fn wait_ready(&mut self) -> Result<(), AcmeError> {
        let status = self
            .order
            .poll_ready(&RetryPolicy::default())
            .await
            .map_err(proto)?;
        if matches!(status, OrderStatus::Ready | OrderStatus::Valid) {
            Ok(())
        } else {
            let detail = self
                .order
                .state()
                .error
                .as_ref()
                .map(|p| p.to_string())
                .unwrap_or_default();
            Err(AcmeError::Protocol(format!(
                "order is {status:?}: {detail}"
            )))
        }
    }

    async fn finalize(&mut self, csr_der: &[u8]) -> Result<String, AcmeError> {
        self.order.finalize_csr(csr_der).await.map_err(proto)?;
        self.order
            .poll_certificate(&RetryPolicy::default())
            .await
            .map_err(proto)
    }
}

/// A scripted in-process CA for unit tests: hands out deterministic key
/// authorizations, requires every challenge to be marked ready, and signs the
/// CSR's public key with its own root so `load_certified_key` accepts the
/// chain. `MockBehavior` injects failures.
#[cfg(test)]
pub(crate) mod mock {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum MockStep {
        NewOrder,
        WaitReady,
        Finalize,
    }

    #[derive(Debug, Clone)]
    pub struct MockBehavior {
        pub fail_step: Option<MockStep>,
        pub ari: Option<(i64, i64)>,
        /// Sign a random key instead of the CSR's (verification must reject it).
        pub wrong_key_chain: bool,
        /// Echo back an authorization for an identifier that was never
        /// requested (a hostile/broken CA).
        pub extra_pending_domain: Option<String>,
        pub validity_secs: i64,
    }

    impl Default for MockBehavior {
        fn default() -> Self {
            Self {
                fail_step: None,
                ari: None,
                wrong_key_chain: false,
                extra_pending_domain: None,
                validity_secs: 90 * 86_400,
            }
        }
    }

    pub struct MockAcmeClient {
        ca_params: rcgen::CertificateParams,
        ca_key: rcgen::KeyPair,
        ca_pem: String,
        pub behavior: Mutex<MockBehavior>,
        orders: AtomicUsize,
    }

    impl MockAcmeClient {
        pub fn new(behavior: MockBehavior) -> Arc<Self> {
            let mut ca_params = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap();
            ca_params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
            ca_params
                .distinguished_name
                .push(rcgen::DnType::CommonName, "Mock ACME CA");
            let ca_key = rcgen::KeyPair::generate().unwrap();
            let ca_pem = ca_params.self_signed(&ca_key).unwrap().pem();
            Arc::new(Self {
                ca_params,
                ca_key,
                ca_pem,
                behavior: Mutex::new(behavior),
                orders: AtomicUsize::new(0),
            })
        }

        pub fn orders(&self) -> usize {
            self.orders.load(Ordering::SeqCst)
        }

        pub fn set_behavior(&self, b: MockBehavior) {
            *self.behavior.lock().unwrap() = b;
        }

        fn behavior(&self) -> MockBehavior {
            self.behavior.lock().unwrap().clone()
        }

        fn sign(&self, csr_der: &[u8], domains: &[String]) -> Result<String, AcmeError> {
            let b = self.behavior();
            let csr = rcgen::CertificateSigningRequestParams::from_der(
                &rustls::pki_types::CertificateSigningRequestDer::from(csr_der.to_vec()),
            )
            .map_err(|e| AcmeError::Protocol(format!("mock: bad CSR: {e}")))?;
            let mut params = rcgen::CertificateParams::new(domains.to_vec()).unwrap();
            let now = time::OffsetDateTime::now_utc();
            params.not_before = now - time::Duration::minutes(1);
            params.not_after = now + time::Duration::seconds(b.validity_secs);
            let issuer = rcgen::Issuer::from_params(&self.ca_params, &self.ca_key);
            let leaf = if b.wrong_key_chain {
                let other = rcgen::KeyPair::generate().unwrap();
                params.signed_by(&other, &issuer).unwrap()
            } else {
                // Same params (SANs, validity) but the CSR's public key.
                rcgen::CertificateSigningRequestParams {
                    params,
                    public_key: csr.public_key,
                }
                .signed_by(&issuer)
                .unwrap()
            };
            Ok(format!("{}{}", leaf.pem(), self.ca_pem))
        }
    }

    pub struct MockOrder {
        client: Arc<MockAcmeClient>,
        domains: Vec<String>,
        ready: Vec<String>,
    }

    /// Implemented on `Arc<MockAcmeClient>` (not `MockAcmeClient`) so an order
    /// can call back into the CA; tests always hold the `Arc` `new()` returns.
    #[async_trait]
    impl AcmeClient for Arc<MockAcmeClient> {
        async fn new_order(&self, domains: &[String]) -> Result<Box<dyn AcmeOrder>, AcmeError> {
            self.orders.fetch_add(1, Ordering::SeqCst);
            if self.behavior().fail_step == Some(MockStep::NewOrder) {
                return Err(AcmeError::Protocol(
                    "mock: newOrder rejected (rateLimited)".into(),
                ));
            }
            Ok(Box::new(MockOrder {
                client: self.clone(),
                domains: domains.to_vec(),
                ready: Vec::new(),
            }))
        }

        async fn renewal_window(&self, _leaf_der: &[u8]) -> Result<Option<(i64, i64)>, AcmeError> {
            Ok(self.behavior().ari)
        }
    }

    #[async_trait]
    impl AcmeOrder for MockOrder {
        async fn pending_challenges(&mut self) -> Result<Vec<PendingChallenge>, AcmeError> {
            let mut out: Vec<PendingChallenge> = self
                .domains
                .iter()
                .map(|d| PendingChallenge {
                    domain: d.clone(),
                    key_auth: format!("tok-{d}.mockthumb"),
                })
                .collect();
            if let Some(extra) = self.client.behavior().extra_pending_domain {
                out.push(PendingChallenge {
                    key_auth: format!("tok-{extra}.mockthumb"),
                    domain: extra,
                });
            }
            Ok(out)
        }

        async fn mark_ready(&mut self, domain: &str) -> Result<(), AcmeError> {
            if !self.domains.iter().any(|d| d == domain) {
                return Err(AcmeError::Protocol(format!(
                    "mock: no authorization for {domain}"
                )));
            }
            self.ready.push(domain.to_string());
            Ok(())
        }

        async fn wait_ready(&mut self) -> Result<(), AcmeError> {
            if self.client.behavior().fail_step == Some(MockStep::WaitReady) {
                return Err(AcmeError::Protocol(
                    "mock: order is Invalid: challenge failed".into(),
                ));
            }
            if self.domains.iter().all(|d| self.ready.contains(d)) {
                Ok(())
            } else {
                Err(AcmeError::Protocol(
                    "mock: order is Invalid: authorization pending".into(),
                ))
            }
        }

        async fn finalize(&mut self, csr_der: &[u8]) -> Result<String, AcmeError> {
            if self.client.behavior().fail_step == Some(MockStep::Finalize) {
                return Err(AcmeError::Protocol(
                    "mock: finalize rejected (badCSR)".into(),
                ));
            }
            self.client.sign(csr_der, &self.domains)
        }
    }

    /// Factory returning the same mock every time (or failing `fail_connects` times first).
    pub struct MockFactory {
        pub client: Arc<MockAcmeClient>,
        pub fail_connects: AtomicUsize,
    }

    impl MockFactory {
        pub fn new(client: Arc<MockAcmeClient>) -> Arc<Self> {
            Arc::new(Self {
                client,
                fail_connects: AtomicUsize::new(0),
            })
        }
    }

    #[async_trait]
    impl AcmeClientFactory for MockFactory {
        async fn connect(&self) -> Result<Arc<dyn AcmeClient>, AcmeError> {
            if self.fail_connects.load(Ordering::SeqCst) > 0 {
                self.fail_connects.fetch_sub(1, Ordering::SeqCst);
                return Err(AcmeError::Protocol("mock: directory unreachable".into()));
            }
            Ok(Arc::new(self.client.clone()) as Arc<dyn AcmeClient>)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_hmac_key_accepts_url_safe_and_standard_base64() {
        assert_eq!(decode_hmac_key("AQID").unwrap(), vec![1, 2, 3]);
        assert_eq!(decode_hmac_key("_-8").unwrap(), vec![0xff, 0xef]);
        assert_eq!(decode_hmac_key("/+8=").unwrap(), vec![0xff, 0xef]);
        assert!(decode_hmac_key("not base64!").is_err());
    }

    #[tokio::test]
    async fn mock_client_issues_a_chain_for_the_csr_key() {
        let client = mock::MockAcmeClient::new(mock::MockBehavior::default());
        let mut order = client
            .new_order(&["a.example.com".into(), "b.example.com".into()])
            .await
            .unwrap();
        let pending = order.pending_challenges().await.unwrap();
        assert_eq!(pending.len(), 2);
        assert!(
            order.wait_ready().await.is_err(),
            "not all challenges marked ready"
        );
        for p in &pending {
            order.mark_ready(&p.domain).await.unwrap();
        }
        order.wait_ready().await.unwrap();
        let key = rcgen::KeyPair::generate().unwrap();
        let csr = rcgen::CertificateParams::new(vec![
            "a.example.com".to_string(),
            "b.example.com".to_string(),
        ])
        .unwrap()
        .serialize_request(&key)
        .unwrap();
        let chain = order.finalize(csr.der()).await.unwrap();
        let (ck, leaf) = crate::acme::load_certified_key(&chain, &key.serialize_pem()).unwrap();
        assert!(ck.end_entity_cert().is_ok());
        let mut sans = crate::acme::leaf_dns_sans(&leaf).unwrap();
        sans.sort();
        assert_eq!(
            sans,
            vec!["a.example.com".to_string(), "b.example.com".to_string()]
        );
        assert!(crate::acme::parse_cert_meta(&leaf)
            .unwrap()
            .issuer
            .contains("Mock ACME CA"));
        assert_eq!(client.orders(), 1);
    }

    #[tokio::test]
    async fn mock_client_honors_failure_script_and_ari() {
        let client = mock::MockAcmeClient::new(mock::MockBehavior {
            fail_step: Some(mock::MockStep::Finalize),
            ari: Some((100, 200)),
            ..Default::default()
        });
        let mut order = client.new_order(&["a.example.com".into()]).await.unwrap();
        for p in order.pending_challenges().await.unwrap() {
            order.mark_ready(&p.domain).await.unwrap();
        }
        order.wait_ready().await.unwrap();
        assert!(order.finalize(b"irrelevant").await.is_err());
        assert_eq!(
            client.renewal_window(b"any").await.unwrap(),
            Some((100, 200))
        );
    }
}
