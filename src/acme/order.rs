//! One certificate issuance, start to finish: fresh key → newOrder → register
//! every TLS-ALPN-01 key authorization with the solver → tell the CA "ready" →
//! wait → CSR → finalize → download → **verify** → hand back a [`StoredCert`].
//! Challenges are cleared on every exit path so a stuck order never leaves a
//! validatable challenge cert behind. Verification runs before anything is
//! persisted: a CA returning garbage never evicts a working certificate.

use rcgen::PublicKeyData;

use super::challenge::TlsAlpnSolver;
use super::client::{AcmeClient, PendingChallenge};
use super::{
    leaf_dns_sans, leaf_spki, load_certified_key, now_unix, parse_cert_meta, AcmeError, StoredCert,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyType {
    EcdsaP256,
    EcdsaP384,
}

impl KeyType {
    pub fn parse(s: &str) -> Result<Self, AcmeError> {
        match s {
            "ecdsa-p256" => Ok(KeyType::EcdsaP256),
            "ecdsa-p384" => Ok(KeyType::EcdsaP384),
            other => Err(AcmeError::Config(format!(
                "key_type '{other}' is not supported (ecdsa-p256 | ecdsa-p384)"
            ))),
        }
    }

    fn alg(self) -> &'static rcgen::SignatureAlgorithm {
        match self {
            KeyType::EcdsaP256 => &rcgen::PKCS_ECDSA_P256_SHA256,
            KeyType::EcdsaP384 => &rcgen::PKCS_ECDSA_P384_SHA384,
        }
    }
}

/// A fresh private key for one issuance (never reused across renewals).
pub fn generate_key(kt: KeyType) -> Result<rcgen::KeyPair, AcmeError> {
    rcgen::KeyPair::generate_for(kt.alg()).map_err(|e| AcmeError::Crypto(e.to_string()))
}

/// DER CSR with SAN = `domains`, CN = the first domain.
pub fn build_csr(domains: &[String], key: &rcgen::KeyPair) -> Result<Vec<u8>, AcmeError> {
    let mut params = rcgen::CertificateParams::new(domains.to_vec())
        .map_err(|e| AcmeError::Crypto(e.to_string()))?;
    if let Some(first) = domains.first() {
        params
            .distinguished_name
            .push(rcgen::DnType::CommonName, first.as_str());
    }
    let csr = params
        .serialize_request(key)
        .map_err(|e| AcmeError::Crypto(e.to_string()))?;
    Ok(csr.der().as_ref().to_vec())
}

/// Clock-skew tolerance for a just-issued `not_before`.
const NOT_BEFORE_SKEW_SECS: i64 = 300;

/// The chain parses, the leaf's public key is `key`, it is valid at `now`
/// (±skew), and its DNS SANs cover every domain.
pub fn verify_chain(
    chain_pem: &str,
    key: &rcgen::KeyPair,
    domains: &[String],
    now: i64,
) -> Result<(), AcmeError> {
    let (_, leaf) = load_certified_key(chain_pem, &key.serialize_pem())?;
    if leaf_spki(&leaf)? != key.subject_public_key_info() {
        return Err(AcmeError::Certificate(
            "leaf public key does not match the generated key".into(),
        ));
    }
    let meta = parse_cert_meta(&leaf)?;
    if meta.not_after <= now {
        return Err(AcmeError::Certificate(format!(
            "issued certificate is already expired (not_after={})",
            meta.not_after
        )));
    }
    if meta.not_before > now + NOT_BEFORE_SKEW_SECS {
        return Err(AcmeError::Certificate(format!(
            "issued certificate is not yet valid (not_before={})",
            meta.not_before
        )));
    }
    let sans = leaf_dns_sans(&leaf)?;
    for d in domains {
        if !sans.iter().any(|s| s == &d.to_ascii_lowercase()) {
            return Err(AcmeError::Certificate(format!(
                "issued certificate lacks SAN {d} (has {sans:?})"
            )));
        }
    }
    Ok(())
}

/// Runs one order. On success the certificate is verified but **not** stored —
/// the caller persists and publishes it.
pub async fn issue(
    client: &dyn AcmeClient,
    solver: &TlsAlpnSolver,
    domains: &[String],
    key_type: KeyType,
) -> Result<StoredCert, AcmeError> {
    let mut registered: Vec<String> = Vec::new();
    let result = run(client, solver, domains, key_type, &mut registered).await;
    for domain in &registered {
        if let Err(e) = solver.clear(domain).await {
            tracing::warn!("acme: failed to clear challenge for {domain}: {e}");
        }
    }
    result
}

async fn run(
    client: &dyn AcmeClient,
    solver: &TlsAlpnSolver,
    domains: &[String],
    key_type: KeyType,
    registered: &mut Vec<String>,
) -> Result<StoredCert, AcmeError> {
    let key = generate_key(key_type)?;
    let mut order = client.new_order(domains).await?;
    let pending: Vec<PendingChallenge> = order.pending_challenges().await?;
    for p in &pending {
        solver.register(&p.domain, &p.key_auth).await?;
        registered.push(p.domain.clone());
    }
    for p in &pending {
        order.mark_ready(&p.domain).await?;
    }
    order.wait_ready().await?;
    let csr = build_csr(domains, &key)?;
    let chain_pem = order.finalize(&csr).await?;
    let now = now_unix();
    verify_chain(&chain_pem, &key, domains, now)?;
    Ok(StoredCert {
        chain_pem,
        key_pem: key.serialize_pem(),
        issued_at: now,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acme::client::mock::{MockAcmeClient, MockBehavior, MockStep};
    use crate::acme::storage::fs::FsCertStorage;
    use std::sync::Arc;

    fn solver(tag: &str) -> Arc<TlsAlpnSolver> {
        let d = std::env::temp_dir().join(format!("fb_acme_order_{}_{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        TlsAlpnSolver::new(Arc::new(FsCertStorage::new(d)))
    }

    fn doms() -> Vec<String> {
        vec!["a.example.com".to_string(), "b.example.com".to_string()]
    }

    #[test]
    fn key_type_parse_and_generate() {
        assert_eq!(KeyType::parse("ecdsa-p256").unwrap(), KeyType::EcdsaP256);
        assert_eq!(KeyType::parse("ecdsa-p384").unwrap(), KeyType::EcdsaP384);
        assert!(KeyType::parse("rsa-2048").is_err());
        let k = generate_key(KeyType::EcdsaP384).unwrap();
        assert!(k.is_compatible(&rcgen::PKCS_ECDSA_P384_SHA384));
        let csr = build_csr(&doms(), &k).unwrap();
        assert!(!csr.is_empty());
    }

    #[tokio::test]
    async fn issue_happy_path_returns_verified_cert_and_clears_challenges() {
        let client = MockAcmeClient::new(MockBehavior::default());
        let solver = solver("happy");
        let stored = issue(&client, &solver, &doms(), KeyType::EcdsaP256)
            .await
            .unwrap();
        let (_, leaf) =
            crate::acme::load_certified_key(&stored.chain_pem, &stored.key_pem).unwrap();
        let mut sans = crate::acme::leaf_dns_sans(&leaf).unwrap();
        sans.sort();
        assert_eq!(sans, doms());
        assert!(stored.issued_at > 0);
        assert!(
            solver.cached_domains().is_empty(),
            "challenges cleared after success"
        );
        assert_eq!(client.orders(), 1);
    }

    #[tokio::test]
    async fn issue_rejects_chain_for_a_foreign_key_and_clears_challenges() {
        let client = MockAcmeClient::new(MockBehavior {
            wrong_key_chain: true,
            ..Default::default()
        });
        let solver = solver("wrongkey");
        let err = issue(&client, &solver, &doms(), KeyType::EcdsaP256)
            .await
            .unwrap_err();
        assert!(matches!(err, AcmeError::Certificate(_)), "{err}");
        assert!(solver.cached_domains().is_empty());
    }

    #[tokio::test]
    async fn issue_surfaces_ca_failures_and_clears_challenges() {
        for step in [MockStep::NewOrder, MockStep::WaitReady, MockStep::Finalize] {
            let client = MockAcmeClient::new(MockBehavior {
                fail_step: Some(step),
                ..Default::default()
            });
            let solver = solver(&format!("fail{step:?}"));
            let err = issue(&client, &solver, &doms(), KeyType::EcdsaP256)
                .await
                .unwrap_err();
            assert!(matches!(err, AcmeError::Protocol(_)), "{step:?}: {err}");
            assert!(
                solver.cached_domains().is_empty(),
                "{step:?}: challenges must be cleared"
            );
        }
    }

    #[test]
    fn verify_chain_checks_sans_validity_and_key() {
        let key = generate_key(KeyType::EcdsaP256).unwrap();
        let mut params = rcgen::CertificateParams::new(vec!["a.example.com".to_string()]).unwrap();
        let now = time::OffsetDateTime::now_utc();
        params.not_before = now - time::Duration::hours(1);
        params.not_after = now + time::Duration::days(30);
        let cert = params.self_signed(&key).unwrap();
        let pem = cert.pem();
        let t = crate::acme::now_unix();
        verify_chain(&pem, &key, &["a.example.com".to_string()], t).unwrap();
        // Missing SAN.
        assert!(verify_chain(&pem, &key, &doms(), t).is_err());
        // Expired (evaluate "now" after not_after).
        assert!(verify_chain(&pem, &key, &["a.example.com".to_string()], t + 31 * 86_400).is_err());
        // Foreign key.
        let other = generate_key(KeyType::EcdsaP256).unwrap();
        assert!(verify_chain(&pem, &other, &["a.example.com".to_string()], t).is_err());
    }
}
