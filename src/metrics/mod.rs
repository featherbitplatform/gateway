//! Prometheus metrics for the gateway: per-route request counters and
//! latency histograms plus per-node execution metrics, rendered in the
//! Prometheus text format at the Admin API's `/metrics` endpoint.

use prometheus::{
    Encoder, HistogramOpts, HistogramVec, IntCounterVec, Opts, Registry, TextEncoder,
};

/// Global metrics registry for the gateway.
///
/// Owns a dedicated Prometheus [`Registry`] with all collectors
/// pre-registered. All collectors are internally thread-safe, so a single
/// instance is shared (behind an `Arc`) between the data-plane and the
/// Admin API.
pub struct GatewayMetrics {
    /// Registry holding every collector below; gathered by [`Self::render`].
    pub registry: Registry,
    /// `gateway_requests_total` — requests by `route`, `method`, `status`.
    pub request_count: IntCounterVec,
    /// `gateway_request_duration_seconds` — end-to-end request latency
    /// histogram per `route` (buckets 1ms to 5s).
    pub request_duration: HistogramVec,
    /// `gateway_request_errors_total` — failed requests by `route` and
    /// `error_code`.
    pub request_errors: IntCounterVec,
    /// `gateway_node_executions_total` — graph node executions by `policy`,
    /// `node_id`, `node_type`.
    pub node_execution_count: IntCounterVec,
    /// `gateway_node_duration_seconds` — per-node execution latency
    /// histogram by `policy` and `node_id` (buckets 0.1ms to 500ms).
    pub node_execution_duration: HistogramVec,
    /// `gateway_node_errors_total` — node failures by `policy`, `node_id`,
    /// `error_code`.
    pub node_errors: IntCounterVec,
    /// `gateway_consumer_requests_total` — requests attributed to an
    /// authenticated `consumer`, by `route` (parity with APISIX's prometheus
    /// per-consumer counter). Recorded by the `prometheus` node, which must be
    /// placed after the auth node that attaches the consumer.
    pub consumer_requests: IntCounterVec,
}

impl GatewayMetrics {
    /// Creates a fresh registry with all gateway collectors registered.
    ///
    /// Panics only if a collector cannot be built or registered, which is
    /// impossible with these fixed names/labels — treated as a programmer
    /// error at startup.
    pub fn new() -> Self {
        let registry = Registry::new();

        let request_count = IntCounterVec::new(
            Opts::new("gateway_requests_total", "Total number of requests"),
            &["route", "method", "status"],
        )
        .unwrap();

        let request_duration = HistogramVec::new(
            HistogramOpts::new(
                "gateway_request_duration_seconds",
                "Request duration in seconds",
            )
            .buckets(vec![
                0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 5.0,
            ]),
            &["route"],
        )
        .unwrap();

        let request_errors = IntCounterVec::new(
            Opts::new(
                "gateway_request_errors_total",
                "Total number of request errors",
            ),
            &["route", "error_code"],
        )
        .unwrap();

        let node_execution_count = IntCounterVec::new(
            Opts::new("gateway_node_executions_total", "Total node executions"),
            &["policy", "node_id", "node_type"],
        )
        .unwrap();

        let node_execution_duration = HistogramVec::new(
            HistogramOpts::new("gateway_node_duration_seconds", "Node execution duration")
                .buckets(vec![0.0001, 0.0005, 0.001, 0.005, 0.01, 0.05, 0.1, 0.5]),
            &["policy", "node_id"],
        )
        .unwrap();

        let node_errors = IntCounterVec::new(
            Opts::new("gateway_node_errors_total", "Total node errors"),
            &["policy", "node_id", "error_code"],
        )
        .unwrap();

        let consumer_requests = IntCounterVec::new(
            Opts::new(
                "gateway_consumer_requests_total",
                "Total requests per consumer",
            ),
            &["consumer", "route"],
        )
        .unwrap();

        registry.register(Box::new(request_count.clone())).unwrap();
        registry
            .register(Box::new(request_duration.clone()))
            .unwrap();
        registry.register(Box::new(request_errors.clone())).unwrap();
        registry
            .register(Box::new(node_execution_count.clone()))
            .unwrap();
        registry
            .register(Box::new(node_execution_duration.clone()))
            .unwrap();
        registry.register(Box::new(node_errors.clone())).unwrap();
        registry
            .register(Box::new(consumer_requests.clone()))
            .unwrap();

        Self {
            registry,
            request_count,
            request_duration,
            request_errors,
            node_execution_count,
            node_execution_duration,
            node_errors,
            consumer_requests,
        }
    }

    /// Renders all registered metrics in the Prometheus text exposition
    /// format, as served by the Admin API's `/metrics` endpoint.
    pub fn render(&self) -> String {
        let encoder = TextEncoder::new();
        let metric_families = self.registry.gather();
        let mut buffer = Vec::new();
        encoder.encode(&metric_families, &mut buffer).unwrap();
        String::from_utf8(buffer).unwrap()
    }
}
