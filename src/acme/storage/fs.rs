//! Filesystem `CertStorage`.
//!
//! Layout under `dir`: `account.json`, `certs/<cert_id>/{chain.pem,key.pem,meta.json}`,
//! `challenges/<domain>` (`{key_auth, expires_at}`), `leases/<cert_id>`
//! (`{owner, expires_at}`). Every write is temp-file + rename; secret files are
//! `0600` on unix. Expired challenge/lease files read as absent.

use std::path::{Path, PathBuf};
use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use super::CertStorage;
use crate::acme::{now_unix, AcmeError, CertId, StoredCert};

pub struct FsCertStorage {
    dir: PathBuf,
}

#[derive(Serialize, Deserialize)]
struct ChallengeFile {
    key_auth: String,
    expires_at: i64,
}

#[derive(Serialize, Deserialize)]
struct LeaseFile {
    owner: String,
    expires_at: i64,
}

#[derive(Serialize, Deserialize)]
struct MetaFile {
    issued_at: i64,
}

fn io_err(what: &str, path: &Path, e: std::io::Error) -> AcmeError {
    AcmeError::Storage(format!("{what} {}: {e}", path.display()))
}

/// Writes `bytes` to `path` atomically (temp file beside it, then rename).
/// `secret` files get `0600` on unix before the rename.
fn atomic_write(path: &Path, bytes: &[u8], secret: bool) -> Result<(), AcmeError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| io_err("create dir", parent, e))?;
    }
    let mut tmp_name = path.file_name().unwrap_or_default().to_os_string();
    tmp_name.push(".tmp");
    let tmp = path.with_file_name(tmp_name);
    std::fs::write(&tmp, bytes).map_err(|e| io_err("write", &tmp, e))?;
    #[cfg(unix)]
    if secret {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))
            .map_err(|e| io_err("chmod", &tmp, e))?;
    }
    #[cfg(not(unix))]
    let _ = secret;
    if let Err(first) = std::fs::rename(&tmp, path) {
        // Windows refuses to rename over an existing file.
        std::fs::remove_file(path).map_err(|e| io_err("replace", path, e))?;
        std::fs::rename(&tmp, path).map_err(|_| io_err("rename", path, first))?;
    }
    Ok(())
}

fn read_opt(path: &Path) -> Result<Option<Vec<u8>>, AcmeError> {
    match std::fs::read(path) {
        Ok(b) => Ok(Some(b)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(io_err("read", path, e)),
    }
}

fn remove_opt(path: &Path) -> Result<(), AcmeError> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(io_err("remove", path, e)),
    }
}

fn expires_at(ttl: Duration) -> i64 {
    now_unix() + ttl.as_secs().max(1) as i64
}

impl FsCertStorage {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    fn cert_dir(&self, id: &CertId) -> PathBuf {
        self.dir.join("certs").join(id.as_str())
    }
    fn challenge_path(&self, domain: &str) -> PathBuf {
        self.dir.join("challenges").join(domain)
    }
    fn lease_path(&self, id: &CertId) -> PathBuf {
        self.dir.join("leases").join(id.as_str())
    }

    fn read_lease(&self, id: &CertId) -> Result<Option<LeaseFile>, AcmeError> {
        let Some(bytes) = read_opt(&self.lease_path(id))? else {
            return Ok(None);
        };
        let lease: LeaseFile = match serde_json::from_slice(&bytes) {
            Ok(l) => l,
            Err(_) => return Ok(None), // a corrupt lease is no lease
        };
        if lease.expires_at <= now_unix() {
            return Ok(None);
        }
        Ok(Some(lease))
    }

    fn write_lease(&self, id: &CertId, owner: &str, ttl: Duration) -> Result<(), AcmeError> {
        let lease = LeaseFile {
            owner: owner.to_string(),
            expires_at: expires_at(ttl),
        };
        atomic_write(
            &self.lease_path(id),
            &serde_json::to_vec(&lease).unwrap(),
            false,
        )
    }
}

#[async_trait]
impl CertStorage for FsCertStorage {
    fn label(&self) -> String {
        "filesystem".to_string()
    }

    async fn load_account(&self) -> Result<Option<Vec<u8>>, AcmeError> {
        read_opt(&self.dir.join("account.json"))
    }

    async fn save_account(&self, creds: &[u8]) -> Result<(), AcmeError> {
        atomic_write(&self.dir.join("account.json"), creds, true)
    }

    async fn load_cert(&self, id: &CertId) -> Result<Option<StoredCert>, AcmeError> {
        let dir = self.cert_dir(id);
        let (Some(chain), Some(key)) = (
            read_opt(&dir.join("chain.pem"))?,
            read_opt(&dir.join("key.pem"))?,
        ) else {
            return Ok(None);
        };
        let issued_at = read_opt(&dir.join("meta.json"))?
            .and_then(|b| serde_json::from_slice::<MetaFile>(&b).ok())
            .map(|m| m.issued_at)
            .unwrap_or(0);
        Ok(Some(StoredCert {
            chain_pem: String::from_utf8_lossy(&chain).into_owned(),
            key_pem: String::from_utf8_lossy(&key).into_owned(),
            issued_at,
        }))
    }

    async fn save_cert(&self, id: &CertId, cert: &StoredCert) -> Result<(), AcmeError> {
        let dir = self.cert_dir(id);
        // Key first, chain last: a reader that sees a chain always finds its key.
        atomic_write(&dir.join("key.pem"), cert.key_pem.as_bytes(), true)?;
        atomic_write(
            &dir.join("meta.json"),
            &serde_json::to_vec(&MetaFile {
                issued_at: cert.issued_at,
            })
            .unwrap(),
            false,
        )?;
        atomic_write(&dir.join("chain.pem"), cert.chain_pem.as_bytes(), false)
    }

    async fn put_challenge(
        &self,
        domain: &str,
        key_auth: &str,
        ttl: Duration,
    ) -> Result<(), AcmeError> {
        let file = ChallengeFile {
            key_auth: key_auth.to_string(),
            expires_at: expires_at(ttl),
        };
        atomic_write(
            &self.challenge_path(domain),
            &serde_json::to_vec(&file).unwrap(),
            false,
        )
    }

    async fn get_challenge(&self, domain: &str) -> Result<Option<String>, AcmeError> {
        let Some(bytes) = read_opt(&self.challenge_path(domain))? else {
            return Ok(None);
        };
        let file: ChallengeFile = match serde_json::from_slice(&bytes) {
            Ok(f) => f,
            Err(_) => return Ok(None),
        };
        if file.expires_at <= now_unix() {
            return Ok(None);
        }
        Ok(Some(file.key_auth))
    }

    async fn remove_challenge(&self, domain: &str) -> Result<(), AcmeError> {
        remove_opt(&self.challenge_path(domain))
    }

    async fn try_acquire_lease(
        &self,
        id: &CertId,
        owner: &str,
        ttl: Duration,
    ) -> Result<bool, AcmeError> {
        match self.read_lease(id)? {
            Some(l) if l.owner != owner => Ok(false),
            _ => {
                self.write_lease(id, owner, ttl)?;
                Ok(true)
            }
        }
    }

    async fn renew_lease(
        &self,
        id: &CertId,
        owner: &str,
        ttl: Duration,
    ) -> Result<bool, AcmeError> {
        match self.read_lease(id)? {
            Some(l) if l.owner == owner => {
                self.write_lease(id, owner, ttl)?;
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    async fn release_lease(&self, id: &CertId, owner: &str) -> Result<(), AcmeError> {
        match self.read_lease(id)? {
            Some(l) if l.owner == owner => remove_opt(&self.lease_path(id)),
            _ => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("fb_acme_{}_{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    #[tokio::test]
    async fn fs_storage_satisfies_contract() {
        let dir = temp_dir("contract");
        let storage = Arc::new(FsCertStorage::new(dir.clone()));
        assert_eq!(storage.label(), "filesystem");
        crate::acme::storage::contract::run_all(storage).await;
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn fs_layout_and_atomic_write() {
        let dir = temp_dir("layout");
        let storage = FsCertStorage::new(dir.clone());
        let (id, _) = CertId::from_domains(&["a.example.com".into()]).unwrap();
        storage
            .save_cert(
                &id,
                &StoredCert {
                    chain_pem: "C".into(),
                    key_pem: "K".into(),
                    issued_at: 1,
                },
            )
            .await
            .unwrap();
        let cert_dir = dir.join("certs").join("a.example.com");
        assert!(cert_dir.join("chain.pem").exists());
        assert!(cert_dir.join("key.pem").exists());
        assert!(cert_dir.join("meta.json").exists());
        assert!(!cert_dir.join("key.pem.tmp").exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(cert_dir.join("key.pem"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        let _ = std::fs::remove_dir_all(dir);
    }
}
