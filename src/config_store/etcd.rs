//! etcd-backed config store for HA clustering.
//!
//! Talks to **etcd's v3 HTTP/JSON gateway** (the `/v3/kv/*` and
//! `/v3/auth/authenticate` endpoints) through the shared [`OutboundClient`] —
//! no gRPC, `protoc`, or `tonic` dependency. Config lives under per-resource
//! keys (`<prefix>/routes/<name>`, `<prefix>/policies/<name>`,
//! `<prefix>/consumers/<name>`), each value the resource's JSON. Every gateway
//! instance loads from the same prefix and a background poll task keeps the
//! cluster converged (see [`spawn_watch`]).
//!
//! # Route ordering
//! etcd returns keys in lexicographic order, so in etcd mode routes are matched
//! **by name order**, not file declaration order. Name routes accordingly when
//! precedence matters.
//!
//! # v1 limitations (documented follow-ups)
//! - Plaintext / no TLS to etcd (loopback or private-network etcd).
//! - Poll-based convergence (default 2 s) rather than a streaming watch.
//! - Uses the first endpoint; multi-endpoint failover is a later addition.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use bytes::Bytes;
use serde_json::{json, Value};
use tokio::sync::Mutex;

use crate::config::{EtcdConfig, GatewayConfig, SystemConfig};
use crate::config::{PolicyConfig, RouteConfig};
use crate::config_store::ConfigStore;
use crate::consumers::ConsumerConfig;
use crate::outbound::{OutboundClient, OutboundRequest};
use crate::state::SharedState;

/// etcd config store over the v3 HTTP/JSON gateway.
pub struct EtcdConfigStore {
    client: Arc<OutboundClient>,
    /// etcd base URLs (v1 uses the first).
    endpoints: Vec<String>,
    /// Key prefix (no trailing slash), e.g. `/featherbit`.
    prefix: String,
    auth: Option<(String, String)>,
    /// Cached auth token, refreshed on 401.
    token: Mutex<Option<String>>,
    timeout: Duration,
}

impl EtcdConfigStore {
    /// Builds the store from `system.yaml`'s etcd settings.
    pub fn new(cfg: &EtcdConfig) -> Self {
        Self {
            client: Arc::new(OutboundClient::new()),
            endpoints: cfg.endpoints.clone(),
            prefix: cfg.prefix.trim_end_matches('/').to_string(),
            auth: match (&cfg.user, &cfg.password) {
                (Some(u), Some(p)) => Some((u.clone(), p.clone())),
                _ => None,
            },
            token: Mutex::new(None),
            timeout: Duration::from_millis(cfg.timeout_ms),
        }
    }

    fn base(&self) -> Result<&str, String> {
        self.endpoints
            .first()
            .map(String::as_str)
            .ok_or_else(|| "no etcd endpoints configured".to_string())
    }

    /// POSTs `body` to an etcd JSON endpoint, attaching the auth token and
    /// re-authenticating once on 401.
    async fn call(&self, path: &str, body: &Value) -> Result<Value, String> {
        match self.call_once(path, body).await {
            Err(EtcdCallError::Unauthorized) if self.auth.is_some() => {
                self.authenticate().await?;
                self.call_once(path, body).await.map_err(|e| e.to_string())
            }
            other => other.map_err(|e| e.to_string()),
        }
    }

    async fn call_once(&self, path: &str, body: &Value) -> Result<Value, EtcdCallError> {
        let url = format!("{}{}", self.base().map_err(EtcdCallError::Other)?, path);
        let mut headers = vec![("content-type".to_string(), "application/json".to_string())];
        if let Some(token) = self.token.lock().await.clone() {
            headers.push(("authorization".to_string(), token));
        }
        let req = OutboundRequest {
            method: http::Method::POST,
            url,
            headers,
            body: Bytes::from(serde_json::to_vec(body).unwrap_or_default()),
            timeout: self.timeout,
            ssl_verify: true,
        };
        let resp = self
            .client
            .request(req)
            .await
            .map_err(|e| EtcdCallError::Other(e.to_string()))?;
        if resp.status == 401 {
            return Err(EtcdCallError::Unauthorized);
        }
        if resp.status != 200 {
            return Err(EtcdCallError::Other(format!(
                "etcd {} returned status {}: {}",
                path,
                resp.status,
                String::from_utf8_lossy(&resp.body)
            )));
        }
        serde_json::from_slice(&resp.body)
            .map_err(|e| EtcdCallError::Other(format!("invalid etcd response: {}", e)))
    }

    /// Authenticates and caches the token.
    async fn authenticate(&self) -> Result<(), String> {
        let (user, pass) = match &self.auth {
            Some(c) => c,
            None => return Ok(()),
        };
        let resp = self
            .call_once(
                "/v3/auth/authenticate",
                &json!({ "name": user, "password": pass }),
            )
            .await
            .map_err(|e| e.to_string())?;
        let token = resp
            .get("token")
            .and_then(|v| v.as_str())
            .ok_or("etcd auth response missing token")?;
        *self.token.lock().await = Some(token.to_string());
        Ok(())
    }

    /// Ranges all keys under the prefix, returning `(key, value_bytes)` pairs.
    async fn range_prefix(&self) -> Result<Vec<(String, Vec<u8>)>, String> {
        let key = format!("{}/", self.prefix);
        let range_end = prefix_range_end(key.as_bytes());
        let resp = self
            .call(
                "/v3/kv/range",
                &json!({
                    "key": BASE64.encode(key.as_bytes()),
                    "range_end": BASE64.encode(range_end),
                }),
            )
            .await?;
        let mut out = Vec::new();
        if let Some(kvs) = resp.get("kvs").and_then(|v| v.as_array()) {
            for kv in kvs {
                let k = kv.get("key").and_then(|v| v.as_str()).unwrap_or("");
                let v = kv.get("value").and_then(|v| v.as_str()).unwrap_or("");
                let key = BASE64
                    .decode(k)
                    .ok()
                    .and_then(|b| String::from_utf8(b).ok())
                    .ok_or("etcd key not valid base64/utf8")?;
                let value = BASE64
                    .decode(v)
                    .map_err(|_| "etcd value not valid base64")?;
                out.push((key, value));
            }
        }
        Ok(out)
    }

    async fn put(&self, key: &str, value: &[u8]) -> Result<(), String> {
        self.call(
            "/v3/kv/put",
            &json!({ "key": BASE64.encode(key.as_bytes()), "value": BASE64.encode(value) }),
        )
        .await
        .map(|_| ())
    }

    async fn delete(&self, key: &str) -> Result<(), String> {
        self.call(
            "/v3/kv/deleterange",
            &json!({ "key": BASE64.encode(key.as_bytes()) }),
        )
        .await
        .map(|_| ())
    }

    fn route_key(&self, name: &str) -> String {
        format!("{}/routes/{}", self.prefix, name)
    }
    fn policy_key(&self, name: &str) -> String {
        format!("{}/policies/{}", self.prefix, name)
    }
    fn consumer_key(&self, name: &str) -> String {
        format!("{}/consumers/{}", self.prefix, name)
    }

    /// Writes every resource in `gw` to etcd (used to seed an empty prefix).
    async fn write_all(&self, gw: &GatewayConfig) -> Result<(), String> {
        for r in &gw.routes {
            self.put(&self.route_key(&r.name), &serde_json::to_vec(r).unwrap())
                .await?;
        }
        for p in &gw.policies {
            self.put(&self.policy_key(&p.name), &serde_json::to_vec(p).unwrap())
                .await?;
        }
        for c in &gw.consumers {
            self.put(&self.consumer_key(&c.name), &serde_json::to_vec(c).unwrap())
                .await?;
        }
        Ok(())
    }
}

#[async_trait]
impl ConfigStore for EtcdConfigStore {
    async fn load_all(&self) -> Result<GatewayConfig, String> {
        let kvs = self.range_prefix().await?;
        gateway_from_kvs(&self.prefix, kvs)
    }

    async fn commit(&self, state: &SharedState, candidate: GatewayConfig) -> Result<(), String> {
        // 1. Reject invalid config before touching etcd (synchronous 400).
        state.validate_gateway(&candidate)?;

        // 2. Reconcile etcd to match the candidate: put all desired keys, then
        //    delete any keys under the prefix that the candidate dropped.
        let current: std::collections::HashSet<String> = self
            .range_prefix()
            .await?
            .into_iter()
            .map(|(k, _)| k)
            .collect();
        let mut desired = std::collections::HashSet::new();

        for r in &candidate.routes {
            let key = self.route_key(&r.name);
            self.put(&key, &serde_json::to_vec(r).unwrap()).await?;
            desired.insert(key);
        }
        for p in &candidate.policies {
            let key = self.policy_key(&p.name);
            self.put(&key, &serde_json::to_vec(p).unwrap()).await?;
            desired.insert(key);
        }
        for c in &candidate.consumers {
            let key = self.consumer_key(&c.name);
            self.put(&key, &serde_json::to_vec(c).unwrap()).await?;
            desired.insert(key);
        }
        for stale in current.difference(&desired) {
            self.delete(stale).await?;
        }

        // 3. Apply locally so the writing node reflects the change immediately;
        //    other nodes converge on their next poll. Idempotent with the poll.
        state.apply_gateway(candidate).await
    }
}

/// Assembles a [`GatewayConfig`] from the etcd key/value pairs under `prefix`.
///
/// Keys are `<prefix>/{routes,policies,consumers}/<name>`; values are the
/// resource JSON. Unknown key shapes are skipped. Malformed resource JSON is an
/// error (so a bad write surfaces rather than silently dropping config).
fn gateway_from_kvs(prefix: &str, kvs: Vec<(String, Vec<u8>)>) -> Result<GatewayConfig, String> {
    let mut gw = GatewayConfig {
        routes: Vec::new(),
        policies: Vec::new(),
        consumers: Vec::new(),
    };
    for (key, value) in kvs {
        let rest = match key.strip_prefix(&format!("{}/", prefix)) {
            Some(r) => r,
            None => continue,
        };
        let (category, _name) = match rest.split_once('/') {
            Some(p) => p,
            None => continue,
        };
        match category {
            "routes" => {
                let r: RouteConfig = serde_json::from_slice(&value)
                    .map_err(|e| format!("bad route '{}': {}", key, e))?;
                gw.routes.push(r);
            }
            "policies" => {
                let p: PolicyConfig = serde_json::from_slice(&value)
                    .map_err(|e| format!("bad policy '{}': {}", key, e))?;
                gw.policies.push(p);
            }
            "consumers" => {
                let c: ConsumerConfig = serde_json::from_slice(&value)
                    .map_err(|e| format!("bad consumer '{}': {}", key, e))?;
                gw.consumers.push(c);
            }
            _ => {}
        }
    }
    Ok(gw)
}

/// Computes etcd's prefix range-end: the prefix with its last non-`0xff` byte
/// incremented (so a Range covers all keys starting with the prefix). An
/// all-`0xff` prefix ranges to the end of the keyspace (`[0]`).
fn prefix_range_end(prefix: &[u8]) -> Vec<u8> {
    let mut end = prefix.to_vec();
    while let Some(&last) = end.last() {
        if last < 0xff {
            *end.last_mut().unwrap() = last + 1;
            return end;
        }
        end.pop();
    }
    vec![0]
}

fn is_empty(gw: &GatewayConfig) -> bool {
    gw.routes.is_empty() && gw.policies.is_empty() && gw.consumers.is_empty()
}

/// Builds the etcd store and the initial gateway config.
///
/// Seeds etcd from the local `gateway.yaml` (`seed_path`) when the etcd prefix
/// is empty, then loads from etcd. Returns `config_path = None` (etcd mode does
/// not reload from disk).
pub async fn build_source(
    system: &SystemConfig,
    seed_path: &std::path::Path,
) -> Result<(Arc<dyn ConfigStore>, GatewayConfig, Option<PathBuf>), String> {
    let cfg = system
        .config
        .etcd
        .as_ref()
        .ok_or("config.source is 'etcd' but no 'config.etcd' block is set")?;
    let store = Arc::new(EtcdConfigStore::new(cfg));
    if store.auth.is_some() {
        store.authenticate().await?;
    }

    let mut gateway = store.load_all().await?;
    if is_empty(&gateway) {
        if let Ok(local) = crate::config::load_yaml_with_env::<GatewayConfig>(seed_path) {
            if !is_empty(&local) {
                tracing::info!("etcd prefix empty — seeding from {}", seed_path.display());
                store.write_all(&local).await?;
                gateway = store.load_all().await?;
            }
        }
    }

    let store: Arc<dyn ConfigStore> = store;
    Ok((store, gateway, None))
}

/// Spawns the poll-based convergence task: every `poll_interval` (default 2 s)
/// it reloads from etcd and, when the config changed, applies it. Errors are
/// logged and retried on the next tick — a transient etcd outage leaves the
/// last-good config serving.
pub fn spawn_watch(state: Arc<SharedState>, system: &SystemConfig) {
    let interval = Duration::from_secs(2);
    let store = state.config_store.clone();
    tokio::spawn(async move {
        let mut last: Option<String> = None;
        loop {
            tokio::time::sleep(interval).await;
            match store.load_all().await {
                Ok(gw) => {
                    let fingerprint = serde_json::to_string(&gw).unwrap_or_default();
                    if last.as_deref() != Some(fingerprint.as_str()) {
                        match state.apply_gateway(gw).await {
                            Ok(_) => last = Some(fingerprint),
                            Err(e) => tracing::error!("etcd config apply failed: {}", e),
                        }
                    }
                }
                Err(e) => tracing::warn!("etcd poll failed (keeping last-good config): {}", e),
            }
        }
    });
    let _ = system; // reserved for future per-source poll-interval config
}

/// Internal call-error type distinguishing a 401 (triggers re-auth) from other
/// failures.
enum EtcdCallError {
    Unauthorized,
    Other(String),
}

impl std::fmt::Display for EtcdCallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unauthorized => write!(f, "etcd unauthorized"),
            Self::Other(m) => write!(f, "{}", m),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_prefix_range_end() {
        assert_eq!(prefix_range_end(b"/featherbit/"), b"/featherbit0".to_vec()); // '/'+1 = '0'
        assert_eq!(prefix_range_end(b"ab"), b"ac".to_vec());
        assert_eq!(prefix_range_end(&[0xff, 0xff]), vec![0]);
        assert_eq!(prefix_range_end(&[0x01, 0xff]), vec![0x02]);
    }

    #[test]
    fn test_gateway_from_kvs() {
        let prefix = "/featherbit";
        let route = serde_json::to_vec(&json!({
            "name": "r", "match": { "path": "/api/*" }, "policy": "p"
        }))
        .unwrap();
        let policy = serde_json::to_vec(&json!({
            "name": "p",
            "nodes": [{ "id": "listener", "type": "listener" }, { "id": "client", "type": "client" }],
            "edges": [{ "from": "listener.out", "to": "client.in" }]
        }))
        .unwrap();
        let consumer = serde_json::to_vec(&json!({ "name": "alice" })).unwrap();

        let kvs = vec![
            ("/featherbit/routes/r".to_string(), route),
            ("/featherbit/policies/p".to_string(), policy),
            ("/featherbit/consumers/alice".to_string(), consumer),
            ("/featherbit/unknown/x".to_string(), b"{}".to_vec()), // skipped
            ("/other/routes/z".to_string(), b"{}".to_vec()),       // wrong prefix, skipped
        ];
        let gw = gateway_from_kvs(prefix, kvs).unwrap();
        assert_eq!(gw.routes.len(), 1);
        assert_eq!(gw.routes[0].name, "r");
        assert_eq!(gw.policies.len(), 1);
        assert_eq!(gw.consumers.len(), 1);
        assert_eq!(gw.consumers[0].name, "alice");
    }

    #[test]
    fn test_gateway_from_kvs_rejects_bad_json() {
        let kvs = vec![("/featherbit/routes/r".to_string(), b"not json".to_vec())];
        assert!(gateway_from_kvs("/featherbit", kvs).is_err());
    }

    #[test]
    fn test_is_empty() {
        assert!(is_empty(&GatewayConfig {
            routes: vec![],
            policies: vec![],
            consumers: vec![]
        }));
    }
}
