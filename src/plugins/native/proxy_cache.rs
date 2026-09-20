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
//! agree. State lives behind a [`crate::traffic::ResponseCache`].
//!
//! # Wiring
//!
//! ```text
//!            ┌──────────────────┐        ┌──────────┐        ┌──────────────────┐
//!  listener →│ proxy-cache      │success →│ upstream │success →│ proxy-cache      │→ client
//!            │  (phase=lookup)  │        │          │        │  (phase=store)   │
//!            └──────────────────┘        └──────────┘        └──────────────────┘
//!                    │ hit                                      (caches responses
//!                    ▼                                        whose status is cacheable)
//!               client.in
//!         (cached response, HIT)
//! ```
//!
//! On a hit, the lookup node writes the cached response onto the context, adds
//! `featherbit-cache-status: HIT`, and exits through the dedicated `hit`
//! port — wired to `client.in`, delivering the cached response without
//! touching the upstream. On a miss it passes through `success`; the store
//! node then caches the upstream response and marks it
//! `featherbit-cache-status: MISS`.

use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use crate::context::Context;
use crate::plugins::resources::PluginResources;
use crate::plugins::{Plugin, PluginOutput, PluginResult};
use crate::traffic::{CachedResponse, ResponseCache};
use crate::vars::template::Template;

/// Header written by both nodes to report the cache outcome.
const CACHE_STATUS_HEADER: &str = "featherbit-cache-status";
/// Response headers hidden from clients when `hide_cache_headers` is set.
const HIDDEN_HEADERS: &[&str] = &["cache-control", "expires"];

/// The `policy` values this build actually supports — naming `redis` on a
/// headless build would describe an option that cannot work.
#[cfg(feature = "redis-store")]
const SUPPORTED_POLICIES: &str = "local, redis";
#[cfg(not(feature = "redis-store"))]
const SUPPORTED_POLICIES: &str = "local";

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
/// Holds a handle to the configured [`ResponseCache`] backend; the key is
/// derived per request from `cache_key`, namespaced by `id`.
pub struct ProxyCachePlugin {
    role: Role,
    /// Shared cache namespace — links the lookup and store nodes.
    id: String,
    /// Cache-key components, each rendered (supports `{{namespace.path}}`
    /// references and legacy `$var` interpolation — see
    /// [`Template::render_with_legacy`]) and joined per request.
    cache_key: Vec<Template>,
    /// Freshness lifetime for stored entries.
    cache_ttl: Duration,
    /// Response statuses eligible for caching.
    cache_statuses: Vec<u16>,
    /// HTTP methods eligible for caching (uppercase).
    cache_methods: Vec<String>,
    /// When set, hides upstream cache headers from served cache hits.
    hide_cache_headers: bool,
    /// The backend this node pair shares, chosen by `policy`: the process-local
    /// cache for `local`, or a shared `RedisResponseCache` over a declared
    /// `stores:` entry for `redis`.
    cache: Arc<dyn ResponseCache>,
    /// Label for the `backend` dimension of `cache_events`.
    backend_label: &'static str,
    /// Label for the `store` dimension of `cache_events`: the declared
    /// store's name for `policy: redis`, empty for `policy: local` (which has
    /// no store to name).
    store_label: String,
    /// A response body larger than this is served but never cached — one
    /// large response must not be able to fill a store that sessions,
    /// counters and ACME also live in.
    max_object_bytes: usize,
    /// Process-wide services, held for the metrics registry.
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
    ///   default `["$request_method", "$host", "$uri"]`): components rendered
    ///   (supports `{{namespace.path}}` references plus legacy `$var`
    ///   interpolation — see
    ///   [`crate::vars::template::Template::render_with_legacy`]) and joined
    ///   to form the key. Both nodes must configure it identically.
    /// - `cache_ttl` (integer seconds, default `300`): freshness lifetime.
    /// - `cache_http_statuses` (array, default `[200, 301, 404]`): statuses
    ///   eligible for caching. (`cache_http_status`, APISIX's singular spelling,
    ///   is also accepted.)
    /// - `cache_method` (array, default `["GET", "HEAD"]`): cacheable methods.
    /// - `hide_cache_headers` (bool, default `false`): strip `cache-control` /
    ///   `expires` from served cache hits.
    /// - `max_object_bytes` (integer, default `1048576`): responses larger
    ///   than this are served normally but never cached, in either backend.
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

        // The `\u{1}` separator is the prefix boundary that purging a pair
        // relies on: an `id` containing it would produce keys another pair's
        // prefix also matches. Refuse it -- and every other control character,
        // which have no business in a cache namespace -- at compile time
        // rather than trust config never to contain one.
        if id.chars().any(char::is_control) {
            return Err(format!(
                "proxy-cache: 'id' must not contain control characters (got {:?})",
                id
            ));
        }

        let cache_key: Vec<String> = match config.get("cache_key") {
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
        // Discard warnings here — the compile-time walk (a later task)
        // reports well-formed-but-unknown references; execution must not.
        let cache_key: Vec<Template> = cache_key.iter().map(|s| Template::parse(s).0).collect();

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

        let max_object_bytes = config
            .get("max_object_bytes")
            .and_then(|v| v.as_u64())
            .unwrap_or(1_048_576) as usize;

        let policy = config
            .get("policy")
            .and_then(|v| v.as_str())
            .unwrap_or("local");
        let (cache, backend_label, store_label): (Arc<dyn ResponseCache>, &'static str, String) =
            match policy {
                "local" => (resources.traffic.cache.clone(), "local", String::new()),
                #[cfg(feature = "redis-store")]
                "redis" => {
                    let name = config
                        .get("store")
                        .and_then(|v| v.as_str())
                        .filter(|s| !s.is_empty())
                        .ok_or_else(|| {
                            "proxy-cache: policy 'redis' requires 'store' naming a declared stores: entry"
                                .to_string()
                        })?;
                    let client = resources.stores.load().client(name)?;
                    (
                        Arc::new(crate::stores::redis_cache::RedisResponseCache::new(client)),
                        "redis",
                        name.to_string(),
                    )
                }
                other => {
                    return Err(format!(
                        "proxy-cache: unknown policy '{other}' — supported: {}",
                        SUPPORTED_POLICIES
                    ))
                }
            };

        Ok(Self {
            role,
            id,
            cache_key,
            cache_ttl: Duration::from_secs(ttl_secs),
            cache_statuses,
            cache_methods,
            hide_cache_headers,
            cache,
            backend_label,
            store_label,
            max_object_bytes,
            resources: resources.clone(),
        })
    }

    /// Whether this request's method is cacheable.
    fn method_cacheable(&self, ctx: &Context) -> bool {
        let method = ctx.request.method.to_uppercase();
        self.cache_methods.contains(&method)
    }

    /// Derives the cache key: `id` namespace + `cache_key` components
    /// (each rendered via `{{namespace.path}}` references plus legacy `$var`
    /// interpolation) joined by a control-char separator (outside the
    /// character set of any header/method/path, so components can't
    /// collide).
    fn derive_key(&self, ctx: &Context) -> String {
        let mut key = String::with_capacity(64);
        key.push_str(&self.id);
        for component in &self.cache_key {
            key.push('\u{1}');
            key.push_str(&component.render_with_legacy(ctx));
        }
        key
    }

    /// Counts one cache outcome. A no-op when metrics are disabled (unit tests).
    fn record(&self, event: &str) {
        if let Some(metrics) = &self.resources.metrics {
            metrics
                .cache_events
                .with_label_values(&[self.backend_label, &self.store_label, event])
                .inc();
        }
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

    async fn execute(&self, mut ctx: Context) -> PluginResult {
        // Non-cacheable methods bypass the cache entirely in both phases.
        if !self.method_cacheable(&ctx) {
            return Ok(PluginOutput::success(ctx));
        }

        let key = self.derive_key(&ctx);

        match self.role {
            Role::Lookup => {
                // A backend that cannot answer is treated as a miss: this
                // cache exists to save a trip upstream, not to decide
                // whether the request is allowed. Counted either way, so a
                // fully-degraded cache stays visible.
                let found = match self.cache.get(&key).await {
                    Ok(found) => found,
                    Err(e) => {
                        tracing::warn!(key = %key, "proxy-cache lookup failed: {e}");
                        self.record("error");
                        None
                    }
                };
                self.record(if found.is_some() { "hit" } else { "miss" });
                if let Some(entry) = found {
                    // Hit: serve the cached response and short-circuit to the
                    // client via the `hit` port (→ client.in).
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

                    return Ok(PluginOutput::on_port(ctx, "hit"));
                }
                // Miss: continue to the upstream.
                Ok(PluginOutput::success(ctx))
            }
            Role::Store => {
                let status = ctx.response.status_code;
                // The size check only applies to a response that would
                // otherwise have been cached — a non-cacheable status was
                // never going to be stored regardless of its size, so it
                // must not inflate a counter whose whole purpose is showing
                // what the size limit excluded.
                if self.cache_statuses.contains(&status) {
                    if ctx.response.body.len() > self.max_object_bytes {
                        // Metered so a route that mysteriously never caches is
                        // explicable rather than mysterious.
                        self.record("too_large");
                    } else {
                        let entry = CachedResponse {
                            status,
                            headers: ctx.response.headers.clone(),
                            body: ctx.response.body.clone(),
                        };
                        if let Err(e) = self.cache.put(&key, &entry, self.cache_ttl).await {
                            // Metered as well as logged: a response is served
                            // correctly whether or not it was cached, so a write
                            // that has stopped working leaves no other trace.
                            tracing::warn!(key = %key, "proxy-cache store failed: {e}");
                            self.record("error");
                        }
                    }
                }
                // This response came from the upstream, not the cache.
                ctx.response
                    .headers
                    .insert(CACHE_STATUS_HEADER.to_string(), vec!["MISS".to_string()]);
                Ok(PluginOutput::success(ctx))
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
                stream: None,
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

    /// A neutral cacheable request; the outage tests don't need to vary it.
    fn test_context() -> Context {
        ctx("GET")
    }

    /// A fresh metrics registry, as `src/graph/engine.rs`'s tests build one.
    fn test_metrics() -> Arc<crate::metrics::GatewayMetrics> {
        Arc::new(crate::metrics::GatewayMetrics::new())
    }

    /// A lookup-phase plugin around a given cache backend, metrics disabled.
    /// A write that cannot reach its backend must be counted too.
    ///
    /// The lookup side already meters its failures, but a store-side outage
    /// left no trace at all: the response is served correctly either way, so
    /// nothing downstream notices that the cache has stopped being written.
    /// That is the same invisibility the lookup counter exists to remove.
    #[tokio::test]
    async fn test_a_failing_store_increments_the_error_counter() {
        let metrics = test_metrics();
        let plugin = store_plugin_with_cache_and_metrics(Arc::new(BrokenCache), metrics.clone());

        let mut ctx = test_context();
        ctx.response.status_code = 200;
        plugin.execute(ctx).await.unwrap();

        assert_eq!(
            metrics
                .cache_events
                .with_label_values(&["local", "", "error"])
                .get(),
            1,
            "a failed write must be visible in metrics, not only in the log"
        );
    }

    fn lookup_plugin_with_cache(cache: Arc<dyn ResponseCache>) -> ProxyCachePlugin {
        let mut plugin = lookup(&PluginResources::empty());
        plugin.cache = cache;
        plugin
    }

    /// A lookup-phase plugin around a given cache backend and metrics registry.
    fn lookup_plugin_with_cache_and_metrics(
        cache: Arc<dyn ResponseCache>,
        metrics: Arc<crate::metrics::GatewayMetrics>,
    ) -> ProxyCachePlugin {
        let mut plugin = lookup(&PluginResources::new(Some(metrics)));
        plugin.cache = cache;
        plugin
    }

    /// A store-phase plugin around a given cache backend and metrics registry.
    fn store_plugin_with_cache_and_metrics(
        cache: Arc<dyn ResponseCache>,
        metrics: Arc<crate::metrics::GatewayMetrics>,
    ) -> ProxyCachePlugin {
        let mut plugin = store(&PluginResources::new(Some(metrics)));
        plugin.cache = cache;
        plugin
    }

    /// A store-phase plugin around a given cache backend and `max_object_bytes`.
    fn store_plugin_with_cache_and_limit(
        cache: Arc<dyn ResponseCache>,
        max_object_bytes: usize,
    ) -> ProxyCachePlugin {
        let mut plugin = store(&PluginResources::empty());
        plugin.cache = cache;
        plugin.max_object_bytes = max_object_bytes;
        plugin
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

    /// The separator is the prefix boundary purge relies on. An `id` that
    /// contains it would produce keys another pair's prefix also matches, so
    /// it is refused at policy-compile time rather than trusted not to happen.
    #[test]
    fn test_an_id_containing_the_separator_is_rejected() {
        let r = PluginResources::empty();
        // `ProxyCachePlugin` holds an `Arc<dyn ResponseCache>`, so it has no
        // `Debug` impl and can't go through `unwrap_err()`; match instead,
        // as `test_unknown_policy_names_only_what_this_build_supports` does.
        let err = match ProxyCachePlugin::from_config(
            &cfg(&[
                ("phase", serde_json::json!("lookup")),
                ("id", serde_json::json!("products\u{1}x")),
            ]),
            &r,
        ) {
            Err(e) => e,
            Ok(_) => panic!("an id containing the separator must fail from_config"),
        };
        assert!(err.contains("control character"), "{err}");
    }

    /// An unknown `policy` must name only the policies this build actually
    /// supports — `redis` on a headless build describes an option that
    /// cannot work.
    #[test]
    fn test_unknown_policy_names_only_what_this_build_supports() {
        let r = PluginResources::empty();
        let err = match ProxyCachePlugin::from_config(
            &cfg(&[
                ("phase", serde_json::json!("lookup")),
                ("id", serde_json::json!("x")),
                ("policy", serde_json::json!("bogus")),
            ]),
            &r,
        ) {
            Err(e) => e,
            Ok(_) => panic!("an unknown policy must fail from_config"),
        };
        assert!(err.contains("local"), "{err}");
        #[cfg(feature = "redis-store")]
        assert!(err.contains("redis"), "{err}");
        #[cfg(not(feature = "redis-store"))]
        assert!(!err.contains("redis"), "{err}");
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
        let miss = l.execute(ctx("GET")).await.unwrap();
        assert!(
            miss.port.is_none(),
            "cold lookup should miss and pass through"
        );

        // Upstream produced a 200 body → store caches it.
        let mut resp = ctx("GET");
        resp.response.status_code = 200;
        resp.response.body = Bytes::from_static(b"cached-body");
        let stored = s.execute(resp).await.unwrap();
        assert_eq!(
            stored.context.response.headers.get(CACHE_STATUS_HEADER),
            Some(&vec!["MISS".to_string()])
        );

        // Warm lookup → hit, short-circuits with the cached body on the `hit` port.
        let hit = l
            .execute(ctx("GET"))
            .await
            .expect("warm lookup should hit and short-circuit");
        assert_eq!(hit.port, Some("hit"));
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
        s.execute(resp).await.unwrap();

        let out = l.execute(ctx("POST")).await.unwrap();
        assert!(
            out.port.is_none(),
            "non-cacheable method must never hit the cache"
        );
    }

    /// A backend that cannot answer. Stands in for a redis outage, so the
    /// degradation path is testable without a live store.
    struct BrokenCache;

    #[async_trait::async_trait]
    impl crate::traffic::ResponseCache for BrokenCache {
        async fn get(
            &self,
            _key: &str,
        ) -> Result<Option<crate::traffic::CachedResponse>, crate::traffic::cache::CacheError>
        {
            Err(crate::traffic::cache::CacheError(
                "backend down".to_string(),
            ))
        }
        async fn put(
            &self,
            _key: &str,
            _entry: &crate::traffic::CachedResponse,
            _ttl: std::time::Duration,
        ) -> Result<(), crate::traffic::cache::CacheError> {
            Err(crate::traffic::cache::CacheError(
                "backend down".to_string(),
            ))
        }
        async fn purge(&self, _id: &str) -> Result<u64, crate::traffic::cache::CacheError> {
            Err(crate::traffic::cache::CacheError(
                "backend down".to_string(),
            ))
        }
    }

    /// The load-bearing behaviour: an outage costs latency, not availability.
    /// A lookup against a dead backend must leave through `success` (on to the
    /// upstream), not `error` and not `hit`.
    #[tokio::test]
    async fn test_a_failing_backend_is_a_miss_not_an_error() {
        let plugin = lookup_plugin_with_cache(Arc::new(BrokenCache));
        let out = plugin
            .execute(test_context())
            .await
            .expect("a cache outage must not fail the request");
        assert_eq!(
            out.port, None,
            "a miss continues to the upstream on `success`"
        );
    }

    /// ...but it must not be silent, or a fully-degraded cache is
    /// indistinguishable from a working one.
    #[tokio::test]
    async fn test_a_failing_backend_increments_the_error_counter() {
        let metrics = test_metrics();
        let plugin = lookup_plugin_with_cache_and_metrics(Arc::new(BrokenCache), metrics.clone());
        plugin.execute(test_context()).await.unwrap();

        assert_eq!(
            metrics
                .cache_events
                .with_label_values(&["local", "", "error"])
                .get(),
            1,
            "a backend error must be visible in metrics"
        );
    }

    /// One large response must not be able to fill a store that sessions,
    /// counters and ACME also live in.
    #[tokio::test]
    async fn test_a_response_over_max_object_bytes_is_not_cached() {
        let cache = Arc::new(crate::traffic::LocalResponseCache::default());
        let plugin = store_plugin_with_cache_and_limit(cache.clone(), 16);

        let mut ctx = test_context();
        ctx.response.status_code = 200;
        ctx.response.body = bytes::Bytes::from(vec![b'x'; 64]);
        plugin.execute(ctx).await.unwrap();

        assert_eq!(cache.len(), 0, "an oversized response must not be stored");
    }

    #[tokio::test]
    async fn test_a_response_within_max_object_bytes_is_cached() {
        let cache = Arc::new(crate::traffic::LocalResponseCache::default());
        let plugin = store_plugin_with_cache_and_limit(cache.clone(), 1024);

        let mut ctx = test_context();
        ctx.response.status_code = 200;
        ctx.response.body = bytes::Bytes::from_static(b"small");
        plugin.execute(ctx).await.unwrap();

        assert_eq!(cache.len(), 1);
    }

    /// `max_object_bytes` must come from the node's own config, not only from
    /// a value a test sets directly on the struct. The other two
    /// `max_object_bytes` tests build the plugin with
    /// `store_plugin_with_cache_and_limit`, which sets the field after
    /// construction and so never exercises `from_config`'s
    /// `config.get("max_object_bytes")` read — renaming or dropping that key
    /// would leave every deployment silently back on the 1 MiB default and
    /// this suite would not notice. This test goes through `from_config`
    /// instead, with a limit (16 bytes) far below the default, so only the
    /// configured value — not the constructor default — can explain a miss.
    #[tokio::test]
    async fn test_max_object_bytes_is_read_from_node_config() {
        let r = PluginResources::empty();
        let plugin = ProxyCachePlugin::from_config(
            &cfg(&[
                ("phase", serde_json::json!("store")),
                ("id", serde_json::json!("cfgtest")),
                ("max_object_bytes", serde_json::json!(16)),
            ]),
            &r,
        )
        .unwrap();

        let mut oversized = test_context();
        oversized.response.status_code = 200;
        oversized.response.body = bytes::Bytes::from(vec![b'x'; 64]);
        plugin.execute(oversized).await.unwrap();
        assert_eq!(
            r.traffic.cache.len(),
            0,
            "a response over the configured max_object_bytes must not be stored"
        );

        let mut small = test_context();
        small.response.status_code = 200;
        small.response.body = bytes::Bytes::from_static(b"tiny");
        plugin.execute(small).await.unwrap();
        assert_eq!(
            r.traffic.cache.len(),
            1,
            "a response under the configured max_object_bytes must still be stored"
        );
    }

    /// Spec §7.1/§8: skipping an oversized response must be metered, so a
    /// route that mysteriously never caches is explicable rather than
    /// mysterious. Asserted nowhere before this test — a mutation that
    /// deleted the `record("too_large")` call passed the whole suite.
    #[tokio::test]
    async fn test_an_oversized_response_increments_the_too_large_counter() {
        let metrics = test_metrics();
        let r = PluginResources::new(Some(metrics.clone()));
        let plugin = ProxyCachePlugin::from_config(
            &cfg(&[
                ("phase", serde_json::json!("store")),
                ("id", serde_json::json!("toolarge")),
                ("max_object_bytes", serde_json::json!(16)),
            ]),
            &r,
        )
        .unwrap();

        let mut ctx = test_context();
        ctx.response.status_code = 200; // cacheable status
        ctx.response.body = bytes::Bytes::from(vec![b'x'; 64]);
        plugin.execute(ctx).await.unwrap();

        assert_eq!(
            metrics
                .cache_events
                .with_label_values(&["local", "", "too_large"])
                .get(),
            1,
            "an oversized response that would otherwise have been cached must be metered"
        );
    }

    /// A response whose status was never going to be cached must not inflate
    /// `too_large`, even if it is also oversized — that counter exists to
    /// show what the *size limit* excluded, not every large response that
    /// passes through the store node.
    #[tokio::test]
    async fn test_too_large_is_not_counted_for_a_non_cacheable_status() {
        let metrics = test_metrics();
        let r = PluginResources::new(Some(metrics.clone()));
        let plugin = ProxyCachePlugin::from_config(
            &cfg(&[
                ("phase", serde_json::json!("store")),
                ("id", serde_json::json!("toolarge2")),
                ("max_object_bytes", serde_json::json!(16)),
            ]),
            &r,
        )
        .unwrap();

        let mut ctx = test_context();
        ctx.response.status_code = 500; // not in the default cache_http_statuses
        ctx.response.body = bytes::Bytes::from(vec![b'x'; 64]);
        plugin.execute(ctx).await.unwrap();

        assert_eq!(
            metrics
                .cache_events
                .with_label_values(&["local", "", "too_large"])
                .get(),
            0,
            "a response that was never cacheable must not count against the size limit"
        );
    }
}
