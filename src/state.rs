//! Shared, lock-protected gateway state used by the data plane, the Admin
//! API, and the hot-reload watcher. Owns the current gateway config and the
//! routes compiled from it, and provides the recompile/swap operations.

use std::sync::Arc;
use tokio::sync::RwLock;

use crate::config::{resolve_plugin_configs, GatewayConfig, RouteConfig, SystemConfig};
use crate::config_store::ConfigStore;
use crate::debug::DebugState;
use crate::graph::{
    compile_policy, expand_policy, validate_policy, validate_supernode, CompiledGraph,
};
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
    ///
    /// Note: on success the candidate store registry remains loaded in
    /// resources.stores; it is only ever read during a compile and is
    /// replaced by the next one, so this is not applied config.
    pub fn validate_gateway(&self, gw: &GatewayConfig) -> Result<(), String> {
        crate::consumers::ConsumerStore::from_config(&gw.consumers)?;
        Self::compile_routes(gw, &self.resources)?;
        Ok(())
    }

    /// Reloads from disk (re-reads `gateway.yaml` raw, keeping `${VAR}`
    /// placeholders — resolution happens at compile/build time), recompiles,
    /// and swaps in the new config.
    ///
    /// Invoked by the hot-reload file watcher. Fails without side effects if
    /// `config_path` is unset, the file cannot be parsed, or compilation fails.
    pub async fn reload_from_disk(&self) -> Result<(), String> {
        let path = self
            .config_path
            .as_ref()
            .ok_or("No config path set for hot-reload")?;
        let new_gw: GatewayConfig = crate::config::load_yaml(path).map_err(|e| e.to_string())?;
        self.apply_gateway(new_gw).await
    }

    /// Validates and compiles every policy, then binds each route to its
    /// compiled graph. Policies shared by multiple routes are compiled once
    /// and shared via `Arc`.
    fn compile_routes(
        gateway: &GatewayConfig,
        resources: &Arc<PluginResources>,
    ) -> Result<Vec<(RouteConfig, Arc<CompiledGraph>)>, String> {
        crate::stores::validate_stores(&gateway.stores)?;
        // Swap the candidate store registry in for the duration of the
        // compile (plugins resolve `store:` names at construction). The
        // registry is never read on the request path, so this transient swap
        // cannot affect in-flight traffic; on failure the previous registry
        // is restored, preserving the last-good invariant.
        let prev = resources.stores.load_full();
        let candidate = crate::stores::StoreRegistry::rebuild(&prev, &gateway.stores)?;
        resources.stores.store(Arc::new(candidate));
        let result = Self::compile_routes_inner(gateway, resources);
        if result.is_err() {
            resources.stores.store(prev);
        }
        result
    }

    /// The pre-stores compile body; called with the candidate registry already swapped in.
    fn compile_routes_inner(
        gateway: &GatewayConfig,
        resources: &Arc<PluginResources>,
    ) -> Result<Vec<(RouteConfig, Arc<CompiledGraph>)>, String> {
        // Materialize shared plugin configs first: supernode definitions and
        // policies both resolve against them, and expansion below copies the
        // resolved inner configs into instances. In-memory only — the stored
        // gateway config keeps the `config_ref` form.
        let gateway = resolve_plugin_configs(gateway)?;
        let gateway = &gateway;

        // Surface likely template typos (e.g. `{{request.headres.x}}`) at
        // load time rather than letting them render as silent literals on
        // every request. Runs on the *resolved* copy so a config_ref'd node's
        // merged-in shared config is covered via the policy/supernode walk;
        // plugin_configs are walked directly too, so an unreferenced shared
        // config is still flagged. Advisory only — never fails compilation.
        for warning in crate::config::collect_template_warnings(gateway) {
            tracing::warn!("{warning}");
        }

        // Supernode definitions are validated first: policies expand against
        // them, so a broken definition must fail before any policy does.
        let mut seen = std::collections::HashSet::new();
        for sn in &gateway.supernodes {
            if !seen.insert(sn.name.as_str()) {
                return Err(format!("Duplicate supernode name '{}'", sn.name));
            }
            if let Err(errors) = validate_supernode(sn) {
                return Err(format!("Invalid supernode '{}': {:?}", sn.name, errors));
            }
        }

        let mut policy_map = std::collections::HashMap::new();
        for policy in &gateway.policies {
            if let Err(errors) = validate_policy(policy) {
                return Err(format!("Invalid policy '{}': {:?}", policy.name, errors));
            }
            // Inline supernode instances; the engine never sees them.
            let expanded = expand_policy(policy, &gateway.supernodes)?;
            let compiled = compile_policy(&expanded, resources.clone())?;
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
            // Resolve `${VAR}` in the route-table copy only; the stored
            // config keeps the placeholder form (the Admin API serves it).
            let mut route = route.clone();
            route.match_rule.interpolate_env();
            routes.push((route, graph));
        }
        Ok(routes)
    }
}

/// Dry-run validation of a gateway config, without a live [`SharedState`].
///
/// Runs exactly the fallible work [`SharedState::validate_gateway`] does —
/// consumer-store construction, supernode/policy validation, expansion, and
/// policy compilation — against a throwaway resource set, so it can be called
/// before any state exists. Used by the etcd seeder to reject a broken local
/// `gateway.yaml` *before* writing it into an empty cluster prefix.
pub fn validate_gateway_config(gw: &GatewayConfig) -> Result<(), String> {
    crate::consumers::ConsumerStore::from_config(&gw.consumers)?;
    SharedState::compile_routes(gw, &PluginResources::new(None))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state_from_yaml(gateway_yaml: &str) -> Result<(), String> {
        let system: crate::config::SystemConfig = serde_yaml::from_str("{}").unwrap();
        let gw: crate::config::GatewayConfig = serde_yaml::from_str(gateway_yaml).unwrap();
        let state = SharedState::new(
            system,
            serde_yaml::from_str("{}").unwrap(),
            None,
            std::sync::Arc::new(crate::config_store::FileConfigStore::new(
                std::path::PathBuf::from("gateway.yaml"),
            )),
        )
        .unwrap();
        state.validate_gateway(&gw)
    }

    const SUPERNODE_GATEWAY: &str = r#"
supernodes:
  - name: secured-call
    nodes:
      - { id: input,  type: input }
      - { id: output, type: output }
      - { id: error,  type: error }
      - { id: up, type: upstream, config: { targets: [{ host: "127.0.0.1", port: 9 }] } }
    edges:
      - { from: input.out,  to: up.in }
      - { from: up.success, to: output.in }
routes:
  - name: r
    match: { path: "/*" }
    policy: p
policies:
  - name: p
    nodes:
      - { id: listener, type: listener }
      - { id: sec, type: supernode, config: { name: secured-call } }
      - { id: client, type: client }
    edges:
      - { from: listener.out, to: sec.in }
      - { from: sec.success, to: client.in }
"#;

    #[test]
    fn test_policy_with_supernode_compiles() {
        assert_eq!(state_from_yaml(SUPERNODE_GATEWAY), Ok(()));
    }

    #[test]
    fn test_route_match_resolves_env_placeholders_in_route_table() {
        // gateway.yaml is loaded raw (placeholders intact, so the Admin API
        // never serves resolved values); the compiled route table is where
        // `${VAR}` in match rules must resolve.
        std::env::set_var("TEST_ROUTE_PREFIX", "/env-api");
        let gw: crate::config::GatewayConfig = serde_yaml::from_str(
            r#"
routes:
  - name: r
    match:
      path: "${TEST_ROUTE_PREFIX}/*"
      host: "${TEST_ROUTE_HOST:-api.example.com}"
      headers: { x-tier: "${TEST_ROUTE_TIER:-gold}" }
    policy: p
policies:
  - name: p
    nodes:
      - { id: listener, type: listener }
      - { id: client, type: client }
    edges:
      - { from: listener.out, to: client.in }
"#,
        )
        .unwrap();

        let routes = SharedState::compile_routes(&gw, &PluginResources::new(None)).unwrap();
        let rule = &routes[0].0.match_rule;
        assert_eq!(rule.path.as_deref(), Some("/env-api/*"));
        assert_eq!(rule.host.as_deref(), Some("api.example.com"));
        assert_eq!(rule.headers["x-tier"], "gold");
        // The stored config keeps the placeholder form.
        assert_eq!(
            gw.routes[0].match_rule.path.as_deref(),
            Some("${TEST_ROUTE_PREFIX}/*")
        );
        std::env::remove_var("TEST_ROUTE_PREFIX");
    }

    /// I5: the etcd seeder's pre-write gate. It must reach the same verdict as
    /// `validate_gateway` without needing a live `SharedState` — accepting a
    /// good config and rejecting one that cannot compile (here: `key-auth`'s
    /// mandatory `denied` port left unwired).
    #[test]
    fn test_validate_gateway_config_is_a_standalone_dry_run() {
        let good: crate::config::GatewayConfig = serde_yaml::from_str(SUPERNODE_GATEWAY).unwrap();
        assert_eq!(validate_gateway_config(&good), Ok(()));

        let broken: crate::config::GatewayConfig = serde_yaml::from_str(
            r#"
routes:
  - name: r
    match: { path: "/*" }
    policy: p
policies:
  - name: p
    nodes:
      - { id: listener, type: listener }
      - { id: auth, type: key-auth, config: { use_consumers: true } }
      - { id: client, type: client }
    edges:
      - { from: listener.out, to: auth.in }
      - { from: auth.success, to: client.in }
"#,
        )
        .unwrap();
        let err = validate_gateway_config(&broken).unwrap_err();
        assert!(
            err.contains("denied") && err.contains("must be wired"),
            "{err}"
        );
    }

    #[test]
    fn test_unknown_supernode_reference_rejected() {
        let yaml = SUPERNODE_GATEWAY.replace("name: secured-call } }", "name: nope } }");
        let err = state_from_yaml(&yaml).unwrap_err();
        assert!(err.contains("unknown supernode"), "{err}");
    }

    #[test]
    fn test_invalid_supernode_definition_rejected() {
        // Missing the input boundary node -> validate_supernode must fail.
        let yaml = r#"
supernodes:
  - name: secured-call
    nodes:
      - { id: output, type: output }
      - { id: error,  type: error }
      - { id: up, type: upstream, config: { targets: [{ host: "127.0.0.1", port: 9 }] } }
    edges:
      - { from: up.success, to: output.in }
routes:
  - name: r
    match: { path: "/*" }
    policy: p
policies:
  - name: p
    nodes:
      - { id: listener, type: listener }
      - { id: sec, type: supernode, config: { name: secured-call } }
      - { id: client, type: client }
    edges:
      - { from: listener.out, to: sec.in }
      - { from: sec.success, to: client.in }
"#;
        let err = state_from_yaml(yaml).unwrap_err();
        assert!(err.contains("Invalid supernode"), "{err}");
    }

    #[test]
    fn test_duplicate_supernode_names_rejected() {
        let yaml = r#"
supernodes:
  - name: secured-call
    nodes:
      - { id: input,  type: input }
      - { id: output, type: output }
      - { id: error,  type: error }
      - { id: up, type: upstream, config: { targets: [{ host: "127.0.0.1", port: 9 }] } }
    edges:
      - { from: input.out,  to: up.in }
      - { from: up.success, to: output.in }
  - name: secured-call
    nodes: []
    edges: []
routes:
  - name: r
    match: { path: "/*" }
    policy: p
policies:
  - name: p
    nodes:
      - { id: listener, type: listener }
      - { id: sec, type: supernode, config: { name: secured-call } }
      - { id: client, type: client }
    edges:
      - { from: listener.out, to: sec.in }
      - { from: sec.success, to: client.in }
"#;
        let err = state_from_yaml(yaml).unwrap_err();
        assert!(err.contains("Duplicate supernode"), "{err}");
    }

    // `upstream` is used deliberately: its `targets` key is REQUIRED, so an
    // UNRESOLVED ref leaves the node without targets and create_plugin fails —
    // giving this test a genuine red state before resolution was wired in.
    // (A permissive plugin like `mocking` would compile even unresolved.)
    const PLUGIN_CONFIG_GATEWAY: &str = r#"
plugin_configs:
  - name: shared-up
    type: upstream
    config: { targets: [ { host: "127.0.0.1", port: 9 } ] }
supernodes:
  - name: wrapped
    nodes:
      - { id: input,  type: input }
      - { id: output, type: output }
      - { id: error,  type: error }
      - { id: up, type: upstream, config_ref: shared-up }
    edges:
      - { from: input.out,  to: up.in }
      - { from: up.success, to: output.in }
routes:
  - name: r
    match: { path: "/*" }
    policy: p
policies:
  - name: p
    nodes:
      - { id: listener, type: listener }
      - { id: direct, type: upstream, config_ref: shared-up, config: { strategy: "round_robin" } }
      - { id: sn, type: supernode, config: { name: wrapped } }
      - { id: client, type: client }
    edges:
      - { from: listener.out,  to: direct.in }
      - { from: direct.success, to: sn.in }
      - { from: sn.success,    to: client.in }
"#;

    /// Refs resolve for a direct policy node AND for a node inside a
    /// supernode definition, and the whole thing compiles.
    #[test]
    fn test_plugin_config_refs_compile() {
        assert_eq!(state_from_yaml(PLUGIN_CONFIG_GATEWAY), Ok(()));
    }

    #[test]
    fn test_unknown_plugin_config_ref_rejected() {
        let yaml = PLUGIN_CONFIG_GATEWAY.replace(
            "config_ref: shared-up, config:",
            "config_ref: nope, config:",
        );
        let err = state_from_yaml(&yaml).unwrap_err();
        assert!(err.contains("unknown plugin config 'nope'"), "{err}");
    }

    /// Removing the shared config while a supernode inner node still
    /// references it is rejected — this is the delete-protection mechanism.
    #[test]
    fn test_delete_referenced_plugin_config_rejected() {
        let yaml = PLUGIN_CONFIG_GATEWAY.replace(
            "  - name: shared-up\n    type: upstream\n    config: { targets: [ { host: \"127.0.0.1\", port: 9 } ] }\n",
            "",
        );
        let err = state_from_yaml(&yaml).unwrap_err();
        assert!(err.contains("unknown plugin config 'shared-up'"), "{err}");
    }

    /// `stores:` validation runs on every compile: duplicates are rejected
    /// with the running config left intact, and the stored config keeps raw
    /// `${...}` placeholders (the security invariant shared with routes).
    #[tokio::test]
    async fn test_stores_validated_at_compile_and_kept_raw() {
        let system: crate::config::SystemConfig = serde_yaml::from_str("{}").unwrap();
        let gw: crate::config::GatewayConfig = serde_yaml::from_str(
            "stores:\n  - name: s1\n    type: redis\n    url: ${STORE_TEST_URL:-redis://127.0.0.1:6379}\n",
        )
        .unwrap();
        let result = SharedState::new(
            system,
            gw,
            None,
            std::sync::Arc::new(crate::config_store::FileConfigStore::new(
                std::path::PathBuf::from("gateway.yaml"),
            )),
        );
        let state = result.unwrap();

        // Stored config still holds the placeholder.
        let gw = state.gateway.read().await;
        assert_eq!(
            gw.stores[0].url,
            "${STORE_TEST_URL:-redis://127.0.0.1:6379}"
        );
        drop(gw);

        // A candidate with a duplicate store name is rejected...
        let bad: crate::config::GatewayConfig = serde_yaml::from_str(
            "stores:\n  - name: d\n    type: redis\n    url: redis://a\n  - name: d\n    type: redis\n    url: redis://b\n",
        )
        .unwrap();
        let err = state.apply_gateway(bad).await.unwrap_err();
        assert!(err.contains("Duplicate store name 'd'"), "{err}");

        // ...and the last-good config keeps serving.
        assert_eq!(state.gateway.read().await.stores.len(), 1);
    }
}
