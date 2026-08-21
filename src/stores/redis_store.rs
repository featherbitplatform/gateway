//! Redis/Valkey client for named stores (`redis-store` feature).
//!
//! One [`RedisStoreClient`] per `stores:` entry: env placeholders in `url` /
//! `password` resolve here (point of use — the stored config stays raw), the
//! underlying connection is a single auto-reconnecting multiplexed
//! `ConnectionManager` created lazily on first use, and a resolved-config
//! fingerprint lets [`super::StoreRegistry::rebuild`] keep the connection
//! across unrelated config reloads.

use std::time::Duration;

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use redis::IntoConnectionInfo;
use ring::digest::{digest, SHA256};
use tokio::sync::OnceCell;

use crate::config::interpolate_env;
use crate::config::StoreConfig;

/// Result of a connectivity check (`POST /api/stores/{name}/ping`).
pub struct PingInfo {
    pub latency_ms: u64,
    /// `valkey_version` when the server is Valkey, else `redis_version`.
    pub version: String,
}

pub struct RedisStoreClient {
    name: String,
    key_prefix: String,
    fingerprint: String,
    connect_timeout: Duration,
    client: redis::Client,
    conn: OnceCell<redis::aio::ConnectionManager>,
}

// `redis::aio::ConnectionManager` has no `Debug` impl, so this can't be
// `#[derive(Debug)]`'d; a manual impl covering the non-connection fields is
// enough for test assertions (`Result::unwrap_err` requires `T: Debug`) and
// avoids ever printing connection internals or credentials.
impl std::fmt::Debug for RedisStoreClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RedisStoreClient")
            .field("name", &self.name)
            .field("key_prefix", &self.key_prefix)
            .field("connect_timeout", &self.connect_timeout)
            .finish_non_exhaustive()
    }
}

impl RedisStoreClient {
    /// Builds the client: resolves `${ENV}` in url/password, parses the URL,
    /// loads the CA bundle if configured. **No network I/O** — connection is
    /// deferred to [`Self::conn`], so config apply never blocks on a store.
    pub fn build(cfg: &StoreConfig) -> Result<Self, String> {
        let url = interpolate_env(&cfg.url);
        if url.trim().is_empty() {
            return Err(format!(
                "store '{}': url resolved to an empty string",
                cfg.name
            ));
        }
        let mut info = url
            .as_str()
            .into_connection_info()
            .map_err(|e| format!("store '{}': invalid url: {}", cfg.name, e))?;
        if let Some(pw) = cfg.password.as_deref() {
            let pw = interpolate_env(pw);
            if !pw.is_empty() {
                info.redis.password = Some(pw);
            }
        }
        let client = match cfg.tls.as_ref().and_then(|t| t.ca_cert_path.as_deref()) {
            Some(path) => {
                let pem = std::fs::read(path).map_err(|e| {
                    format!(
                        "store '{}': cannot read ca_cert_path '{}': {}",
                        cfg.name, path, e
                    )
                })?;
                redis::Client::build_with_tls(
                    info,
                    redis::TlsCertificates {
                        client_tls: None,
                        root_cert: Some(pem),
                    },
                )
                .map_err(|e| format!("store '{}': tls setup: {}", cfg.name, e))?
            }
            None => {
                redis::Client::open(info).map_err(|e| format!("store '{}': {}", cfg.name, e))?
            }
        };
        Ok(Self {
            name: cfg.name.clone(),
            key_prefix: cfg.key_prefix.clone(),
            fingerprint: Self::fingerprint_of(cfg),
            connect_timeout: Duration::from_millis(cfg.connect_timeout_ms),
            client,
            conn: OnceCell::new(),
        })
    }

    /// Fingerprint over the *resolved* connection-relevant fields, so a
    /// changed env var (not just changed YAML) rebuilds the client on the
    /// next reload. Hashed so no secret sits in an easily-dumped string.
    pub fn fingerprint_of(cfg: &StoreConfig) -> String {
        let material = format!(
            "{}|{}|{}|{}|{}",
            interpolate_env(&cfg.url),
            cfg.password
                .as_deref()
                .map(interpolate_env)
                .unwrap_or_default(),
            cfg.key_prefix,
            cfg.connect_timeout_ms,
            cfg.tls
                .as_ref()
                .and_then(|t| t.ca_cert_path.as_deref())
                .unwrap_or(""),
        );
        BASE64.encode(digest(&SHA256, material.as_bytes()))
    }

    /// The shared multiplexed connection; established on first use and
    /// auto-reconnecting thereafter.
    pub async fn conn(&self) -> Result<redis::aio::ConnectionManager, String> {
        let manager = self
            .conn
            .get_or_try_init(|| async {
                let cfg = redis::aio::ConnectionManagerConfig::new()
                    .set_connection_timeout(self.connect_timeout)
                    .set_response_timeout(self.connect_timeout);
                redis::aio::ConnectionManager::new_with_config(self.client.clone(), cfg).await
            })
            .await
            .map_err(|e| format!("store '{}': connect: {}", self.name, e))?;
        Ok(manager.clone())
    }

    /// `PING` + server version, for the Admin API connectivity check.
    pub async fn ping(&self) -> Result<PingInfo, String> {
        let mut conn = self.conn().await?;
        let start = std::time::Instant::now();
        let pong: String = redis::cmd("PING")
            .query_async(&mut conn)
            .await
            .map_err(|e| format!("store '{}': ping: {}", self.name, e))?;
        if pong != "PONG" {
            return Err(format!(
                "store '{}': unexpected PING reply '{}'",
                self.name, pong
            ));
        }
        let latency_ms = start.elapsed().as_millis() as u64;
        let info: String = redis::cmd("INFO")
            .arg("server")
            .query_async(&mut conn)
            .await
            .unwrap_or_default();
        let version = info
            .lines()
            .find_map(|l| {
                l.strip_prefix("valkey_version:")
                    .or_else(|| l.strip_prefix("redis_version:"))
            })
            .unwrap_or("unknown")
            .trim()
            .to_string();
        Ok(PingInfo {
            latency_ms,
            version,
        })
    }

    // Only this module's own tests call this — not yet used by any
    // production code path.
    #[allow(dead_code)]
    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn key_prefix(&self) -> &str {
        &self.key_prefix
    }

    pub fn fingerprint(&self) -> &str {
        &self.fingerprint
    }

    pub fn connect_timeout(&self) -> Duration {
        self.connect_timeout
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(yaml: &str) -> StoreConfig {
        serde_yaml::from_str(yaml).unwrap()
    }

    /// Env placeholders resolve at build; the config object stays raw; a
    /// changed env var changes the fingerprint.
    #[test]
    fn test_build_resolves_env_and_fingerprints() {
        std::env::set_var("STORE_T3_URL", "redis://127.0.0.1:6399");
        let c = cfg("name: s1\ntype: redis\nurl: ${STORE_T3_URL}\n");
        let built = RedisStoreClient::build(&c).unwrap();
        assert_eq!(built.name(), "s1");
        assert_eq!(built.key_prefix(), "fb");
        // Raw config untouched.
        assert_eq!(c.url, "${STORE_T3_URL}");

        let fp1 = RedisStoreClient::fingerprint_of(&c);
        std::env::set_var("STORE_T3_URL", "redis://127.0.0.1:6400");
        let fp2 = RedisStoreClient::fingerprint_of(&c);
        assert_ne!(fp1, fp2, "resolved env change must change the fingerprint");
        std::env::remove_var("STORE_T3_URL");
    }

    #[test]
    fn test_build_rejects_bad_url() {
        let c = cfg("name: s1\ntype: redis\nurl: 'not a url'\n");
        let err = RedisStoreClient::build(&c).unwrap_err();
        assert!(err.contains("store 's1'"), "{err}");
    }

    /// Live-backend test; skipped unless FEATHERBIT_TEST_REDIS_URL is set
    /// (e.g. `docker run -p 6379:6379 redis:7` then
    /// FEATHERBIT_TEST_REDIS_URL=redis://127.0.0.1:6379 cargo test).
    #[tokio::test]
    async fn test_ping_live() {
        let Ok(url) = std::env::var("FEATHERBIT_TEST_REDIS_URL") else {
            eprintln!("skipping test_ping_live: FEATHERBIT_TEST_REDIS_URL not set");
            return;
        };
        let c = cfg(&format!("name: live\ntype: redis\nurl: {url}\n"));
        let client = RedisStoreClient::build(&c).unwrap();
        let info = client.ping().await.unwrap();
        assert!(!info.version.is_empty());
    }
}
