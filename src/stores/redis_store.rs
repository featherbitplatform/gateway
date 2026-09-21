//! Redis/Valkey client for named stores (`redis-store` feature).
//!
//! One [`RedisStoreClient`] per `stores:` entry: env placeholders in `url` /
//! `password` resolve here (point of use — the stored config stays raw), the
//! underlying connection is a single auto-reconnecting multiplexed
//! `ConnectionManager` created lazily on first use and handed out as a
//! [`StoreConn`] that bounds every command by `connect_budget`, and a
//! resolved-config fingerprint lets [`super::StoreRegistry::rebuild`] keep
//! the connection across unrelated config reloads.

use std::time::Duration;

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use redis::IntoConnectionInfo;
use ring::digest::{digest, SHA256};
use tokio::sync::OnceCell;

use crate::config::interpolate_env;
use crate::config::StoreConfig;

/// The connection handed to every store consumer.
///
/// A thin wrapper over the shared `ConnectionManager` whose every command is
/// bounded by the store's `connect_budget`. The manager alone bounds only the
/// *first* connection: once the store goes away mid-life, each command awaits
/// the manager's shared reconnect future -- six exponentially backed-off
/// attempts, each up to `connect_timeout` -- and nothing in the crate caps
/// that total. Measured against a stopped container, the second request
/// after an outage held its worker for ~55s before the `503` the design
/// promises. The budget is the one figure an operator can reason about, so it
/// bounds every wait for the store, not just the opening one.
#[derive(Clone)]
pub struct StoreConn {
    inner: redis::aio::ConnectionManager,
    budget: Duration,
    name: String,
}

impl StoreConn {
    fn gave_up(&self) -> redis::RedisError {
        redis::RedisError::from((
            redis::ErrorKind::IoError,
            "store operation gave up",
            format!(
                "store '{}': no connection within {}ms (connect_budget_ms)",
                self.name,
                self.budget.as_millis()
            ),
        ))
    }
}

impl redis::aio::ConnectionLike for StoreConn {
    fn req_packed_command<'a>(
        &'a mut self,
        cmd: &'a redis::Cmd,
    ) -> redis::RedisFuture<'a, redis::Value> {
        Box::pin(async move {
            match tokio::time::timeout(self.budget, self.inner.req_packed_command(cmd)).await {
                Ok(result) => result,
                Err(_) => Err(self.gave_up()),
            }
        })
    }

    fn req_packed_commands<'a>(
        &'a mut self,
        cmd: &'a redis::Pipeline,
        offset: usize,
        count: usize,
    ) -> redis::RedisFuture<'a, Vec<redis::Value>> {
        Box::pin(async move {
            match tokio::time::timeout(
                self.budget,
                self.inner.req_packed_commands(cmd, offset, count),
            )
            .await
            {
                Ok(result) => result,
                Err(_) => Err(self.gave_up()),
            }
        })
    }

    fn get_db(&self) -> i64 {
        self.inner.get_db()
    }
}

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
    connect_budget: Duration,
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
            connect_budget: Duration::from_millis(cfg.connect_budget_ms),
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
    /// auto-reconnecting thereafter. Every command on it is bounded by
    /// `connect_budget` -- see [`StoreConn`].
    pub async fn conn(&self) -> Result<StoreConn, String> {
        let manager = self
            .conn
            .get_or_try_init(|| async {
                let cfg = redis::aio::ConnectionManagerConfig::new()
                    .set_connection_timeout(self.connect_timeout)
                    .set_response_timeout(self.connect_timeout)
                    // No individual backoff may outlast the budget itself.
                    .set_max_delay(self.connect_budget.as_millis() as u64);

                // `connect_timeout` bounds one attempt; it says nothing about
                // the retry schedule around them. With the crate defaults that
                // is six attempts separated by 100/200/400/800/1600/3200ms of
                // backoff, none of it capped -- so an unreachable store held
                // the request open for tens of seconds before the `503` the
                // design promises, on the first request after an outage began.
                // The budget bounds connect + retries + waits together, which
                // is the only figure an operator can actually reason about.
                match tokio::time::timeout(
                    self.connect_budget,
                    redis::aio::ConnectionManager::new_with_config(self.client.clone(), cfg),
                )
                .await
                {
                    Ok(result) => result.map_err(|e| e.to_string()),
                    Err(_) => Err(format!(
                        "gave up after {}ms (connect_budget_ms)",
                        self.connect_budget.as_millis()
                    )),
                }
            })
            .await
            .map_err(|e| format!("store '{}': connect: {}", self.name, e))?;
        Ok(StoreConn {
            inner: manager.clone(),
            budget: self.connect_budget,
            name: self.name.clone(),
        })
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

    /// A store that cannot be reached must give up inside its budget rather
    /// than working through the connection manager's retry schedule.
    ///
    /// Nothing listens on port 1, so every attempt is refused immediately and
    /// the elapsed time is almost entirely the crate's exponential backoff:
    /// with the defaults that is 100+200+400+800+1600+3200ms of waiting
    /// between six attempts. The budget has to cut that short, because this
    /// happens on the first request after an outage begins -- exactly when a
    /// gateway should shed load fastest, not hold the request open.
    #[tokio::test]
    async fn test_connect_gives_up_inside_its_budget() {
        let c = cfg("name: s1
type: redis
url: redis://127.0.0.1:1
connect_budget_ms: 300
");
        let client = RedisStoreClient::build(&c).unwrap();

        let started = std::time::Instant::now();
        // `ConnectionManager` has no `Debug`, so `unwrap_err()` is unavailable.
        let err = match client.conn().await {
            Ok(_) => panic!("connecting to a closed port must fail"),
            Err(e) => e,
        };
        let elapsed = started.elapsed();

        assert!(
            elapsed < Duration::from_millis(2000),
            "connect must abandon inside its budget, not run the full retry schedule: took {elapsed:?}"
        );
        assert!(err.contains("store 's1'"), "{err}");
    }

    /// A failed connect must not be cached: the `OnceCell` stays uninitialised
    /// so the next request tries again, rather than a single outage poisoning
    /// the store for the process's lifetime.
    #[tokio::test]
    async fn test_a_failed_connect_is_not_cached() {
        let c = cfg("name: s1
type: redis
url: redis://127.0.0.1:1
connect_budget_ms: 300
");
        let client = RedisStoreClient::build(&c).unwrap();

        assert!(client.conn().await.is_err());
        let started = std::time::Instant::now();
        assert!(client.conn().await.is_err());
        assert!(
            started.elapsed() > Duration::from_millis(50),
            "a second attempt must actually retry, not return a cached failure instantly"
        );
    }

    /// The budget has a default, so an existing config with no new key is
    /// still bounded rather than inheriting the crate's unbounded schedule.
    #[test]
    fn test_connect_budget_defaults() {
        let c = cfg("name: s1
type: redis
url: redis://127.0.0.1:6379
");
        assert_eq!(c.connect_budget_ms, 5000);
    }

    /// A TCP relay in front of the live redis that the test can kill, so the
    /// gateway's connection sees the store vanish mid-life -- the case
    /// `test_connect_gives_up_inside_its_budget` cannot reach, because it
    /// never gets a connection in the first place.
    struct KillableProxy {
        port: u16,
        relays: std::sync::Arc<std::sync::Mutex<Vec<tokio::task::JoinHandle<()>>>>,
        accept: tokio::task::JoinHandle<()>,
    }

    impl KillableProxy {
        async fn start(target: String) -> Self {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let port = listener.local_addr().unwrap().port();
            let relays = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
            let relays_for_accept = relays.clone();
            let accept = tokio::spawn(async move {
                loop {
                    let Ok((mut inbound, _)) = listener.accept().await else {
                        return;
                    };
                    let target = target.clone();
                    let task = tokio::spawn(async move {
                        let Ok(mut outbound) = tokio::net::TcpStream::connect(target).await else {
                            return;
                        };
                        let _ = tokio::io::copy_bidirectional(&mut inbound, &mut outbound).await;
                    });
                    relays_for_accept.lock().unwrap().push(task);
                }
            });
            Self {
                port,
                relays,
                accept,
            }
        }

        /// Stops accepting and aborts every relay, which drops both ends of
        /// each relayed socket: from the client's side the store has gone
        /// away and its port now refuses connections.
        fn kill(self) {
            self.accept.abort();
            for t in self.relays.lock().unwrap().drain(..) {
                t.abort();
            }
        }
    }

    /// The budget must bound every store operation, not just the first
    /// connection. After an outage the connection manager reconnects with
    /// six exponentially backed-off attempts; a command issued meanwhile
    /// awaits that whole schedule -- measured at ~55s against a stopped
    /// container -- long after `connect_budget_ms` says the store should
    /// have been given up on.
    ///
    /// Live-backend test; skipped unless FEATHERBIT_TEST_REDIS_URL is set.
    #[tokio::test]
    async fn test_a_command_during_reconnect_gives_up_inside_its_budget() {
        let Ok(url) = std::env::var("FEATHERBIT_TEST_REDIS_URL") else {
            eprintln!("skipping: FEATHERBIT_TEST_REDIS_URL not set");
            return;
        };
        let target = url
            .trim_start_matches("redis://")
            .trim_end_matches('/')
            .to_string();
        let proxy = KillableProxy::start(target).await;
        let port = proxy.port;

        let c = cfg(&format!(
            "name: s1
type: redis
url: redis://127.0.0.1:{port}
connect_timeout_ms: 200
connect_budget_ms: 300
"
        ));
        let client = RedisStoreClient::build(&c).unwrap();
        let mut conn = client.conn().await.expect("connect through the relay");
        let pong: String = redis::cmd("PING").query_async(&mut conn).await.unwrap();
        assert_eq!(pong, "PONG");

        proxy.kill();

        // The first command after the outage fails fast on the dead socket
        // and starts the reconnect; the second is the one that used to wait
        // out the manager's entire retry schedule.
        let _ = redis::cmd("PING")
            .query_async::<String>(&mut conn)
            .await
            .expect_err("the socket is gone");
        let started = std::time::Instant::now();
        let err = redis::cmd("PING")
            .query_async::<String>(&mut conn)
            .await
            .expect_err("nothing listens on the relay port any more");
        let elapsed = started.elapsed();

        assert!(
            elapsed < Duration::from_millis(1500),
            "a command during reconnect must give up inside connect_budget_ms, not wait out the retry schedule: took {elapsed:?} ({err})"
        );
    }
}
