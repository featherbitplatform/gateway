//! Configuration loading and schema types for the gateway's two YAML files:
//! `system.yaml` (process-level settings: listener, timeouts, admin, logging)
//! and `gateway.yaml` (routes and node-graph policies). All values support
//! `${ENV_VAR:-default}` interpolation, resolved at different times by file:
//! `system.yaml` is text-interpolated at load by [`load_yaml_with_env`];
//! `gateway.yaml` is loaded **raw** by [`load_yaml`] — placeholders stay in
//! the stored config (which the Admin API serves to the Web UI, so resolved
//! secrets never leak there) and are resolved at the point of consumption:
//! plugin node config at graph-compile time ([`interpolate_env_json`]), route
//! match rules when the route table is built, consumer credentials when the
//! consumer store is built.

mod gateway;
mod loader;
mod resolve;
mod system;
mod warnings;

pub use loader::{interpolate_env, interpolate_env_json, load_yaml, load_yaml_with_env};
// featherbit is a binary crate, so `pub` exports nothing externally: re-exports
// consumed only by `#[cfg(test)]` code read as unused in the bin build.
#[allow(unused_imports)]
pub use gateway::{
    EdgeConfig, GatewayConfig, MatchRule, NodeConfig, PluginConfigDef, PolicyConfig, Position,
    RouteConfig, StoreConfig, StoreTlsConfig, SupernodeConfig,
};
#[allow(unused_imports)]
pub use resolve::resolve_plugin_configs;
#[allow(unused_imports)]
pub use system::{
    AdminConfig, ConfigSourceKind, DebugConfig, EtcdConfig, LoggingConfig, SniCert, SniRoute,
    StreamListenerConfig, StreamProtocol, StreamUpstreamConfig, SystemConfig, TimeoutConfig,
    TlsConfig,
};
pub use warnings::collect_template_warnings;
