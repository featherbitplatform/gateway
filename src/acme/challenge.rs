//! TLS-ALPN-01 (RFC 8737) challenge solving.
//!
//! The CA opens a TLS connection to the domain on 443 with ALPN `acme-tls/1`
//! and expects a self-signed certificate whose SAN is the domain and which
//! carries a critical `acmeIdentifier` extension holding the SHA-256 of the key
//! authorization. `server::tls`' resolver asks a [`ChallengeSolver`] for that
//! certificate; [`TlsAlpnSolver`] builds it from the key authorization the
//! order state machine registered.
//!
//! The resolver call is synchronous, so the solver keeps an in-process cache.
//! Registrations also go to [`CertStorage`] so a *peer* instance (behind a TCP
//! load balancer) can adopt them via [`TlsAlpnSolver::refresh_from_storage`].

use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::sign::CertifiedKey;

use super::storage::CertStorage;
use super::AcmeError;

pub const ACME_TLS_ALPN: &[u8] = b"acme-tls/1";
/// A pending challenge that outlives this is stale; storage and cache both drop it.
pub const CHALLENGE_TTL: Duration = Duration::from_secs(600);

pub trait ChallengeSolver: Send + Sync + std::fmt::Debug {
    /// The challenge certificate for `server_name` (SNI, any case), if a
    /// validation is pending for it.
    fn challenge_cert(&self, server_name: &str) -> Option<Arc<CertifiedKey>>;
}

pub fn key_authorization_digest(key_auth: &str) -> Vec<u8> {
    ring::digest::digest(&ring::digest::SHA256, key_auth.as_bytes())
        .as_ref()
        .to_vec()
}

/// Self-signed challenge certificate: SAN = `domain`, critical `acmeIdentifier`
/// = SHA-256(`key_auth`), fresh key, valid for the challenge TTL.
pub fn build_challenge_cert(domain: &str, key_auth: &str) -> Result<Arc<CertifiedKey>, AcmeError> {
    let mut params = rcgen::CertificateParams::new(vec![domain.to_string()])
        .map_err(|e| AcmeError::Crypto(e.to_string()))?;
    let mut ext = rcgen::CustomExtension::new_acme_identifier(&key_authorization_digest(key_auth));
    ext.set_criticality(true);
    params.custom_extensions = vec![ext];
    let now = time::OffsetDateTime::now_utc();
    params.not_before = now - time::Duration::minutes(5);
    params.not_after = now + time::Duration::seconds(CHALLENGE_TTL.as_secs() as i64 + 300);
    let key = rcgen::KeyPair::generate().map_err(|e| AcmeError::Crypto(e.to_string()))?;
    let cert = params
        .self_signed(&key)
        .map_err(|e| AcmeError::Crypto(e.to_string()))?;
    // Not `load_certified_key`: it calls `CertifiedKey::keys_match()`, which
    // parses the leaf through rustls-webpki's strict `EndEntityCert`, and that
    // rejects any certificate carrying a critical extension it doesn't
    // recognize — exactly the critical `acmeIdentifier` this certificate
    // exists to carry. Cert and key were just minted together above, so the
    // match is already guaranteed; build the `CertifiedKey` directly instead.
    certified_key_from_pem(&cert.pem(), &key.serialize_pem())
}

/// Like [`super::load_certified_key`] but skips `CertifiedKey::keys_match()`
/// (see the comment in [`build_challenge_cert`] for why that check can't run
/// on a challenge certificate).
fn certified_key_from_pem(chain_pem: &str, key_pem: &str) -> Result<Arc<CertifiedKey>, AcmeError> {
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
    let signing_key = super::provider()
        .key_provider
        .load_private_key(key)
        .map_err(|e| AcmeError::Certificate(format!("certificate/key: {e}")))?;
    Ok(Arc::new(CertifiedKey::new(chain, signing_key)))
}

struct Cached {
    key_auth: String,
    cert: Arc<CertifiedKey>,
    expires_at: Instant,
}

pub struct TlsAlpnSolver {
    storage: Arc<dyn CertStorage>,
    cache: RwLock<HashMap<String, Cached>>,
    ttl: Duration,
}

impl std::fmt::Debug for TlsAlpnSolver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "TlsAlpnSolver({} pending)",
            self.cache.read().map(|c| c.len()).unwrap_or(0)
        )
    }
}

impl TlsAlpnSolver {
    pub fn new(storage: Arc<dyn CertStorage>) -> Arc<Self> {
        Self::with_ttl(storage, CHALLENGE_TTL)
    }

    pub fn with_ttl(storage: Arc<dyn CertStorage>, ttl: Duration) -> Arc<Self> {
        Arc::new(Self {
            storage,
            cache: RwLock::new(HashMap::new()),
            ttl,
        })
    }

    fn cache_insert(&self, domain: &str, key_auth: &str) -> Result<(), AcmeError> {
        let cert = build_challenge_cert(domain, key_auth)?;
        let mut cache = self.cache.write().unwrap_or_else(|e| e.into_inner());
        cache.insert(
            domain.to_ascii_lowercase(),
            Cached {
                key_auth: key_auth.to_string(),
                cert,
                expires_at: Instant::now() + self.ttl,
            },
        );
        Ok(())
    }

    fn cache_remove(&self, domain: &str) {
        let mut cache = self.cache.write().unwrap_or_else(|e| e.into_inner());
        cache.remove(&domain.to_ascii_lowercase());
    }

    /// Registers a pending validation: persisted (for peers) and cached (for
    /// this instance's resolver).
    pub async fn register(&self, domain: &str, key_auth: &str) -> Result<(), AcmeError> {
        self.storage
            .put_challenge(domain, key_auth, self.ttl)
            .await?;
        self.cache_insert(domain, key_auth)
    }

    pub async fn clear(&self, domain: &str) -> Result<(), AcmeError> {
        self.cache_remove(domain);
        self.storage.remove_challenge(domain).await
    }

    /// Adopts (or evicts) challenges another instance registered in storage.
    pub async fn refresh_from_storage(&self, domains: &[String]) -> Result<(), AcmeError> {
        for domain in domains {
            match self.storage.get_challenge(domain).await? {
                Some(key_auth) => {
                    let already = {
                        let cache = self.cache.read().unwrap_or_else(|e| e.into_inner());
                        cache
                            .get(&domain.to_ascii_lowercase())
                            .is_some_and(|c| c.key_auth == key_auth)
                    };
                    if !already {
                        self.cache_insert(domain, &key_auth)?;
                    }
                }
                None => self.cache_remove(domain),
            }
        }
        Ok(())
    }

    pub fn cached_domains(&self) -> Vec<String> {
        let now = Instant::now();
        let cache = self.cache.read().unwrap_or_else(|e| e.into_inner());
        let mut v: Vec<String> = cache
            .iter()
            .filter(|(_, c)| c.expires_at > now)
            .map(|(d, _)| d.clone())
            .collect();
        v.sort();
        v
    }
}

impl ChallengeSolver for TlsAlpnSolver {
    fn challenge_cert(&self, server_name: &str) -> Option<Arc<CertifiedKey>> {
        let name = server_name.to_ascii_lowercase();
        let cache = self.cache.read().unwrap_or_else(|e| e.into_inner());
        match cache.get(&name) {
            Some(c) if c.expires_at > Instant::now() => Some(c.cert.clone()),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acme::storage::fs::FsCertStorage;

    /// The `acmeIdentifier` extension (RFC 8737 §3): OID 1.3.6.1.5.5.7.1.31,
    /// critical, value = DER OCTET STRING of the SHA-256 key-auth digest.
    fn acme_identifier(leaf_der: &[u8]) -> Option<(bool, Vec<u8>)> {
        use x509_parser::prelude::*;
        let (_, cert) = X509Certificate::from_der(leaf_der).unwrap();
        cert.extensions()
            .iter()
            .find(|e| e.oid.to_id_string() == "1.3.6.1.5.5.7.1.31")
            .map(|e| (e.critical, e.value.to_vec()))
    }

    fn temp_storage(tag: &str) -> Arc<dyn CertStorage> {
        let d = std::env::temp_dir().join(format!("fb_acme_ch_{}_{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        Arc::new(FsCertStorage::new(d))
    }

    #[test]
    fn challenge_cert_carries_critical_acme_identifier_with_digest() {
        let ck = build_challenge_cert("x.example.com", "token.thumbprint").unwrap();
        let leaf = ck.end_entity_cert().unwrap();
        let (critical, value) = acme_identifier(leaf.as_ref()).expect("extension present");
        assert!(critical);
        let digest = key_authorization_digest("token.thumbprint");
        assert_eq!(value.len(), 34);
        assert_eq!(&value[..2], &[0x04, 0x20]);
        assert_eq!(&value[2..], digest.as_slice());
        assert_eq!(
            crate::acme::leaf_dns_sans(leaf.as_ref()).unwrap(),
            vec!["x.example.com".to_string()]
        );
    }

    #[tokio::test]
    async fn register_serves_then_clear_stops() {
        let solver = TlsAlpnSolver::new(temp_storage("reg"));
        assert!(solver.challenge_cert("a.example.com").is_none());
        solver.register("a.example.com", "ka").await.unwrap();
        assert!(solver.challenge_cert("a.example.com").is_some());
        assert!(
            solver.challenge_cert("A.EXAMPLE.COM").is_some(),
            "SNI is case-insensitive"
        );
        assert!(solver.challenge_cert("b.example.com").is_none());
        assert_eq!(solver.cached_domains(), vec!["a.example.com".to_string()]);
        solver.clear("a.example.com").await.unwrap();
        assert!(solver.challenge_cert("a.example.com").is_none());
    }

    #[tokio::test]
    async fn refresh_from_storage_picks_up_a_peer_registration() {
        let storage = temp_storage("peer");
        let peer = TlsAlpnSolver::new(storage.clone());
        let me = TlsAlpnSolver::new(storage);
        peer.register("p.example.com", "peer-ka").await.unwrap();
        assert!(me.challenge_cert("p.example.com").is_none());
        me.refresh_from_storage(&["p.example.com".to_string(), "none.example.com".to_string()])
            .await
            .unwrap();
        let ck = me
            .challenge_cert("p.example.com")
            .expect("adopted from storage");
        let (_, value) = acme_identifier(ck.end_entity_cert().unwrap().as_ref()).unwrap();
        assert_eq!(&value[2..], key_authorization_digest("peer-ka").as_slice());
        peer.clear("p.example.com").await.unwrap();
        me.refresh_from_storage(&["p.example.com".to_string()])
            .await
            .unwrap();
        assert!(
            me.challenge_cert("p.example.com").is_none(),
            "cleared upstream ⇒ evicted"
        );
    }

    #[tokio::test]
    async fn cache_entries_expire() {
        let solver = TlsAlpnSolver::with_ttl(temp_storage("ttl"), Duration::from_millis(30));
        solver.register("t.example.com", "ka").await.unwrap();
        assert!(solver.challenge_cert("t.example.com").is_some());
        tokio::time::sleep(Duration::from_millis(60)).await;
        assert!(solver.challenge_cert("t.example.com").is_none());
    }
}
