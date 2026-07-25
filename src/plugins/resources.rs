//! Process-wide services shared by every plugin instance.
//!
//! `SharedState` owns one [`PluginResources`] for the process lifetime and
//! threads it through `compile_policy` → `create_plugin` → each plugin's
//! `from_config`, so plugins can hold handles to shared infrastructure
//! (metrics registry, outbound HTTP client, consumer store, rate-limit
//! counter backends) instead of constructing their own per node or — worse —
//! per request.

use std::sync::Arc;

use arc_swap::ArcSwap;

use crate::consumers::ConsumerStore;
use crate::metrics::GatewayMetrics;
use crate::outbound::OutboundClient;
use crate::ratelimit::CounterStoreRegistry;
use crate::traffic::TrafficRegistries;

/// Handle to process-wide services, injected into plugins at construction.
///
/// Grows as shared infrastructure lands (counter backends); every field must
/// be cheap to clone or shared behind its own `Arc`.
pub struct PluginResources {
    /// Shared Prometheus registry; `None` disables recording (unit tests).
    pub metrics: Option<Arc<GatewayMetrics>>,
    /// Pooled outbound HTTP client for upstream proxying and plugin callouts.
    pub outbound: Arc<OutboundClient>,
    /// Consumer store, swapped atomically on config reload so lookups on the
    /// request path are lock-free. Plugins should `load()` per request, not
    /// cache the inner Arc across requests.
    pub consumers: ArcSwap<ConsumerStore>,
    /// Rate-limit counter backends, resolved by `policy` name at config load.
    pub counters: CounterStoreRegistry,
    /// Shared state for traffic-control node pairs (concurrency, breakers, cache).
    pub traffic: TrafficRegistries,
}

impl PluginResources {
    /// Creates the process-wide resources handle with an empty consumer
    /// store; callers swap in the real store via [`PluginResources::consumers`].
    pub fn new(metrics: Option<Arc<GatewayMetrics>>) -> Arc<Self> {
        Arc::new(Self {
            metrics,
            outbound: Arc::new(OutboundClient::new()),
            consumers: ArcSwap::from_pointee(ConsumerStore::default()),
            counters: CounterStoreRegistry::default(),
            traffic: TrafficRegistries::default(),
        })
    }

    /// Resources with optional services disabled — for unit tests.
    #[cfg(test)]
    pub fn empty() -> Arc<Self> {
        Self::new(None)
    }
}
