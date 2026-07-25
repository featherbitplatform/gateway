//! The `prometheus` node — a thin parity node over featherbit's built-in
//! Prometheus metrics.
//!
//! **featherbit already exposes Prometheus metrics out of the box.** The
//! [`GatewayMetrics`](crate::metrics::GatewayMetrics) registry records
//! per-route request counters and latency histograms plus per-node execution
//! metrics; the graph engine and data-plane listener feed it on every request
//! with no plugin required, and it is rendered at the Admin API's `/metrics`
//! endpoint. So this node does **not** stand up metrics from scratch.
//!
//! It exists only to add a dimension APISIX's `prometheus` plugin tracks that
//! featherbit's always-on core metrics do not: a **per-consumer request
//! counter** (`gateway_consumer_requests_total`, labelled `consumer` and
//! `route`). Drop it into a pipeline **after** the auth node that attaches the
//! consumer (`key-auth`, `basic-auth`, ...); each execution bumps the counter
//! for the request's consumer (`anonymous` when none is attached). It never
//! mutates the context and never fails.
//!
//! ## Deviations from APISIX
//! - APISIX's plugin wires up the entire Prometheus exporter and its full
//!   metric set. In featherbit the core metrics are built-in and always on, so
//!   this node is a thin add-on that only records the per-consumer counter.
//! - APISIX's `prefer_name` toggles route *name* vs route *id* in labels.
//!   featherbit has no route object on the context here, so the `route` label
//!   uses the request `Host` (kept low-cardinality on purpose). `prefer_name`
//!   is accepted for config compatibility but is otherwise inert.

use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::Arc;

use crate::context::Context;
use crate::metrics::GatewayMetrics;
use crate::plugins::resources::PluginResources;
use crate::plugins::{Plugin, PluginOutput, PluginResult};

/// Records a per-consumer request counter against the shared
/// [`GatewayMetrics`]; a no-op when metrics are disabled (unit tests).
pub struct PrometheusPlugin {
    /// Accepted for APISIX config compatibility; inert (see module docs).
    prefer_name: bool,
    /// Shared metrics handle from `PluginResources`; `None` disables recording.
    metrics: Option<Arc<GatewayMetrics>>,
}

impl PrometheusPlugin {
    /// Builds the plugin from node config.
    ///
    /// Config keys (all optional):
    /// - `prefer_name` (bool, default `false`): accepted for parity with
    ///   APISIX's `prometheus` plugin; inert in featherbit (the `route` label
    ///   is always the request host — see module docs).
    ///
    /// ```yaml
    /// type: prometheus
    /// config:
    ///   prefer_name: true
    /// ```
    pub fn from_config(
        config: &HashMap<String, serde_json::Value>,
        resources: &Arc<PluginResources>,
    ) -> Result<Self, String> {
        let prefer_name = config
            .get("prefer_name")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        Ok(Self {
            prefer_name,
            metrics: resources.metrics.clone(),
        })
    }

    /// The `consumer` label for this request: the consumer name attached by an
    /// upstream auth node, or `anonymous` when none is present.
    fn consumer_label(ctx: &Context) -> String {
        ctx.message
            .get("consumer.name")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .unwrap_or("anonymous")
            .to_string()
    }

    /// The `route` label for this request. featherbit has no route object on
    /// the context here, so the request host is used (low-cardinality);
    /// falls back to `unknown` when absent.
    fn route_label(ctx: &Context) -> String {
        let host = ctx.request.host.trim();
        if host.is_empty() {
            "unknown".to_string()
        } else {
            host.to_string()
        }
    }
}

#[async_trait]
impl Plugin for PrometheusPlugin {
    fn plugin_type(&self) -> &str {
        "prometheus"
    }

    async fn execute(
        &self,
        ctx: Context,
        _named_inputs: &HashMap<String, serde_json::Value>,
    ) -> PluginResult {
        if let Some(ref metrics) = self.metrics {
            let consumer = Self::consumer_label(&ctx);
            let route = Self::route_label(&ctx);
            metrics
                .consumer_requests
                .with_label_values(&[&consumer, &route])
                .inc();
        }

        // `prefer_name` is intentionally read-but-inert; touch it so the field
        // is never flagged as dead while documenting its parity purpose.
        let _ = self.prefer_name;

        Ok(PluginOutput {
            context: ctx,
            named_outputs: HashMap::new(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::{GatewayRequest, GatewayResponse, Protocol};
    use bytes::Bytes;

    fn test_ctx(consumer: Option<&str>) -> Context {
        let mut message = HashMap::new();
        if let Some(name) = consumer {
            message.insert("consumer.name".to_string(), serde_json::json!(name));
        }
        Context {
            request: GatewayRequest {
                method: "GET".to_string(),
                path: "/api/users".to_string(),
                host: "api.example.com".to_string(),
                scheme: "http".to_string(),
                headers: HashMap::new(),
                query_params: HashMap::new(),
                body: Bytes::new(),
                remote_addr: "10.0.0.1:1234".to_string(),
                protocol: Protocol::Http1,
            },
            response: GatewayResponse {
                status_code: 200,
                headers: HashMap::new(),
                body: Bytes::new(),
            },
            message,
            errors: Vec::new(),
        }
    }

    fn plugin_with_metrics(metrics: Arc<GatewayMetrics>) -> PrometheusPlugin {
        let resources = PluginResources::new(Some(metrics));
        PrometheusPlugin::from_config(&HashMap::new(), &resources).unwrap()
    }

    #[test]
    fn from_config_reads_prefer_name() {
        let cfg: HashMap<String, serde_json::Value> =
            serde_json::from_value(serde_json::json!({ "prefer_name": true })).unwrap();
        let p = PrometheusPlugin::from_config(&cfg, &PluginResources::empty()).unwrap();
        assert!(p.prefer_name);
        // default false
        let p = PrometheusPlugin::from_config(&HashMap::new(), &PluginResources::empty()).unwrap();
        assert!(!p.prefer_name);
    }

    #[tokio::test]
    async fn execute_bumps_consumer_counter() {
        let metrics = Arc::new(GatewayMetrics::new());
        let p = plugin_with_metrics(metrics.clone());

        let out = p
            .execute(test_ctx(Some("alice")), &HashMap::new())
            .await
            .unwrap();
        // context passes through unchanged
        assert_eq!(out.context.request.path, "/api/users");

        assert_eq!(
            metrics
                .consumer_requests
                .with_label_values(&["alice", "api.example.com"])
                .get(),
            1
        );

        // a second request for the same consumer increments again
        p.execute(test_ctx(Some("alice")), &HashMap::new())
            .await
            .unwrap();
        assert_eq!(
            metrics
                .consumer_requests
                .with_label_values(&["alice", "api.example.com"])
                .get(),
            2
        );
    }

    #[tokio::test]
    async fn execute_defaults_to_anonymous() {
        let metrics = Arc::new(GatewayMetrics::new());
        let p = plugin_with_metrics(metrics.clone());

        p.execute(test_ctx(None), &HashMap::new()).await.unwrap();
        assert_eq!(
            metrics
                .consumer_requests
                .with_label_values(&["anonymous", "api.example.com"])
                .get(),
            1
        );
    }

    #[tokio::test]
    async fn execute_is_noop_without_metrics() {
        // resources.metrics == None must not panic and must pass ctx through.
        let p = PrometheusPlugin::from_config(&HashMap::new(), &PluginResources::empty()).unwrap();
        let out = p
            .execute(test_ctx(Some("bob")), &HashMap::new())
            .await
            .unwrap();
        assert_eq!(out.context.response.status_code, 200);
    }
}
