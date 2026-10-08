//! etcd-backed config store for HA clustering.
//!
//! Talks to **etcd's v3 HTTP/JSON gateway** (the `/v3/kv/*` and
//! `/v3/auth/authenticate` endpoints) through the shared [`OutboundClient`] —
//! no gRPC, `protoc`, or `tonic` dependency. Config lives under per-resource
//! keys (`<prefix>/routes/<name>`, `<prefix>/policies/<name>`,
//! `<prefix>/consumers/<name>`, `<prefix>/supernodes/<name>`, `<prefix>/plugin_configs/<name>`,
//! `<prefix>/stores/<name>`),
//! each value the resource's JSON. Every gateway instance loads from the same prefix and a
//! background poll task keeps the cluster converged (see [`spawn_watch`]).
//! Note: an older build sharing the same prefix garbage-collects unknown key
//! families on its next commit.
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

use crate::config::{EtcdConfig, GatewayConfig, PluginConfigDef, StoreConfig, SystemConfig};
use crate::config::{PolicyConfig, RouteConfig, SupernodeConfig};
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
            tls: None,
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
    fn supernode_key(&self, name: &str) -> String {
        format!("{}/supernodes/{}", self.prefix, name)
    }
    fn plugin_config_key(&self, name: &str) -> String {
        format!("{}/plugin_configs/{}", self.prefix, name)
    }
    fn store_key(&self, name: &str) -> String {
        format!("{}/stores/{}", self.prefix, name)
    }

    /// The key/value pairs `gw` maps to under this store's prefix: one JSON
    /// document per resource, under `routes/`, `policies/`, `consumers/`,
    /// `supernodes/`, `plugin_configs/` and `stores/`. The write-side mirror
    /// of [`gateway_from_kvs`].
    fn desired_kvs(&self, gw: &GatewayConfig) -> Vec<(String, Vec<u8>)> {
        let mut kvs = Vec::new();
        for r in &gw.routes {
            kvs.push((self.route_key(&r.name), serde_json::to_vec(r).unwrap()));
        }
        for p in &gw.policies {
            kvs.push((self.policy_key(&p.name), serde_json::to_vec(p).unwrap()));
        }
        for c in &gw.consumers {
            kvs.push((self.consumer_key(&c.name), serde_json::to_vec(c).unwrap()));
        }
        for s in &gw.supernodes {
            kvs.push((self.supernode_key(&s.name), serde_json::to_vec(s).unwrap()));
        }
        for pc in &gw.plugin_configs {
            kvs.push((
                self.plugin_config_key(&pc.name),
                serde_json::to_vec(pc).unwrap(),
            ));
        }
        for s in &gw.stores {
            kvs.push((self.store_key(&s.name), serde_json::to_vec(s).unwrap()));
        }
        kvs
    }

    /// Puts every resource of `gw` (no deletes). Used by the first-boot seeder.
    async fn write_all(&self, gw: &GatewayConfig) -> Result<(), String> {
        for (key, value) in self.desired_kvs(gw) {
            self.put(&key, &value).await?;
        }
        Ok(())
    }

    /// Makes the prefix hold exactly `desired`: puts every desired key, then
    /// deletes the keys under the prefix that `desired` no longer contains.
    /// Only keys in the gateway's own layout (see [`parse_resource_key`]) are
    /// ever deleted; anything else under the prefix is left alone.
    ///
    /// Not transactional — a reader polling mid-way may see a mix of old and
    /// new documents. The gateway loads the prefix in one range request and
    /// either applies that snapshot as a whole or rejects it and keeps its
    /// last-good config, then retries on the next poll.
    async fn reconcile(&self, desired: &GatewayConfig) -> Result<(), String> {
        let current: Vec<String> = self
            .range_prefix()
            .await?
            .into_iter()
            .map(|(k, _)| k)
            .collect();
        let mut kept = std::collections::HashSet::new();
        for (key, value) in self.desired_kvs(desired) {
            self.put(&key, &value).await?;
            kept.insert(key);
        }
        for stale in stale_keys(&self.prefix, &current, &kept) {
            self.delete(stale).await?;
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
        // 2. Reconcile etcd to match the candidate.
        self.reconcile(&candidate).await?;
        // 3. Apply locally so the writing node reflects the change immediately;
        //    other nodes converge on their next poll. Idempotent with the poll.
        state.apply_gateway(candidate).await
    }
}

/// Makes the etcd prefix described by `cfg` hold exactly `desired`, with no
/// gateway runtime involved. The Kubernetes operator's etcd sink calls this so
/// the key layout and write sequence stay the gateway's own code. The caller
/// is responsible for validating `desired` first
/// ([`crate::state::validate_gateway_config`]); every gateway polling the
/// prefix re-validates on apply and keeps its last-good config on failure.
///
/// Fails without any I/O when the prefix is empty (`""` or `/`): that would
/// make the gateway's key range the whole keyspace.
pub async fn reconcile_prefix(cfg: &EtcdConfig, desired: &GatewayConfig) -> Result<(), String> {
    let store = EtcdConfigStore::new(cfg);
    if store.prefix.is_empty() {
        return Err(format!(
            "etcd prefix {:?} is empty after trimming; refusing to reconcile the whole keyspace",
            cfg.prefix
        ));
    }
    if store.auth.is_some() {
        store.authenticate().await?;
    }
    store.reconcile(desired).await
}

/// The resource families under a prefix, in key order.
const RESOURCE_FAMILIES: [&str; 6] = [
    "routes",
    "policies",
    "consumers",
    "supernodes",
    "plugin_configs",
    "stores",
];

/// Recognises a gateway resource key: `<prefix>/<family>/<name>` where family
/// is one of [`RESOURCE_FAMILIES`]. Returns `(family, name)`; anything else
/// (other prefix, nested prefix such as `<prefix>/b/routes/x`, unknown family,
/// no name separator) is `None`. Shared by the reader and the stale-key filter
/// so the two cannot drift.
fn parse_resource_key<'a>(prefix: &str, key: &'a str) -> Option<(&'a str, &'a str)> {
    let rest = key.strip_prefix(prefix)?.strip_prefix('/')?;
    let (family, name) = rest.split_once('/')?;
    RESOURCE_FAMILIES
        .contains(&family)
        .then_some((family, name))
}

/// The keys in `current` that are gateway resource keys under `prefix` but not
/// in `kept`: the only keys `reconcile` may delete.
fn stale_keys<'a>(
    prefix: &str,
    current: &'a [String],
    kept: &std::collections::HashSet<String>,
) -> Vec<&'a String> {
    current
        .iter()
        .filter(|k| parse_resource_key(prefix, k).is_some() && !kept.contains(*k))
        .collect()
}

/// Assembles a [`GatewayConfig`] from the etcd key/value pairs under `prefix`.
///
/// Keys are `<prefix>/{routes,policies,consumers,supernodes,plugin_configs,stores}/<name>`; values
/// are the resource JSON. Unknown key shapes are skipped. Malformed resource
/// JSON is an error (so a bad write surfaces rather than silently dropping
/// config).
fn gateway_from_kvs(prefix: &str, kvs: Vec<(String, Vec<u8>)>) -> Result<GatewayConfig, String> {
    let mut gw = GatewayConfig {
        routes: Vec::new(),
        policies: Vec::new(),
        consumers: Vec::new(),
        supernodes: Vec::new(),
        plugin_configs: Vec::new(),
        stores: Vec::new(),
    };
    for (key, value) in kvs {
        let (category, name) = match parse_resource_key(prefix, &key) {
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
            "supernodes" => {
                let s: SupernodeConfig = serde_json::from_slice(&value)
                    .map_err(|e| format!("bad supernode '{}': {}", key, e))?;
                gw.supernodes.push(s);
            }
            "plugin_configs" => {
                let pc: PluginConfigDef = serde_json::from_slice(&value)
                    .map_err(|e| format!("bad plugin config '{}': {}", key, e))?;
                gw.plugin_configs.push(pc);
            }
            "stores" => {
                let s: StoreConfig = serde_json::from_str(&String::from_utf8_lossy(&value))
                    .map_err(|e| format!("bad store '{}': {}", name, e))?;
                gw.stores.push(s);
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
    gw.routes.is_empty()
        && gw.policies.is_empty()
        && gw.consumers.is_empty()
        && gw.supernodes.is_empty()
        && gw.plugin_configs.is_empty()
        && gw.stores.is_empty()
}

/// Builds the etcd store and the initial gateway config.
///
/// Seeds etcd from the local `gateway.yaml` (`seed_path`) when the etcd prefix
/// is empty, then loads from etcd. Returns `config_path = None` (etcd mode does
/// not reload from disk).
///
/// The seed candidate is **dry-run compiled** before anything is written (see
/// [`crate::state::validate_gateway_config`]). A local config that does not
/// compile is logged and skipped, leaving the prefix empty: startup then
/// proceeds with no routes, and the next boot re-seeds once the file is fixed.
/// Writing it anyway would publish a config the whole cluster then fails to
/// apply — and, because the prefix would no longer be empty, no later boot
/// would ever re-seed it.
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
        // Raw load: seeding must publish the `${VAR}` placeholder form, never
        // locally-resolved secrets — every cluster node resolves its own env
        // at compile time.
        if let Ok(local) = crate::config::load_yaml::<GatewayConfig>(seed_path) {
            if !is_empty(&local) {
                // Never publish a config the cluster cannot apply: compile it
                // locally first and skip the seed on failure, so the prefix
                // stays empty and a later boot can re-seed a fixed file.
                match crate::state::validate_gateway_config(&local) {
                    Ok(()) => {
                        tracing::info!("etcd prefix empty — seeding from {}", seed_path.display());
                        store.write_all(&local).await?;
                        gateway = store.load_all().await?;
                    }
                    Err(e) => tracing::error!(
                        "etcd prefix empty but the seed config at {} does not compile — \
                         NOT seeding (fix it and restart; the prefix stays empty so the \
                         next boot re-seeds): {}",
                        seed_path.display(),
                        e
                    ),
                }
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
            consumers: vec![],
            supernodes: vec![],
            plugin_configs: vec![],
            stores: vec![],
        }));
    }

    #[test]
    fn test_gateway_from_kvs_parses_supernodes() {
        let sn = serde_json::json!({
            "name": "secured-call",
            "nodes": [ { "id": "input", "type": "input", "config": {} } ],
            "edges": []
        });
        let kvs = vec![(
            "gw/supernodes/secured-call".to_string(),
            serde_json::to_vec(&sn).unwrap(),
        )];
        let gw = gateway_from_kvs("gw", kvs).unwrap();
        assert_eq!(gw.supernodes.len(), 1);
        assert_eq!(gw.supernodes[0].name, "secured-call");
    }

    #[test]
    fn test_gateway_from_kvs_bad_supernode_json_is_error() {
        let kvs = vec![("gw/supernodes/x".to_string(), b"not json".to_vec())];
        let err = gateway_from_kvs("gw", kvs).unwrap_err();
        assert!(err.contains("bad supernode"), "{err}");
    }

    #[test]
    fn test_is_empty_counts_supernodes() {
        let mut gw: GatewayConfig = serde_yaml::from_str("{}").unwrap();
        assert!(is_empty(&gw));
        gw.supernodes.push(SupernodeConfig {
            name: "s".into(),
            description: None,
            nodes: vec![],
            edges: vec![],
        });
        assert!(!is_empty(&gw));
    }

    #[test]
    fn test_gateway_from_kvs_parses_plugin_configs() {
        let def = serde_json::json!({ "name": "corp", "type": "cors", "config": {} });
        let kvs = vec![(
            "gw/plugin_configs/corp".to_string(),
            serde_json::to_vec(&def).unwrap(),
        )];
        let gw = gateway_from_kvs("gw", kvs).unwrap();
        assert_eq!(gw.plugin_configs.len(), 1);
        assert_eq!(gw.plugin_configs[0].name, "corp");
    }

    #[test]
    fn test_gateway_from_kvs_bad_plugin_config_json_is_error() {
        let kvs = vec![("gw/plugin_configs/x".to_string(), b"not json".to_vec())];
        let err = gateway_from_kvs("gw", kvs).unwrap_err();
        assert!(err.contains("bad plugin config"), "{err}");
    }

    #[test]
    fn test_is_empty_counts_plugin_configs() {
        let mut gw: GatewayConfig = serde_yaml::from_str("{}").unwrap();
        assert!(is_empty(&gw));
        gw.plugin_configs.push(PluginConfigDef {
            name: "c".into(),
            plugin_type: "cors".into(),
            description: None,
            config: Default::default(),
        });
        assert!(!is_empty(&gw));
    }

    #[test]
    fn test_gateway_from_kvs_parses_stores() {
        let kvs = vec![(
            "/fb/stores/s1".to_string(),
            br#"{"name":"s1","type":"redis","url":"redis://127.0.0.1:6379","key_prefix":"fb","connect_timeout_ms":2000}"#.to_vec(),
        )];
        let gw = gateway_from_kvs("/fb", kvs).unwrap();
        assert_eq!(gw.stores.len(), 1);
        assert_eq!(gw.stores[0].name, "s1");
        assert_eq!(gw.stores[0].store_type, "redis");
        assert!(!is_empty(&gw));

        let err = gateway_from_kvs(
            "/fb",
            vec![("/fb/stores/bad".to_string(), b"{notjson".to_vec())],
        )
        .unwrap_err();
        assert!(err.contains("bad store 'bad'"), "{err}");
    }

    fn store_at(prefix: &str) -> EtcdConfigStore {
        let cfg: EtcdConfig = serde_yaml::from_str(&format!(
            "endpoints: ['http://127.0.0.1:2379']\nprefix: {prefix}\n"
        ))
        .unwrap();
        EtcdConfigStore::new(&cfg)
    }

    fn sample_config() -> GatewayConfig {
        serde_yaml::from_str(
            r#"
routes:
  - { name: r1, match: { path: /a }, policy: p1 }
policies:
  - name: p1
    nodes: [{ id: listener, type: listener }, { id: client, type: client }]
    edges: [{ from: listener.out, to: client.in }]
consumers:
  - { name: c1, credentials: { key-auth: { key: k } } }
supernodes:
  - { name: s1, nodes: [{ id: input, type: input }, { id: output, type: output }], edges: [{ from: input.out, to: output.in }] }
plugin_configs:
  - { name: pc1, type: cors, config: { allow_origins: "*" } }
stores:
  - { name: st1, type: redis, url: "redis://r:6379" }
"#,
        )
        .unwrap()
    }

    /// Only keys the reader recognises are stale candidates: a nested
    /// gateway's prefix and unrelated keys survive, a removed route does not.
    #[test]
    fn stale_keys_only_covers_the_gateways_own_layout() {
        let current: Vec<String> = [
            "/fb/routes/keep",
            "/fb/routes/old",
            "/fb/b/routes/x",
            "/fb/other",
            "/fb/unknown/y",
            "/fbx/routes/z",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let kept: std::collections::HashSet<String> =
            ["/fb/routes/keep".to_string()].into_iter().collect();
        let stale = stale_keys("/fb", &current, &kept);
        assert_eq!(stale, vec!["/fb/routes/old"]);
    }

    #[test]
    fn parse_resource_key_recognises_the_six_families() {
        for f in RESOURCE_FAMILIES {
            assert_eq!(
                parse_resource_key("/fb", &format!("/fb/{f}/n")),
                Some((f, "n"))
            );
        }
        assert_eq!(parse_resource_key("/fb", "/fb/routes"), None);
        assert_eq!(parse_resource_key("/fb", "/fb/b/routes/x"), None);
    }

    /// An empty (or `/`) prefix is refused before any network I/O.
    #[tokio::test]
    async fn reconcile_prefix_rejects_an_empty_prefix() {
        let empty: GatewayConfig = serde_yaml::from_str("{}").unwrap();
        for prefix in ["''", "/", "'/'"] {
            let cfg: EtcdConfig = serde_yaml::from_str(&format!(
                "endpoints: ['http://127.0.0.1:1']
prefix: {prefix}
"
            ))
            .unwrap();
            let err = reconcile_prefix(&cfg, &empty).await.unwrap_err();
            assert!(err.contains("prefix"), "{err}");
            assert!(err.contains("empty"), "{err}");
        }
    }

    /// desired_kvs is the write side of gateway_from_kvs: one JSON document
    /// per resource under the six key families, and reading them back yields
    /// the same config.
    #[test]
    fn desired_kvs_round_trips_through_gateway_from_kvs() {
        let store = store_at("/featherbit");
        let gw = sample_config();
        let kvs = store.desired_kvs(&gw);
        let keys: Vec<&str> = kvs.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(
            keys,
            [
                "/featherbit/routes/r1",
                "/featherbit/policies/p1",
                "/featherbit/consumers/c1",
                "/featherbit/supernodes/s1",
                "/featherbit/plugin_configs/pc1",
                "/featherbit/stores/st1",
            ]
        );
        let back = gateway_from_kvs("/featherbit", kvs).unwrap();
        assert_eq!(
            serde_json::to_value(&back).unwrap(),
            serde_json::to_value(&gw).unwrap()
        );
    }

    /// An empty desired config produces no keys, so reconcile deletes every
    /// current key: the operator uses this to empty a gateway's prefix.
    #[test]
    fn desired_kvs_of_empty_config_is_empty() {
        let store = store_at("/featherbit/");
        let empty: GatewayConfig = serde_yaml::from_str("{}").unwrap();
        assert!(store.desired_kvs(&empty).is_empty());
    }

    /// The prefix's trailing slash is normalized by `new`, so keys never
    /// double up a separator.
    #[test]
    fn desired_kvs_normalizes_prefix() {
        let store = store_at("/fb/");
        let kvs = store.desired_kvs(&sample_config());
        assert_eq!(kvs[0].0, "/fb/routes/r1");
    }
}
