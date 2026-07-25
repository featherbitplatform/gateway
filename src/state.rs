//! Shared, lock-protected gateway state used by the data plane, the Admin
//! API, and the hot-reload watcher. Owns the current gateway config and the
//! routes compiled from it, and provides the recompile/swap operations.

use std::sync::Arc;
use tokio::sync::RwLock;

use crate::config::{GatewayConfig, RouteConfig, SystemConfig};
use crate::config_store::ConfigStore;
use crate::debug::DebugState;
use crate::graph::{compile_policy, validate_policy, CompiledGraph};
use crate::metrics::GatewayMetrics;
use crate::plugins::resources::PluginResources;

/// Shared gateway state, accessible from both the data-plane server and the Admin API.
///
/// Wrapped in an [`Arc`] and cloned into every server task. The data plane
/// only ever takes short **read** locks on `routes` while matching a request;
/// **write** locks are taken by the Admin API and hot-reload paths when
/// swapping in a freshly compiled route table. Route recompilation happens on
/// [`SharedState::reload`] / [`SharedState::reload_from_disk`] — never on the
/// request path.
pub struct SharedState {
    /// Immutable system-level configuration (`system.yaml`); fixed for the process lifetime.
    #[allow(dead_code)] // callers take `&SystemConfig` directly; kept on state for reference
    pub system: SystemConfig,
    /// Current gateway configuration (`gateway.yaml`), mutated by the Admin API CRUD endpoints.
    pub gateway: RwLock<GatewayConfig>,
    /// Route table: each route paired with the compiled graph of the policy it references.
    /// Kept in declaration order; the first matching route wins.
    pub routes: RwLock<Vec<(RouteConfig, Arc<CompiledGraph>)>>,
    /// Path to `gateway.yaml`, if known; required for [`SharedState::reload_from_disk`].
    pub config_path: Option<std::path::PathBuf>,
    /// Process-wide Prometheus registry. Compiled graphs record per-node
    /// metrics into it, the data plane records per-request metrics, and the
    /// Admin API's `/metrics` endpoint renders it.
    pub metrics: Arc<GatewayMetrics>,
    /// Shared plugin services (metrics handle, shared clients), threaded into
    /// every plugin at policy compile time.
    pub resources: Arc<PluginResources>,
    /// Backend the config is loaded from and Admin API mutations are persisted
    /// to (file by default; etcd for HA clusters).
    pub config_store: Arc<dyn ConfigStore>,
    /// Debug-mode settings and the bounded trace buffer. Written by the data
    /// plane when a request opts into tracing, read by the Admin API. Fixed at
    /// startup — `system.yaml` is not hot-reloaded.
    pub debug: Arc<DebugState>,
}

impl SharedState {
    /// Creates the shared state, validating and compiling every policy up front.
    ///
    /// Fails if any policy is invalid or a route references an unknown policy,
    /// so a successfully constructed `SharedState` always has a usable route table.
    pub fn new(
        system: SystemConfig,
        gateway: GatewayConfig,
        config_path: Option<std::path::PathBuf>,
        config_store: Arc<dyn ConfigStore>,
    ) -> Result<Self, String> {
        let metrics = Arc::new(GatewayMetrics::new());
        let resources = PluginResources::new(Some(metrics.clone()));
        resources
            .consumers
            .store(Arc::new(crate::consumers::ConsumerStore::from_config(
                &gateway.consumers,
            )?));
        let routes = Self::compile_routes(&gateway, &resources)?;
        let debug_state = Arc::new(DebugState::new(&system.debug));
        if debug_state.enabled {
            let bodies = if debug_state.capture_bodies {
                "captured"
            } else {
                "excluded"
            };
            tracing::warn!(
                "debug mode is ENABLED: policy traces capture request headers and \
                 context state into memory (bodies: {}). Do not enable in production.",
                bodies
            );
            if debug_state.trace_all {
                let header = debug_state.trigger_header.clone();
                tracing::warn!(
                    "debug.trace_all is on: EVERY request is traced, not just those \
                     carrying '{}'. This snapshots the context once per node for all traffic.",
                    header
                );
            }
        }
        Ok(Self {
            system,
            gateway: RwLock::new(gateway),
            routes: RwLock::new(routes),
            config_path,
            metrics,
            resources,
            config_store,
            debug: debug_state,
        })
    }

    /// Validates and compiles `new_gw`, then atomically swaps the consumer
    /// store, route table, and in-memory gateway config.
    ///
    /// This is the single swap path used by every config driver (file watcher,
    /// etcd watch, Admin API commits). All fallible work — consumer-store build
    /// and policy compilation — happens **before** any swap, so a failure
    /// leaves the running config untouched (the last-good guarantee).
    pub async fn apply_gateway(&self, new_gw: GatewayConfig) -> Result<(), String> {
        let consumers = crate::consumers::ConsumerStore::from_config(&new_gw.consumers)?;
        let routes = Self::compile_routes(&new_gw, &self.resources)?;
        tracing::info!(
            "Applied config: {} routes from {} policies",
            routes.len(),
            new_gw.policies.len()
        );
        self.resources.consumers.store(Arc::new(consumers));
        let mut gw = self.gateway.write().await;
        *gw = new_gw;
        let mut r = self.routes.write().await;
        *r = routes;
        Ok(())
    }

    /// Validates and compiles `gw` **without** swapping anything.
    ///
    /// Config stores call this to reject a candidate config before persisting
    /// it, so the Admin API can return an error synchronously even when the
    /// change will be applied asynchronously by a watch.
    pub fn validate_gateway(&self, gw: &GatewayConfig) -> Result<(), String> {
        crate::consumers::ConsumerStore::from_config(&gw.consumers)?;
        Self::compile_routes(gw, &self.resources)?;
        Ok(())
    }

    /// Reloads from disk (re-reads `gateway.yaml` with env interpolation),
    /// recompiles, and swaps in the new config.
    ///
    /// Invoked by the hot-reload file watcher. Fails without side effects if
    /// `config_path` is unset, the file cannot be parsed, or compilation fails.
    pub async fn reload_from_disk(&self) -> Result<(), String> {
        let path = self
            .config_path
            .as_ref()
            .ok_or("No config path set for hot-reload")?;
        let new_gw: GatewayConfig =
            crate::config::load_yaml_with_env(path).map_err(|e| e.to_string())?;
        self.apply_gateway(new_gw).await
    }

    /// Validates and compiles every policy, then binds each route to its
    /// compiled graph. Policies shared by multiple routes are compiled once
    /// and shared via `Arc`.
    fn compile_routes(
        gateway: &GatewayConfig,
        resources: &Arc<PluginResources>,
    ) -> Result<Vec<(RouteConfig, Arc<CompiledGraph>)>, String> {
        let mut policy_map = std::collections::HashMap::new();
        for policy in &gateway.policies {
            if let Err(errors) = validate_policy(policy) {
                return Err(format!("Invalid policy '{}': {:?}", policy.name, errors));
            }
            let compiled = compile_policy(policy, resources.clone())?;
            policy_map.insert(policy.name.clone(), Arc::new(compiled));
        }

        let mut routes = Vec::new();
        for route in &gateway.routes {
            let graph = policy_map
                .get(&route.policy)
                .ok_or(format!(
                    "Route '{}' references unknown policy '{}'",
                    route.name, route.policy
                ))?
                .clone();
            routes.push((route.clone(), graph));
        }
        Ok(routes)
    }
}
