//! `stores:`-backed `CertStorage` (redis/valkey; `redis-store` feature).
//!
//! Keys under the store's `key_prefix`: `acme:account`, `acme:cert:{<id>}`,
//! `acme:challenge:<domain>` (with TTL), `acme:lease:{<id>}` (`SET NX PX`,
//! owner-checked renew/release via Lua). Account credentials and certificate
//! private keys are sealed with [`CookieSealer`] (AES-256-GCM, key = SHA-256 of
//! `acme.storage.encryption_key`) before they are written; chains are stored
//! in clear. Hash tags keep one certificate's keys on a single Cluster slot.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use redis::AsyncCommands;
use serde::{Deserialize, Serialize};

use super::CertStorage;
use crate::acme::{AcmeError, CertId, StoredCert};
use crate::plugins::util::cookie_session::CookieSealer;
use crate::stores::redis_store::RedisStoreClient;

/// Sealed blobs never expire on their own; storage TTLs govern lifetime.
const SEAL_TTL: Duration = Duration::from_secs(100 * 365 * 86_400);

const RENEW_LEASE_SCRIPT: &str = r#"
if redis.call('GET', KEYS[1]) == ARGV[1] then
  return redis.call('PEXPIRE', KEYS[1], ARGV[2])
else
  return 0
end"#;

const RELEASE_LEASE_SCRIPT: &str = r#"
if redis.call('GET', KEYS[1]) == ARGV[1] then
  return redis.call('DEL', KEYS[1])
else
  return 0
end"#;

pub(super) fn account_key(prefix: &str) -> String {
    format!("{prefix}:acme:account")
}
pub(super) fn cert_key(prefix: &str, id: &CertId) -> String {
    format!("{prefix}:acme:cert:{{{}}}", id.as_str())
}
fn challenge_key(prefix: &str, domain: &str) -> String {
    format!("{prefix}:acme:challenge:{domain}")
}
fn lease_key(prefix: &str, id: &CertId) -> String {
    format!("{prefix}:acme:lease:{{{}}}", id.as_str())
}

#[derive(Serialize, Deserialize)]
struct CertRecord {
    chain_pem: String,
    /// `CookieSealer::seal(key_pem)`.
    key_sealed: String,
    issued_at: i64,
}

pub struct RedisCertStorage {
    client: Arc<RedisStoreClient>,
    sealer: CookieSealer,
    renew_script: redis::Script,
    release_script: redis::Script,
}

impl RedisCertStorage {
    pub fn new(client: Arc<RedisStoreClient>, encryption_key: &str) -> Self {
        Self {
            client,
            sealer: CookieSealer::new(encryption_key),
            renew_script: redis::Script::new(RENEW_LEASE_SCRIPT),
            release_script: redis::Script::new(RELEASE_LEASE_SCRIPT),
        }
    }

    async fn conn(&self) -> Result<redis::aio::ConnectionManager, AcmeError> {
        self.client.conn().await.map_err(AcmeError::Storage)
    }

    fn err(&self, what: &str, e: impl std::fmt::Display) -> AcmeError {
        AcmeError::Storage(format!("store '{}': {what}: {e}", self.client.name()))
    }

    fn open(&self, sealed: &str, what: &str) -> Result<Vec<u8>, AcmeError> {
        self.sealer.open(sealed).map_err(|e| {
            AcmeError::Storage(format!(
                "store '{}': cannot unseal {what} ({e}) — was acme.storage.encryption_key changed?",
                self.client.name()
            ))
        })
    }

    fn prefix(&self) -> &str {
        self.client.key_prefix()
    }
}

#[async_trait]
impl CertStorage for RedisCertStorage {
    fn label(&self) -> String {
        format!("store:{}", self.client.name())
    }

    async fn load_account(&self) -> Result<Option<Vec<u8>>, AcmeError> {
        let mut conn = self.conn().await?;
        let sealed: Option<String> = conn
            .get(account_key(self.prefix()))
            .await
            .map_err(|e| self.err("load_account", e))?;
        sealed
            .map(|s| self.open(&s, "account credentials"))
            .transpose()
    }

    async fn save_account(&self, creds: &[u8]) -> Result<(), AcmeError> {
        let mut conn = self.conn().await?;
        conn.set::<_, _, ()>(
            account_key(self.prefix()),
            self.sealer.seal(creds, SEAL_TTL),
        )
        .await
        .map_err(|e| self.err("save_account", e))
    }

    async fn load_cert(&self, id: &CertId) -> Result<Option<StoredCert>, AcmeError> {
        let mut conn = self.conn().await?;
        let raw: Option<String> = conn
            .get(cert_key(self.prefix(), id))
            .await
            .map_err(|e| self.err("load_cert", e))?;
        let Some(raw) = raw else { return Ok(None) };
        let rec: CertRecord =
            serde_json::from_str(&raw).map_err(|e| self.err("load_cert: corrupt record", e))?;
        let key = self.open(&rec.key_sealed, "certificate private key")?;
        Ok(Some(StoredCert {
            chain_pem: rec.chain_pem,
            key_pem: String::from_utf8(key).map_err(|e| self.err("load_cert: key utf8", e))?,
            issued_at: rec.issued_at,
        }))
    }

    async fn save_cert(&self, id: &CertId, cert: &StoredCert) -> Result<(), AcmeError> {
        let rec = CertRecord {
            chain_pem: cert.chain_pem.clone(),
            key_sealed: self.sealer.seal(cert.key_pem.as_bytes(), SEAL_TTL),
            issued_at: cert.issued_at,
        };
        let mut conn = self.conn().await?;
        conn.set::<_, _, ()>(
            cert_key(self.prefix(), id),
            serde_json::to_string(&rec).unwrap(),
        )
        .await
        .map_err(|e| self.err("save_cert", e))
    }

    async fn put_challenge(
        &self,
        domain: &str,
        key_auth: &str,
        ttl: Duration,
    ) -> Result<(), AcmeError> {
        let mut conn = self.conn().await?;
        redis::cmd("SET")
            .arg(challenge_key(self.prefix(), domain))
            .arg(key_auth)
            .arg("PX")
            .arg(ttl.as_millis().max(1) as u64)
            .query_async::<()>(&mut conn)
            .await
            .map_err(|e| self.err("put_challenge", e))
    }

    async fn get_challenge(&self, domain: &str) -> Result<Option<String>, AcmeError> {
        let mut conn = self.conn().await?;
        conn.get(challenge_key(self.prefix(), domain))
            .await
            .map_err(|e| self.err("get_challenge", e))
    }

    async fn remove_challenge(&self, domain: &str) -> Result<(), AcmeError> {
        let mut conn = self.conn().await?;
        conn.del::<_, ()>(challenge_key(self.prefix(), domain))
            .await
            .map_err(|e| self.err("remove_challenge", e))
    }

    async fn try_acquire_lease(
        &self,
        id: &CertId,
        owner: &str,
        ttl: Duration,
    ) -> Result<bool, AcmeError> {
        let mut conn = self.conn().await?;
        let key = lease_key(self.prefix(), id);
        let acquired: Option<String> = redis::cmd("SET")
            .arg(&key)
            .arg(owner)
            .arg("NX")
            .arg("PX")
            .arg(ttl.as_millis().max(1) as u64)
            .query_async(&mut conn)
            .await
            .map_err(|e| self.err("try_acquire_lease", e))?;
        if acquired.is_some() {
            return Ok(true);
        }
        // Re-entrant for the current owner (and refreshes its TTL).
        self.renew_lease(id, owner, ttl).await
    }

    async fn renew_lease(
        &self,
        id: &CertId,
        owner: &str,
        ttl: Duration,
    ) -> Result<bool, AcmeError> {
        let mut conn = self.conn().await?;
        let n: i64 = self
            .renew_script
            .key(lease_key(self.prefix(), id))
            .arg(owner)
            .arg(ttl.as_millis().max(1) as u64)
            .invoke_async(&mut conn)
            .await
            .map_err(|e| self.err("renew_lease", e))?;
        Ok(n == 1)
    }

    async fn release_lease(&self, id: &CertId, owner: &str) -> Result<(), AcmeError> {
        let mut conn = self.conn().await?;
        let _: i64 = self
            .release_script
            .key(lease_key(self.prefix(), id))
            .arg(owner)
            .invoke_async(&mut conn)
            .await
            .map_err(|e| self.err("release_lease", e))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use redis::AsyncCommands;

    fn live_client(tag: &str) -> Option<Arc<RedisStoreClient>> {
        let Ok(url) = std::env::var("FEATHERBIT_TEST_REDIS_URL") else {
            eprintln!("skipping: FEATHERBIT_TEST_REDIS_URL not set");
            return None;
        };
        let cfg: crate::config::StoreConfig = serde_yaml::from_str(&format!(
            "name: acme-live\ntype: redis\nurl: {url}\nkey_prefix: fbacme{tag}{}\n",
            std::process::id()
        ))
        .unwrap();
        Some(Arc::new(RedisStoreClient::build(&cfg).unwrap()))
    }

    #[tokio::test]
    async fn redis_storage_satisfies_contract() {
        let Some(client) = live_client("c") else {
            return;
        };
        let storage = Arc::new(RedisCertStorage::new(client, "test-secret"));
        assert_eq!(storage.label(), "store:acme-live");
        crate::acme::storage::contract::run_all(storage).await;
    }

    #[tokio::test]
    async fn redis_storage_seals_private_material() {
        let Some(client) = live_client("s") else {
            return;
        };
        let storage = RedisCertStorage::new(client.clone(), "test-secret");
        let (id, _) = CertId::from_domains(&["sealed.example.com".into()]).unwrap();
        let cert = StoredCert {
            chain_pem: "-----BEGIN CERTIFICATE-----\nAAA\n".into(),
            key_pem: "-----BEGIN PRIVATE KEY-----\nSECRET-KEY-BYTES\n".into(),
            issued_at: 42,
        };
        storage.save_cert(&id, &cert).await.unwrap();
        storage.save_account(b"ACCOUNT-SECRET").await.unwrap();

        let mut conn = client.conn().await.unwrap();
        let raw_cert: String = conn.get(cert_key(client.key_prefix(), &id)).await.unwrap();
        assert!(
            raw_cert.contains("BEGIN CERTIFICATE"),
            "chain is stored in clear"
        );
        assert!(
            !raw_cert.contains("SECRET-KEY-BYTES"),
            "key must be sealed: {raw_cert}"
        );
        let raw_acct: String = conn.get(account_key(client.key_prefix())).await.unwrap();
        assert!(!raw_acct.contains("ACCOUNT-SECRET"));

        // A different secret cannot open it.
        let other = RedisCertStorage::new(client.clone(), "wrong");
        assert!(other.load_cert(&id).await.is_err());
        assert_eq!(storage.load_cert(&id).await.unwrap().unwrap(), cert);
    }
}
