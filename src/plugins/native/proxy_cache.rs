//! Response caching (`proxy-cache`), ported from APISIX's
//! `apisix/plugins/proxy-cache`.
//!
//! Serving a cached response means *looking up* the cache before the upstream
//! call and *storing* the fresh response after it — two moments a single
//! featherbit node cannot span. The behavior is expressed as a **pair of
//! nodes** sharing one cache, linked by a required `id`:
//!
//! - a **lookup** node placed *before* `upstream`, which serves a cache hit
//!   straight to the client (short-circuiting the upstream call), and
//! - a **store** node placed *after* `upstream`, which caches a fresh response
//!   for later hits.
//!
//! Both nodes derive the cache key identically from the same `cache_key`
//! template and the request, and share one namespace via `id`, so they always
//! agree. State lives in [`crate::traffic::CacheRegistry`].
//!
//! # Wiring
//!
//! ```text
//!            ┌──────────────────┐        ┌──────────┐        ┌──────────────────┐
//!  listener →│ proxy-cache      │success →│ upstream │success →│ proxy-cache      │→ client
//!            │  (phase=lookup)  │        │          │        │  (phase=store)   │
//!            └──────────────────┘        └──────────┘        └──────────────────┘
//!                    │ error                                    (caches responses
//!                    ▼                                        whose status is cacheable)
//!               client.in
//!         (cached response, HIT)
//! ```
//!
//! On a hit, the lookup node writes the cached response onto the context, adds
//! `featherbit-cache-status: HIT`, and fails with `PROXY_CACHE_HIT` — its `error`
//! port goes to `client.in`, delivering the cached response without touching
//! the upstream. On a miss it passes through; the store node then caches the
//! upstream response and marks it `featherbit-cache-status: MISS`.

use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use crate::context::{Context, GatewayError};
use crate::plugins::resources::PluginResources;
use crate::plugins::{Plugin, PluginExecutionError, PluginOutput, PluginResult};

/// Header written by both nodes to report the cache outcome.
const CACHE_STATUS_HEADER: &str = "featherbit-cache-status";
/// Response headers hidden from clients when `hide_cache_headers` is set.
const HIDDEN_HEADERS: &[&str] = &["cache-control", "expires"];

/// Which half of the pair this node is.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Role {
    /// Runs before `upstream`: serves a cache hit.
    Lookup,
    /// Runs after `upstream`: stores a fresh response.
    Store,
}

/// One node of a `proxy-cache` lookup/store pair.
///
/// Holds a handle to the process-wide [`crate::traffic::CacheRegistry`]; the
/// key is derived per request from `cache_key`, namespaced by `id`.
pub struct ProxyCachePlugin {
    role: Role,
    /// Shared cache namespace — links the lookup and store nodes.
    id: String,
    /// Cache-key components, each interpolated and joined per request.
    cache_key: Vec<String>,
    /// Freshness lifetime for stored entries.
    cache_ttl: Duration,
    /// Response statuses eligible for caching.
    cache_statuses: Vec<u16>,
    /// HTTP methods eligible for caching (uppercase).
    cache_methods: Vec<String>,
    /// When set, hides upstream cache headers from served cache hits.
    hide_cache_headers: bool,
    resources: Arc<PluginResources>,
}

impl ProxyCachePlugin {
    /// Builds one node of the pair from node config.
    ///
    /// Accepted keys:
    /// - `phase` / `role` (string, **required**): `lookup` (before upstream) or
    ///   `store` (after upstream).
    /// - `id` (string, **required**): shared cache namespace; the lookup and
    ///   store nodes of one pair must use the same `id`.
    /// - `cache_key` (array of string templates **or** a single string,
    ///   default `["$request_method", "$host", "$uri"]`): components
    ///   interpolated (see [`crate::vars::interpolate`]) and joined to form the
    ///   key. Both nodes must configure it identically.
    /// - `cache_ttl` (integer seconds, default `300`): freshness lifetime.
    /// - `cache_http_statuses` (array, default `[200, 301, 404]`): statuses
    ///   eligible for caching. (`cache_http_status`, APISIX's singular spelling,
    ///   is also accepted.)
    /// - `cache_method` (array, default `["GET", "HEAD"]`): cacheable methods.
    /// - `hide_cache_headers` (bool, default `false`): strip `cache-control` /
    ///   `expires` from served cache hits.
    ///
    /// ```yaml
    /// # before upstream
    /// type: proxy-cache
    /// config:
    ///   phase: lookup
    ///   id: catalog
    ///   cache_key: ["$request_method", "$host", "$uri"]
    ///   cache_ttl: 300
    /// ---
    /// # after upstream
    /// type: proxy-cache
    /// config:
    ///   phase: store
    ///   id: catalog
    ///   cache_key: ["$request_method", "$host", "$uri"]
    ///   cache_ttl: 300
    ///   cache_http_statuses: [200, 301, 404]
    /// ```
    pub fn from_config(
        config: &HashMap<String, serde_json::Value>,
        resources: &Arc<PluginResources>,
    ) -> Result<Self, String> {
        let role = match config
            .get("phase")
            .or_else(|| config.get("role"))
            .and_then(|v| v.as_str())
        {
            Some("lookup") => Role::Lookup,
            Some("store") => Role::Store,
            Some(other) => {
                return Err(format!(
                    "proxy-cache: unknown phase/role '{}' (expected 'lookup' or 'store')",
                    other
                ))
            }
            None => {
                return Err(
                    "proxy-cache: 'phase' (or 'role') is required: 'lookup' or 'store'".to_string(),
                )
            }
        };

        let id = config
            .get("id")
            .and_then(|v| v.as_str())
            .filter(|s| !s.trim().is_empty())
            .ok_or("proxy-cache: 'id' is required (links the lookup/store pair)")?
            .to_string();

        let cache_key = match config.get("cache_key") {
            None => vec![
                "$request_method".to_string(),
                "$host".to_string(),
                "$uri".to_string(),
            ],
            Some(serde_json::Value::String(s)) => vec![s.clone()],
            Some(serde_json::Value::Array(items)) => {
                let mut out = Vec::with_capacity(items.len());
                for item in items {
                    let s = item
                        .as_str()
                        .ok_or("proxy-cache: cache_key entries must be strings")?;
                    out.push(s.to_string());
                }
                if out.is_empty() {
                    return Err("proxy-cache: cache_key must not be empty".to_string());
                }
                out
            }
            Some(_) => {
                return Err(
                    "proxy-cache: cache_key must be a string or an array of strings".to_string(),
                )
            }
        };

        let ttl_secs = config
            .get("cache_ttl")
            .and_then(|v| v.as_u64())
            .unwrap_or(300);
        if ttl_secs == 0 {
            return Err("proxy-cache: cache_ttl must be >= 1 second".to_string());
        }

        let cache_statuses = parse_statuses(
            config
                .get("cache_http_statuses")
                .or_else(|| config.get("cache_http_status")),
        )?
        .unwrap_or_else(|| vec![200, 301, 404]);

        let cache_methods = match config.get("cache_method") {
            None => vec!["GET".to_string(), "HEAD".to_string()],
            Some(v) => {
                let arr = v
                    .as_array()
                    .ok_or("proxy-cache: cache_method must be an array of strings")?;
                let mut out = Vec::with_capacity(arr.len());
                for item in arr {
                    let m = item
                        .as_str()
                        .ok_or("proxy-cache: cache_method entries must be strings")?;
                    out.push(m.to_uppercase());
                }
                if out.is_empty() {
                    return Err("proxy-cache: cache_method must not be empty".to_string());
                }
                out
            }
        };

        let hide_cache_headers = config
            .get("hide_cache_headers")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        Ok(Self {
            role,
            id,
            cache_key,
            cache_ttl: Duration::from_secs(ttl_secs),
            cache_statuses,
            cache_methods,
            hide_cache_headers,
            resources: resources.clone(),
        })
    }

    /// Whether this request's method is cacheable.
    fn method_cacheable(&self, ctx: &Context) -> bool {
        let method = ctx.request.method.to_uppercase();
        self.cache_methods.contains(&method)
    }

    /// Derives the cache key: `id` namespace + interpolated `cache_key`
    /// components joined by a control-char separator (outside the character set
    /// of any header/method/path, so components can't collide).
    fn derive_key(&self, ctx: &Context) -> String {
        let mut key = String::with_capacity(64);
        key.push_str(&self.id);
        for component in &self.cache_key {
            key.push('\u{1}');
            key.push_str(&crate::vars::interpolate(ctx, component));
        }
        key
    }
}

/// Reads a `Vec<u16>` of HTTP statuses from a JSON array, if present and valid.
fn parse_statuses(v: Option<&serde_json::Value>) -> Result<Option<Vec<u16>>, String> {
    let Some(v) = v else { return Ok(None) };
    let arr = v
        .as_array()
        .ok_or("proxy-cache: cache_http_statuses must be an array of integers")?;
    let mut out = Vec::with_capacity(arr.len());
    for item in arr {
        let n = item
            .as_u64()
            .ok_or("proxy-cache: cache_http_statuses entries must be integers")?;
        if !(200..=599).contains(&n) {
            return Err(format!(
                "proxy-cache: cache status {} is out of range (200-599)",
                n
            ));
        }
        out.push(n as u16);
    }
    if out.is_empty() {
        return Err("proxy-cache: cache_http_statuses must not be empty".to_string());
    }
    Ok(Some(out))
}

#[async_trait]
impl Plugin for ProxyCachePlugin {
    fn plugin_type(&self) -> &str {
        "proxy-cache"
    }

    async fn execute(
        &self,
        mut ctx: Context,
        _named_inputs: &HashMap<String, serde_json::Value>,
    ) -> PluginResult {
        // Non-cacheable methods bypass the cache entirely in both phases.
        if !self.method_cacheable(&ctx) {
            return Ok(PluginOutput {
                context: ctx,
                named_outputs: HashMap::new(),
            });
        }

        let key = self.derive_key(&ctx);

        match self.role {
            Role::Lookup => {
                if let Some(entry) = self.resources.traffic.cache.get(&key) {
                    // Hit: serve the cached response and short-circuit to the
                    // client via the error port (→ client.in).
                    ctx.response.status_code = entry.status;
                    ctx.response.headers = entry.headers;
                    ctx.response.body = entry.body;
                    if self.hide_cache_headers {
                        for h in HIDDEN_HEADERS {
                            ctx.response.headers.remove(*h);
                        }
                    }
                    ctx.response
                        .headers
                        .insert(CACHE_STATUS_HEADER.to_string(), vec!["HIT".to_string()]);

                    let error = GatewayError {
                        node_id: String::new(),
                        code: "PROXY_CACHE_HIT".to_string(),
                        message: "Served from cache".to_string(),
                        metadata: HashMap::new(),
                    };
                    return Err(PluginExecutionError {
                        context: ctx,
                        error,
                    });
                }
                // Miss: continue to the upstream.
                Ok(PluginOutput {
                    context: ctx,
                    named_outputs: HashMap::new(),
                })
            }
            Role::Store => {
                let status = ctx.response.status_code;
                if self.cache_statuses.contains(&status) {
                    self.resources.traffic.cache.put(
                        key,
                        status,
                        ctx.response.headers.clone(),
                        ctx.response.body.clone(),
                        self.cache_ttl,
                    );
                }
                // This response came from the upstream, not the cache.
                ctx.response
                    .headers
                    .insert(CACHE_STATUS_HEADER.to_string(), vec!["MISS".to_string()]);
                Ok(PluginOutput {
                    context: ctx,
                    named_outputs: HashMap::new(),
                })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::{GatewayRequest, GatewayResponse, Protocol};
    use bytes::Bytes;

    fn ctx(method: &str) -> Context {
        Context {
            request: GatewayRequest {
                method: method.to_string(),
                path: "/products".to_string(),
                host: "shop.example".to_string(),
                scheme: "http".to_string(),
                headers: HashMap::new(),
                query_params: HashMap::new(),
                body: Bytes::new(),
                remote_addr: "10.0.0.1:5000".to_string(),
                protocol: Protocol::Http1,
            },
            response: GatewayResponse {
                status_code: 0,
                headers: HashMap::new(),
                body: Bytes::new(),
            },
            message: HashMap::new(),
            errors: Vec::new(),
        }
    }

    fn cfg(pairs: &[(&str, serde_json::Value)]) -> HashMap<String, serde_json::Value> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect()
    }

    fn lookup(r: &Arc<PluginResources>) -> ProxyCachePlugin {
        ProxyCachePlugin::from_config(
            &cfg(&[
                ("phase", serde_json::json!("lookup")),
                ("id", serde_json::json!("cat")),
            ]),
            r,
        )
        .unwrap()
    }

    fn store(r: &Arc<PluginResources>) -> ProxyCachePlugin {
        ProxyCachePlugin::from_config(
            &cfg(&[
                ("phase", serde_json::json!("store")),
                ("id", serde_json::json!("cat")),
            ]),
            r,
        )
        .unwrap()
    }

    #[test]
    fn test_missing_id_and_bad_role_fail() {
        let r = PluginResources::empty();
        assert!(
            ProxyCachePlugin::from_config(&cfg(&[("phase", serde_json::json!("lookup"))]), &r)
                .is_err()
        );
        assert!(ProxyCachePlugin::from_config(
            &cfg(&[
                ("phase", serde_json::json!("bogus")),
                ("id", serde_json::json!("x"))
            ]),
            &r
        )
        .is_err());
    }

    #[test]
    fn test_key_derivation_is_deterministic_and_shared() {
        let r = PluginResources::empty();
        let l = lookup(&r);
        let s = store(&r);
        // Both nodes derive the same key from the same request + config.
        assert_eq!(l.derive_key(&ctx("GET")), s.derive_key(&ctx("GET")));
        // Method participates in the default key.
        assert_ne!(l.derive_key(&ctx("GET")), l.derive_key(&ctx("HEAD")));
    }

    #[tokio::test]
    async fn test_store_then_lookup_returns_hit() {
        let r = PluginResources::empty();
        let l = lookup(&r);
        let s = store(&r);

        // Cold lookup → miss (passes through).
        let miss = l.execute(ctx("GET"), &HashMap::new()).await;
        assert!(miss.is_ok(), "cold lookup should miss and pass through");

        // Upstream produced a 200 body → store caches it.
        let mut resp = ctx("GET");
        resp.response.status_code = 200;
        resp.response.body = Bytes::from_static(b"cached-body");
        let stored = s.execute(resp, &HashMap::new()).await.unwrap();
        assert_eq!(
            stored.context.response.headers.get(CACHE_STATUS_HEADER),
            Some(&vec!["MISS".to_string()])
        );

        // Warm lookup → hit, short-circuits with the cached body.
        let hit = l
            .execute(ctx("GET"), &HashMap::new())
            .await
            .expect_err("warm lookup should hit and short-circuit");
        assert_eq!(hit.error.code, "PROXY_CACHE_HIT");
        assert_eq!(hit.context.response.status_code, 200);
        assert_eq!(
            hit.context.response.body,
            Bytes::from_static(b"cached-body")
        );
        assert_eq!(
            hit.context.response.headers.get(CACHE_STATUS_HEADER),
            Some(&vec!["HIT".to_string()])
        );
    }

    #[tokio::test]
    async fn test_non_cacheable_method_passes_through() {
        let r = PluginResources::empty();
        let l = lookup(&r);
        let s = store(&r);

        // POST is not in the default cache_method → both phases pass through.
        let mut resp = ctx("POST");
        resp.response.status_code = 200;
        resp.response.body = Bytes::from_static(b"not-cached");
        s.execute(resp, &HashMap::new()).await.unwrap();

        let out = l.execute(ctx("POST"), &HashMap::new()).await;
        assert!(out.is_ok(), "non-cacheable method must never hit the cache");
    }
}
