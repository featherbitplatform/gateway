//! Named shared stores (`stores:` in gateway.yaml): redis/valkey connections
//! referenced by name from plugin config (`store: <name>`).
//!
//! [`StoreRegistry`] holds one client per named store. It is rebuilt by
//! [`crate::state::SharedState::compile_routes`] on every config (re)compile,
//! reusing the previous client when a store's *resolved* config is unchanged
//! (so an unrelated reload does not drop connections). The registry lives in
//! [`crate::plugins::resources::PluginResources`] behind an `ArcSwap` and is
//! only read at plugin-construction time — never on the request path — so a
//! bad `store:` reference fails policy compilation, not a request.
//!
//! Backend code is gated behind the default-on `redis-store` cargo feature;
//! a headless `--no-default-features` build rejects any declared store at
//! config load with a descriptive error.

use std::collections::HashMap;
use std::sync::Arc;

use crate::config::StoreConfig;
use crate::ratelimit::CounterStore;

#[cfg(feature = "redis-store")]
pub mod counter;
#[cfg(feature = "redis-store")]
pub mod redis_store;

/// Validates the `stores:` section: unique non-empty names, known types,
/// and v1 topology restrictions (standalone only). Runs before any client
/// is built, in every compile path (file, etcd, Admin API, dry-run).
pub fn validate_stores(stores: &[StoreConfig]) -> Result<(), String> {
    let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
    for s in stores {
        if s.name.trim().is_empty() {
            return Err("store with empty name".to_string());
        }
        if !seen.insert(s.name.as_str()) {
            return Err(format!("Duplicate store name '{}'", s.name));
        }
        match s.store_type.as_str() {
            "redis" | "valkey" => {}
            other => {
                return Err(format!(
                    "store '{}': unknown type '{}' — supported: redis, valkey",
                    s.name, other
                ))
            }
        }
        if let Some(t) = s.topology.as_deref() {
            if t != "standalone" {
                return Err(format!(
                    "store '{}': topology '{}' is not yet supported (v1 supports standalone only)",
                    s.name, t
                ));
            }
        }
        if s.urls.is_some() {
            return Err(format!(
                "store '{}': 'urls' requires a sentinel/cluster topology, which is not yet supported — use 'url'",
                s.name
            ));
        }
        if s.url.trim().is_empty() {
            return Err(format!("store '{}': url must not be empty", s.name));
        }
    }
    Ok(())
}

/// One client per named store, plus the counter backend built on it.
/// Read-only after construction; replaced wholesale via the `ArcSwap` in
/// `PluginResources` when config changes.
#[derive(Default)]
pub struct StoreRegistry {
    #[cfg(feature = "redis-store")]
    clients: HashMap<String, Arc<redis_store::RedisStoreClient>>,
    #[cfg(feature = "redis-store")]
    counters: HashMap<String, Arc<dyn CounterStore>>,
    #[cfg(feature = "redis-store")]
    #[allow(dead_code)] // consumed by task 5 (session backend helper)
    sessions: HashMap<String, Arc<dyn crate::sessions::SessionStore>>,
    // Keeps the struct non-empty (and the imports used) in headless builds.
    #[cfg(not(feature = "redis-store"))]
    #[allow(clippy::type_complexity)]
    _headless: std::marker::PhantomData<(HashMap<(), ()>, fn() -> Arc<dyn CounterStore>)>,
}

impl StoreRegistry {
    /// Builds a registry for `stores`, reusing `prev`'s client wherever a
    /// store's resolved config fingerprint is unchanged, so unrelated config
    /// reloads keep established connections.
    #[cfg(feature = "redis-store")]
    pub fn rebuild(
        prev: &StoreRegistry,
        stores: &[StoreConfig],
        metrics: Option<Arc<crate::metrics::GatewayMetrics>>,
    ) -> Result<StoreRegistry, String> {
        let mut clients = HashMap::new();
        let mut counters: HashMap<String, Arc<dyn CounterStore>> = HashMap::new();
        let mut sessions: HashMap<String, Arc<dyn crate::sessions::SessionStore>> = HashMap::new();
        for cfg in stores {
            let fingerprint = redis_store::RedisStoreClient::fingerprint_of(cfg);
            let client = match prev.clients.get(&cfg.name) {
                Some(existing) if existing.fingerprint() == fingerprint => existing.clone(),
                _ => Arc::new(redis_store::RedisStoreClient::build(cfg)?),
            };
            counters.insert(
                cfg.name.clone(),
                Arc::new(counter::RedisCounterStore::new(
                    client.clone(),
                    cfg.name.clone(),
                    metrics.clone(),
                )) as Arc<dyn CounterStore>,
            );
            sessions.insert(
                cfg.name.clone(),
                Arc::new(crate::sessions::redis::RedisSessionStore::new(
                    client.clone(),
                    metrics.clone(),
                )) as Arc<dyn crate::sessions::SessionStore>,
            );
            clients.insert(cfg.name.clone(), client);
        }
        Ok(StoreRegistry {
            clients,
            counters,
            sessions,
        })
    }

    /// Headless build: any declared store is a configuration error.
    #[cfg(not(feature = "redis-store"))]
    pub fn rebuild(
        _prev: &StoreRegistry,
        stores: &[StoreConfig],
        _metrics: Option<Arc<crate::metrics::GatewayMetrics>>,
    ) -> Result<StoreRegistry, String> {
        if stores.is_empty() {
            Ok(StoreRegistry::default())
        } else {
            Err(format!(
                "gateway config declares {} store(s) but this binary was built without the redis-store feature",
                stores.len()
            ))
        }
    }

    /// The raw client for `name` (ACME storage borrows it; sessions/counters have
    /// their own typed accessors).
    #[cfg(feature = "redis-store")]
    #[allow(dead_code)] // consumed by the ACME manager wiring (later task)
    pub fn client(&self, name: &str) -> Result<Arc<redis_store::RedisStoreClient>, String> {
        self.clients.get(name).cloned().ok_or_else(|| {
            let mut names: Vec<&str> = self.clients.keys().map(String::as_str).collect();
            names.sort_unstable();
            format!(
                "unknown store '{}' — declared stores: {}",
                name,
                if names.is_empty() {
                    "(none)".to_string()
                } else {
                    names.join(", ")
                }
            )
        })
    }

    /// Resolves the counter backend for a named store; the error carries the
    /// declared-store list so a typo is self-explanatory.
    #[cfg(feature = "redis-store")]
    pub fn counter_store(&self, name: &str) -> Result<Arc<dyn CounterStore>, String> {
        self.counters.get(name).cloned().ok_or_else(|| {
            let mut names: Vec<&str> = self.clients.keys().map(String::as_str).collect();
            names.sort_unstable();
            format!(
                "unknown store '{}' — declared stores: {}",
                name,
                if names.is_empty() {
                    "(none)".to_string()
                } else {
                    names.join(", ")
                }
            )
        })
    }

    #[cfg(not(feature = "redis-store"))]
    pub fn counter_store(&self, name: &str) -> Result<Arc<dyn CounterStore>, String> {
        Err(format!(
            "store '{}': this binary was built without the redis-store feature",
            name
        ))
    }

    /// Resolves the session backend for a named store; the error carries the
    /// declared-store list so a typo is self-explanatory.
    #[cfg(feature = "redis-store")]
    #[allow(dead_code)] // consumed by task 5 (session backend helper)
    pub fn session_store(
        &self,
        name: &str,
    ) -> Result<Arc<dyn crate::sessions::SessionStore>, String> {
        self.sessions.get(name).cloned().ok_or_else(|| {
            let mut names: Vec<&str> = self.clients.keys().map(String::as_str).collect();
            names.sort_unstable();
            format!(
                "unknown store '{}' — declared stores: {}",
                name,
                if names.is_empty() {
                    "(none)".to_string()
                } else {
                    names.join(", ")
                }
            )
        })
    }

    #[allow(dead_code)] // consumed by task 5 (session backend helper)
    #[cfg(not(feature = "redis-store"))]
    pub fn session_store(
        &self,
        name: &str,
    ) -> Result<Arc<dyn crate::sessions::SessionStore>, String> {
        Err(format!(
            "store '{}': this binary was built without the redis-store feature",
            name
        ))
    }

    /// Test-only registry holding one injected fake session store.
    #[cfg(test)]
    pub fn with_fake_session_store(
        name: &str,
        store: Arc<dyn crate::sessions::SessionStore>,
    ) -> StoreRegistry {
        #[cfg(feature = "redis-store")]
        {
            let mut reg = StoreRegistry::default();
            reg.sessions.insert(name.to_string(), store);
            reg
        }
        #[cfg(not(feature = "redis-store"))]
        {
            let _ = (name, store);
            StoreRegistry::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store(name: &str, ty: &str) -> StoreConfig {
        serde_yaml::from_str(&format!(
            "name: {name}\ntype: {ty}\nurl: redis://127.0.0.1:6379\n"
        ))
        .unwrap()
    }

    #[test]
    fn test_validate_stores_rules() {
        assert!(validate_stores(&[]).is_ok());
        assert!(validate_stores(&[store("a", "redis"), store("b", "valkey")]).is_ok());

        let err = validate_stores(&[store("a", "redis"), store("a", "redis")]).unwrap_err();
        assert!(err.contains("Duplicate store name 'a'"), "{err}");

        let err = validate_stores(&[store("a", "memcached")]).unwrap_err();
        assert!(err.contains("unknown type 'memcached'"), "{err}");

        let mut s = store("a", "redis");
        s.topology = Some("cluster".to_string());
        let err = validate_stores(&[s]).unwrap_err();
        assert!(err.contains("not yet supported"), "{err}");

        let mut s = store("a", "redis");
        s.urls = Some(vec!["redis://x".to_string()]);
        let err = validate_stores(&[s]).unwrap_err();
        assert!(err.contains("'urls' requires"), "{err}");

        let mut s = store("a", "redis");
        s.url = String::new();
        let err = validate_stores(&[s]).unwrap_err();
        assert!(err.contains("url must not be empty"), "{err}");
    }

    #[test]
    fn test_counter_store_unknown_name_lists_declared() {
        let reg = StoreRegistry::default();
        // `Arc<dyn CounterStore>` isn't `Debug`, so `unwrap_err()` doesn't
        // compile here — match instead.
        let err = match reg.counter_store("nope") {
            Ok(_) => panic!("expected an error for an unknown store name"),
            Err(e) => e,
        };
        // Exact wording differs by build flavor; both name the store.
        assert!(err.contains("'nope'"), "{err}");
    }

    #[tokio::test]
    async fn test_session_store_lookup_and_fake_injection() {
        let reg = StoreRegistry::default();
        let err = match reg.session_store("nope") {
            Ok(_) => panic!("expected an error for an unknown store name"),
            Err(e) => e,
        };
        assert!(err.contains("'nope'"), "{err}");

        let fake: std::sync::Arc<dyn crate::sessions::SessionStore> =
            std::sync::Arc::new(crate::sessions::FakeSessionStore::default());
        let reg = StoreRegistry::with_fake_session_store("s1", fake);
        assert!(reg.session_store("s1").is_ok());
    }
}
