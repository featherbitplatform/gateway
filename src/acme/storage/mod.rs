//! ACME state storage: account credentials, per-certificate key+chain, pending
//! TLS-ALPN-01 key authorizations, and the renewal lease that keeps N gateway
//! instances from ordering the same certificate at once.
//!
//! Backends: [`fs::FsCertStorage`] (single node, or a shared volume for certs —
//! not for challenges) and, behind the `redis-store` feature,
//! `redis::RedisCertStorage` (cluster-ready; keys sealed at rest).

use std::time::Duration;

use async_trait::async_trait;

use super::{AcmeError, CertId, StoredCert};

pub mod fs;
#[cfg(feature = "redis-store")]
pub mod redis;

#[async_trait]
pub trait CertStorage: Send + Sync {
    /// Operator-facing name shown in the Admin API (`filesystem`, `store:<name>`).
    fn label(&self) -> String;
    async fn load_account(&self) -> Result<Option<Vec<u8>>, AcmeError>;
    async fn save_account(&self, creds: &[u8]) -> Result<(), AcmeError>;
    async fn load_cert(&self, id: &CertId) -> Result<Option<StoredCert>, AcmeError>;
    async fn save_cert(&self, id: &CertId, cert: &StoredCert) -> Result<(), AcmeError>;
    /// Registers `key_auth` for `domain`; expires after `ttl`.
    async fn put_challenge(
        &self,
        domain: &str,
        key_auth: &str,
        ttl: Duration,
    ) -> Result<(), AcmeError>;
    async fn get_challenge(&self, domain: &str) -> Result<Option<String>, AcmeError>;
    async fn remove_challenge(&self, domain: &str) -> Result<(), AcmeError>;
    /// `true` when this `owner` now holds the lease for `id` (fresh or already
    /// its own); `false` when another live owner holds it.
    async fn try_acquire_lease(
        &self,
        id: &CertId,
        owner: &str,
        ttl: Duration,
    ) -> Result<bool, AcmeError>;
    /// Extends the lease iff `owner` holds it.
    async fn renew_lease(&self, id: &CertId, owner: &str, ttl: Duration)
        -> Result<bool, AcmeError>;
    /// Releases iff `owner` holds it (a no-op otherwise).
    async fn release_lease(&self, id: &CertId, owner: &str) -> Result<(), AcmeError>;
}

/// Backend-agnostic behavior every `CertStorage` must satisfy. Each backend's
/// tests call `run_all`. TTL checks sleep ≥1 s because backends may keep
/// second-resolution expiries.
#[cfg(test)]
pub(crate) mod contract {
    use super::*;
    use std::sync::Arc;

    pub async fn run_all(storage: Arc<dyn CertStorage>) {
        account_round_trip(&*storage).await;
        cert_round_trip(&*storage).await;
        challenge_ttl(&*storage).await;
        lease_semantics(&*storage).await;
    }

    async fn account_round_trip(s: &dyn CertStorage) {
        assert_eq!(s.load_account().await.unwrap(), None);
        s.save_account(b"{\"id\":\"acct\"}").await.unwrap();
        assert_eq!(
            s.load_account().await.unwrap().unwrap(),
            b"{\"id\":\"acct\"}"
        );
        s.save_account(b"v2").await.unwrap();
        assert_eq!(s.load_account().await.unwrap().unwrap(), b"v2");
    }

    async fn cert_round_trip(s: &dyn CertStorage) {
        let (id, _) =
            CertId::from_domains(&["a.example.com".into(), "b.example.com".into()]).unwrap();
        assert!(s.load_cert(&id).await.unwrap().is_none());
        let cert = StoredCert {
            chain_pem: "CHAIN".into(),
            key_pem: "KEY".into(),
            issued_at: 1_700_000_000,
        };
        s.save_cert(&id, &cert).await.unwrap();
        assert_eq!(s.load_cert(&id).await.unwrap().unwrap(), cert);
        let (other, _) = CertId::from_domains(&["c.example.com".into()]).unwrap();
        assert!(s.load_cert(&other).await.unwrap().is_none());
    }

    async fn challenge_ttl(s: &dyn CertStorage) {
        assert!(s.get_challenge("x.example.com").await.unwrap().is_none());
        s.put_challenge("x.example.com", "tok.thumb", Duration::from_secs(60))
            .await
            .unwrap();
        assert_eq!(
            s.get_challenge("x.example.com").await.unwrap().unwrap(),
            "tok.thumb"
        );
        s.remove_challenge("x.example.com").await.unwrap();
        assert!(s.get_challenge("x.example.com").await.unwrap().is_none());
        s.put_challenge("y.example.com", "old", Duration::from_millis(50))
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(1_100)).await;
        assert!(
            s.get_challenge("y.example.com").await.unwrap().is_none(),
            "expired reads as absent"
        );
    }

    async fn lease_semantics(s: &dyn CertStorage) {
        let (id, _) = CertId::from_domains(&["lease.example.com".into()]).unwrap();
        let ttl = Duration::from_secs(30);
        assert!(s.try_acquire_lease(&id, "me", ttl).await.unwrap());
        assert!(!s.try_acquire_lease(&id, "peer", ttl).await.unwrap());
        assert!(
            s.try_acquire_lease(&id, "me", ttl).await.unwrap(),
            "re-entrant for the owner"
        );
        assert!(s.renew_lease(&id, "me", ttl).await.unwrap());
        assert!(!s.renew_lease(&id, "peer", ttl).await.unwrap());
        s.release_lease(&id, "peer").await.unwrap(); // not the owner: no-op
        assert!(!s.try_acquire_lease(&id, "peer", ttl).await.unwrap());
        s.release_lease(&id, "me").await.unwrap();
        assert!(s
            .try_acquire_lease(&id, "peer", Duration::from_millis(50))
            .await
            .unwrap());
        tokio::time::sleep(Duration::from_millis(1_100)).await;
        assert!(
            s.try_acquire_lease(&id, "me", ttl).await.unwrap(),
            "expired lease is free"
        );
        s.release_lease(&id, "me").await.unwrap();
    }
}
