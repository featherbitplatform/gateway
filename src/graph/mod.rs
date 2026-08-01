//! Node-graph policy engine: compiles YAML-declared policies into executable
//! pipelines and validates their structure.
//!
//! A policy is a directed graph of plugin nodes connected by edges written as
//! `from: node_id.port` / `to: node_id.port` (ports: `out`, `success`,
//! `error`, `in`). Policies referencing supernodes are inlined by `expand_policy` before compilation.
//! [`compile_policy`] produces a [`CompiledGraph`] that the
//! data-plane executes per request; [`validate_policy`] checks structure first.

mod engine;
mod expand;
mod validation;

pub use engine::{compile_policy, CompiledGraph};
pub use expand::expand_policy;
pub use validation::validate_policy;
