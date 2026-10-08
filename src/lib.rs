//! # featherbit
//!
//! A high-performance API gateway delivered as a single Rust binary.
//!
//! Featherbit routes traffic through **node-graph policies** declared in YAML:
//! each policy is a pipeline of nodes wired together by success/error ports.
//! Plugins come in two tiers — native Rust plugins (proxying, auth,
//! rate-limiting, CORS, logging, ...) plus scripted plugins written in Lua.
//! A [`context::Context`] object (`request`, `response`, `message`, `errors`)
//! flows through every node in the pipeline. Operations are handled by an
//! admin REST API with an embedded React UI, configuration hot-reload via a
//! file watcher, and Prometheus metrics per route and per node.
//!
//! # Architecture
//!
//! Request flow: HTTP request → `server::listener` matches a route → builds a
//! `Context` → `CompiledGraph::execute()` walks the policy's nodes following
//! success/error ports → the final `Context.response` is sent to the client.
//!
//! Configuration lives in two files: `system.yaml` (listeners, timeouts,
//! admin API, logging) and `gateway.yaml` (routes and policies). Both support
//! `${ENV_VAR:-default}` interpolation and the latter is hot-reloaded on change.
//!
//! # Library use
//!
//! The binary in `src/main.rs` is a thin CLI over this crate. External
//! consumers (the Kubernetes operator in `featherbitplatform/gateway-operator`)
//! link the crate for its config types and validators:
//! [`config`], [`graph`], [`routing`], [`stores`], [`consumers`],
//! [`state::validate_gateway_config`], [`plugins::port_spec`] and
//! [`config_store::etcd::reconcile_prefix`]. `tests/lib_surface.rs` pins that
//! surface.
//!
//! Depend on it with `default-features = false`: the `ui` feature embeds the
//! gitignored `ui/dist/`, and `mcp` mounts a transport a library consumer does
//! not need. Enable `redis-store` when the redis store backends must be
//! compiled in (the `stores:` section's static checks,
//! [`stores::validate_stores`], do not depend on it).

// `PluginExecutionError` deliberately carries the whole `Context` by value so the
// graph engine can route a failing node's context out through its `error` port
// (see `plugins::PluginExecutionError`). That makes the `Err` variant large by
// design; boxing it would ripple through the `Plugin` trait and every plugin.
#![allow(clippy::result_large_err)]

pub mod acme;
pub mod admin;
pub mod balancer;
pub mod batch;
pub mod config;
pub mod config_store;
pub mod consumers;
pub mod context;
pub mod debug;
pub mod graph;
pub mod hot_reload;
pub mod mcp;
pub mod metrics;
pub mod net;
pub mod outbound;
pub mod plugins;
pub mod ratelimit;
pub mod routing;
pub mod server;
pub mod sessions;
pub mod state;
pub mod stores;
pub mod stream;
#[cfg(test)]
pub(crate) mod test_log;
pub mod traffic;
pub mod vars;
