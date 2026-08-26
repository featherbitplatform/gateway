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
use crate::plugins::resources::PluginResources;
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

/// Holds the *name* of the store, never a client: the client is resolved from
/// the live [`StoreRegistry`](crate::stores::StoreRegistry) on every call, so a
/// `PUT /api/stores/:name` credential/URL change (which swaps the registry
/// inside `PluginResources`) is picked up by the next ACME operation instead of
/// being pinned to the connection that existed at startup.
pub struct RedisCertStorage {
    resources: Arc<PluginResources>,
    name: String,
    sealer: CookieSealer,
    renew_script: redis::Script,
    release_script: redis::Script,
}

impl RedisCertStorage {
    pub fn new(
        resources: Arc<PluginResources>,
        name: impl Into<String>,
        encryption_key: &str,
    ) -> Self {
        Self {
            resources,
            name: name.into(),
            sealer: CookieSealer::new(encryption_key),
            renew_script: redis::Script::new(RENEW_LEASE_SCRIPT),
            release_script: redis::Script::new(RELEASE_LEASE_SCRIPT),
        }
    }

    /// The store's current client, or a storage error naming the store when it
    /// is no longer declared.
    pub(crate) fn client(&self) -> Result<Arc<RedisStoreClient>, AcmeError> {
        self.resources
            .stores
            .load()
            .client(&self.name)
            .map_err(AcmeError::Storage)
    }

    /// Resolves the current client *and* a connection from it. Every operation
    /// goes through here, so both the connection and the key prefix come from
    /// whatever the registry holds right now.
    async fn conn(
        &self,
    ) -> Result<(Arc<RedisStoreClient>, redis::aio::ConnectionManager), AcmeError> {
        let client = self.client()?;
        let conn = client.conn().await.map_err(AcmeError::Storage)?;
        Ok((client, conn))
    }

    fn err(&self, what: &str, e: impl std::fmt::Display) -> AcmeError {
        AcmeError::Storage(format!("store '{}': {what}: {e}", self.name))
    }

    fn open(&self, sealed: &str, what: &str) -> Result<Vec<u8>, AcmeError> {
        self.sealer.open(sealed).map_err(|e| {
            AcmeError::Storage(format!(
                "store '{}': cannot unseal {what} ({e}) — was acme.storage.encryption_key changed?",
                self.name
            ))
        })
    }
}

#[async_trait]
impl CertStorage for RedisCertStorage {
    fn label(&self) -> String {
        format!("store:{}", self.name)
    }

    async fn load_account(&self) -> Result<Option<Vec<u8>>, AcmeError> {
        let (client, mut conn) = self.conn().await?;
        let sealed: Option<String> = conn
            .get(account_key(client.key_prefix()))
            .await
            .map_err(|e| self.err("load_account", e))?;
        sealed
            .map(|s| self.open(&s, "account credentials"))
            .transpose()
    }

    async fn save_account(&self, creds: &[u8]) -> Result<(), AcmeError> {
        let (client, mut conn) = self.conn().await?;
        conn.set::<_, _, ()>(
            account_key(client.key_prefix()),
            self.sealer.seal(creds, SEAL_TTL),
        )
        .await
        .map_err(|e| self.err("save_account", e))
    }

    async fn load_cert(&self, id: &CertId) -> Result<Option<StoredCert>, AcmeError> {
        let (client, mut conn) = self.conn().await?;
        let raw: Option<String> = conn
            .get(cert_key(client.key_prefix(), id))
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
        let (client, mut conn) = self.conn().await?;
        conn.set::<_, _, ()>(
            cert_key(client.key_prefix(), id),
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
        let (client, mut conn) = self.conn().await?;
        redis::cmd("SET")
            .arg(challenge_key(client.key_prefix(), domain))
            .arg(key_auth)
            .arg("PX")
            .arg(ttl.as_millis().max(1) as u64)
            .query_async::<()>(&mut conn)
            .await
            .map_err(|e| self.err("put_challenge", e))
    }

    async fn get_challenge(&self, domain: &str) -> Result<Option<String>, AcmeError> {
        let (client, mut conn) = self.conn().await?;
        conn.get(challenge_key(client.key_prefix(), domain))
            .await
            .map_err(|e| self.err("get_challenge", e))
    }

    async fn remove_challenge(&self, domain: &str) -> Result<(), AcmeError> {
        let (client, mut conn) = self.conn().await?;
        conn.del::<_, ()>(challenge_key(client.key_prefix(), domain))
            .await
            .map_err(|e| self.err("remove_challenge", e))
    }

    async fn try_acquire_lease(
        &self,
        id: &CertId,
        owner: &str,
        ttl: Duration,
    ) -> Result<bool, AcmeError> {
        let (client, mut conn) = self.conn().await?;
        let key = lease_key(client.key_prefix(), id);
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
        let (client, mut conn) = self.conn().await?;
        let n: i64 = self
            .renew_script
            .key(lease_key(client.key_prefix(), id))
            .arg(owner)
            .arg(ttl.as_millis().max(1) as u64)
            .invoke_async(&mut conn)
            .await
            .map_err(|e| self.err("renew_lease", e))?;
        Ok(n == 1)
    }

    async fn release_lease(&self, id: &CertId, owner: &str) -> Result<(), AcmeError> {
        let (client, mut conn) = self.conn().await?;
        let _: i64 = self
            .release_script
            .key(lease_key(client.key_prefix(), id))
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
    use crate::stores::StoreRegistry;
    use redis::AsyncCommands;

    fn store_cfg(tag: &str, url: &str) -> crate::config::StoreConfig {
        serde_yaml::from_str(&format!(
            "name: acme-live
type: redis
url: {url}
key_prefix: fbacme{tag}{}
",
            std::process::id()
        ))
        .unwrap()
    }

    /// `PluginResources` whose registry holds one live `acme-live` store, plus
    /// the client for direct assertions. `None` when the gate env var is unset.
    fn live_resources(tag: &str) -> Option<(Arc<PluginResources>, Arc<RedisStoreClient>)> {
        let Ok(url) = std::env::var("FEATHERBIT_TEST_REDIS_URL") else {
            eprintln!("skipping: FEATHERBIT_TEST_REDIS_URL not set");
            return None;
        };
        let cfg = store_cfg(tag, &url);
        let resources = PluginResources::new(None);
        let registry =
            StoreRegistry::rebuild(&StoreRegistry::default(), std::slice::from_ref(&cfg), None)
                .unwrap();
        let client = registry.client("acme-live").unwrap();
        resources.stores.store(Arc::new(registry));
        Some((resources, client))
    }

    #[tokio::test]
    async fn redis_storage_satisfies_contract() {
        let Some((resources, _)) = live_resources("c") else {
            return;
        };
        let storage = Arc::new(RedisCertStorage::new(resources, "acme-live", "test-secret"));
        assert_eq!(storage.label(), "store:acme-live");
        crate::acme::storage::contract::run_all(storage).await;
    }

    #[tokio::test]
    async fn redis_storage_seals_private_material() {
        let Some((resources, client)) = live_resources("s") else {
            return;
        };
        let storage = RedisCertStorage::new(resources.clone(), "acme-live", "test-secret");
        let (id, _) = CertId::from_domains(&["sealed.example.com".into()]).unwrap();
        let cert = StoredCert {
            chain_pem: "-----BEGIN CERTIFICATE-----
AAA
"
            .into(),
            key_pem: "-----BEGIN PRIVATE KEY-----
SECRET-KEY-BYTES
"
            .into(),
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
        let other = RedisCertStorage::new(resources, "acme-live", "wrong");
        assert!(other.load_cert(&id).await.is_err());
        assert_eq!(storage.load_cert(&id).await.unwrap().unwrap(), cert);
    }

    /// The store's client is resolved per call, not captured once: swapping the
    /// registry (what `PUT /api/stores/:name` does) must be visible to the very
    /// next operation. Observed through `key_prefix`, which is part of the
    /// client — a storage pinned to the old client would keep writing under the
    /// old prefix.
    #[tokio::test]
    async fn client_is_resolved_per_call_so_a_store_edit_is_picked_up() {
        let Some((resources, first)) = live_resources("swap1") else {
            return;
        };
        let storage = RedisCertStorage::new(resources.clone(), "acme-live", "test-secret");
        let (id, _) = CertId::from_domains(&["swap.example.com".into()]).unwrap();
        let cert = StoredCert {
            chain_pem: "OLD".into(),
            key_pem: "OLD-KEY".into(),
            issued_at: 1,
        };
        storage.save_cert(&id, &cert).await.unwrap();
        assert_eq!(storage.load_cert(&id).await.unwrap().unwrap(), cert);

        // Rebuild the registry for the same store name with a different
        // key_prefix and swap it in, exactly as an Admin API store edit does.
        let url = std::env::var("FEATHERBIT_TEST_REDIS_URL").unwrap();
        let cfg2 = store_cfg("swap2", &url);
        let registry2 =
            StoreRegistry::rebuild(&StoreRegistry::default(), std::slice::from_ref(&cfg2), None)
                .unwrap();
        let second = registry2.client("acme-live").unwrap();
        assert_ne!(first.key_prefix(), second.key_prefix());
        resources.stores.store(Arc::new(registry2));

        // The next read goes through the new client's namespace: the record
        // written under the old prefix is invisible.
        assert!(
            storage.load_cert(&id).await.unwrap().is_none(),
            "the next call must use the swapped-in client, not the original"
        );
        let fresh = StoredCert {
            chain_pem: "NEW".into(),
            key_pem: "NEW-KEY".into(),
            issued_at: 2,
        };
        storage.save_cert(&id, &fresh).await.unwrap();
        let mut conn = second.conn().await.unwrap();
        let raw: Option<String> = conn.get(cert_key(second.key_prefix(), &id)).await.unwrap();
        assert!(
            raw.is_some_and(|r| r.contains("NEW")),
            "the write landed under the new client's key_prefix"
        );

        // Dropping the store from the registry surfaces as a storage error,
        // not a panic or a stale success.
        resources.stores.store(Arc::new(StoreRegistry::default()));
        let err = storage.load_cert(&id).await.unwrap_err();
        assert!(
            matches!(err, AcmeError::Storage(ref m) if m.contains("acme-live")),
            "{err}"
        );
    }
}
