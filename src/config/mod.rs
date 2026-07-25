//! Configuration loading and schema types for the gateway's two YAML files:
//! `system.yaml` (process-level settings: listener, timeouts, admin, logging)
//! and `gateway.yaml` (routes and node-graph policies). All values support
//! `${ENV_VAR:-default}` interpolation: raw YAML file text is interpolated by
//! [`load_yaml_with_env`], and structured plugin config authored through the
//! Admin API / Web UI (or delivered over etcd) is interpolated at graph-compile
//! time by [`interpolate_env_json`].

mod gateway;
mod loader;
mod system;

pub use loader::{interpolate_env_json, load_yaml_with_env};
// featherbit is a binary crate, so `pub` exports nothing externally: re-exports
// consumed only by `#[cfg(test)]` code read as unused in the bin build.
#[allow(unused_imports)]
pub use gateway::{EdgeConfig, GatewayConfig, MatchRule, NodeConfig, PolicyConfig, RouteConfig};
#[allow(unused_imports)]
pub use system::{
    AdminConfig, ConfigSourceKind, DebugConfig, EtcdConfig, LoggingConfig, SniCert, SniRoute,
    StreamListenerConfig, StreamProtocol, StreamUpstreamConfig, SystemConfig, TimeoutConfig,
    TlsConfig,
};
