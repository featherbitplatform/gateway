//! Distributed tracing via Zipkin / B3 propagation (`zipkin`).
//!
//! The featherbit port of Apache APISIX's `zipkin` plugin. Where APISIX runs it
//! across nginx phases, featherbit models it as a **start/end node pair** wired
//! around the `upstream` node, using the shared
//! [`trace`](crate::plugins::util::trace) helper to carry a [`SpanContext`]
//! through `context.message`:
//!
//! - **start node** (`phase: start`, placed right after `listener`, before
//!   `upstream`): extracts B3 propagation from either the single `b3` header or
//!   the `x-b3-*` multi-header form. If present, this hop *continues* the
//!   incoming trace (same `trace_id`, the caller's span becomes our parent, the
//!   incoming sampled flag is honored). If absent, a *new* trace is started
//!   (`new_trace_id()`, no parent, sampled per `sample_ratio`). This hop gets a
//!   fresh `span_id` and start time, the span is stored via [`store_span`], and
//!   the outgoing `x-b3-*` headers are injected so the upstream continues the
//!   trace.
//! - **end node** (`phase: end`, placed after `upstream`, right before
//!   `client`): loads the span, computes its duration, and — when sampled —
//!   builds a Zipkin v2 JSON span array and **fire-and-forgets** a POST to the
//!   collector `endpoint` on a detached `tokio::spawn` task (the same
//!   best-effort pattern as `proxy-mirror`). The export result is ignored and
//!   never blocks or fails the request.
//!
//! Both nodes always return `Ok`: tracing is observability, so it never
//! short-circuits the request through an error port. If the end node finds no
//! stored span it passes through untouched.
//!
//! ## Deviations from APISIX
//! - Spans are exported one-per-request (fire-and-forget) rather than through a
//!   batched reporter; only span_version 2 (the APISIX default) is modeled, and
//!   only a single `SERVER` span per hop (no `apisix.rewrite`/`proxy` children).
//! - The new-trace sampling draw is a *pseudo-random*, per-trace-consistent
//!   hash of the trace id (no `rand` crate), so a given trace id always samples
//!   the same way in a process.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{json, Value};

use crate::context::Context;
use crate::outbound::{OutboundClient, OutboundRequest};
use crate::plugins::resources::PluginResources;
use crate::plugins::util::trace::{
    build_b3_headers, load_span, new_span_id, new_trace_id, now_ms, parse_b3, store_span,
    SpanContext,
};
use crate::plugins::{Plugin, PluginOutput, PluginResult};

/// Which node of the tracing pair this instance is.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Phase {
    /// Before `upstream`: extract/create the span and inject `x-b3-*` headers.
    Start,
    /// After `upstream`: export the finished span (fire-and-forget).
    End,
}

/// A pseudo-random draw in `[0.0, 1.0)` derived deterministically from a trace
/// id, so the same trace id always samples the same way within a process.
/// Not cryptographic; good enough for `sample_ratio` without a `rand` crate.
fn ratio_draw(trace_id: &str) -> f64 {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut h = DefaultHasher::new();
    trace_id.hash(&mut h);
    h.finish() as f64 / (u64::MAX as f64 + 1.0)
}

/// One node of the Zipkin tracing pair.
pub struct ZipkinPlugin {
    phase: Phase,
    /// Full collector URL spans POST to, e.g.
    /// `http://localhost:9411/api/v2/spans`.
    endpoint: String,
    service_name: String,
    /// Optional `localEndpoint.ipv4` reported on each span.
    server_addr: Option<String>,
    /// Fraction of new traces to sample, in `0.0..=1.0`.
    sample_ratio: f64,
    ssl_verify: bool,
    timeout: Duration,
    client: Arc<OutboundClient>,
}

impl ZipkinPlugin {
    /// Builds the plugin from node config.
    ///
    /// Config keys:
    ///
    /// | Key | Type | Default | Description |
    /// |---|---|---|---|
    /// | `phase` | string | — (**required**) | `start` (before upstream) or `end` (after upstream). |
    /// | `endpoint` | string | — (**required**) | Full Zipkin v2 collector URL, e.g. `http://localhost:9411/api/v2/spans`. |
    /// | `service_name` | string | `"featherbit"` | `localEndpoint.serviceName` reported on each span. |
    /// | `server_addr` | string | — | Optional `localEndpoint.ipv4` for the reporting host. |
    /// | `sample_ratio` | number | `1.0` | Fraction of new traces to sample, in `0.0..=1.0`. |
    /// | `ssl_verify` | bool | `true` | Verify the collector's TLS certificate. |
    /// | `timeout` | int (seconds) | `3` | Export request timeout (end node). |
    ///
    /// Fails fast on an unknown `phase`, a missing/empty `endpoint`, or a
    /// `sample_ratio` outside `0.0..=1.0`. The same config (including
    /// `endpoint`) is placed on both the start and end nodes.
    ///
    /// ```yaml
    /// # start node — right after listener, before upstream
    /// - id: zipkin-start
    ///   type: zipkin
    ///   config:
    ///     phase: start
    ///     endpoint: http://localhost:9411/api/v2/spans
    ///     service_name: my-gateway
    ///     sample_ratio: 0.1
    /// # end node — after upstream, right before client
    /// - id: zipkin-end
    ///   type: zipkin
    ///   config:
    ///     phase: end
    ///     endpoint: http://localhost:9411/api/v2/spans
    /// ```
    pub fn from_config(
        config: &HashMap<String, Value>,
        resources: &Arc<PluginResources>,
    ) -> Result<Self, String> {
        let phase = match config.get("phase").and_then(|v| v.as_str()) {
            Some("start") => Phase::Start,
            Some("end") => Phase::End,
            Some(other) => {
                return Err(format!(
                    "zipkin `phase` must be `start` or `end` (got `{other}`)"
                ))
            }
            None => return Err("zipkin requires `phase` (`start` or `end`)".to_string()),
        };

        let endpoint = config
            .get("endpoint")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .ok_or("zipkin requires `endpoint` (e.g. \"http://localhost:9411/api/v2/spans\")")?
            .to_string();

        let service_name = config
            .get("service_name")
            .and_then(|v| v.as_str())
            .unwrap_or("featherbit")
            .to_string();

        let server_addr = config
            .get("server_addr")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(String::from);

        let sample_ratio = match config.get("sample_ratio") {
            None => 1.0,
            Some(v) => v
                .as_f64()
                .filter(|r| (0.0..=1.0).contains(r))
                .ok_or("zipkin `sample_ratio` must be a number in 0.0..=1.0")?,
        };

        let ssl_verify = config
            .get("ssl_verify")
            .and_then(|v| v.as_bool())
            .unwrap_or(true);
        let timeout =
            Duration::from_secs(config.get("timeout").and_then(|v| v.as_u64()).unwrap_or(3));

        Ok(Self {
            phase,
            endpoint,
            service_name,
            server_addr,
            sample_ratio,
            ssl_verify,
            timeout,
            client: resources.outbound.clone(),
        })
    }

    /// Decides whether a brand-new trace should be sampled, per `sample_ratio`.
    fn sample_new(&self, trace_id: &str) -> bool {
        if self.sample_ratio >= 1.0 {
            true
        } else if self.sample_ratio <= 0.0 {
            false
        } else {
            ratio_draw(trace_id) < self.sample_ratio
        }
    }

    /// START: extract or create the span, store it, and inject `x-b3-*`.
    fn run_start(&self, ctx: &mut Context) {
        let first = |name: &str| {
            ctx.request
                .headers
                .get(name)
                .and_then(|v| v.first())
                .map(|s| s.as_str())
        };
        let incoming = parse_b3(
            first("b3"),
            first("x-b3-traceid"),
            first("x-b3-spanid"),
            first("x-b3-sampled"),
        );

        let (trace_id, parent_span_id, sampled) = match incoming {
            Some((trace_id, parent, sampled)) => (trace_id, Some(parent), sampled),
            None => {
                let trace_id = new_trace_id();
                let sampled = self.sample_new(&trace_id);
                (trace_id, None, sampled)
            }
        };

        let span = SpanContext {
            trace_id,
            span_id: new_span_id(),
            parent_span_id,
            sampled,
            start_ms: now_ms(),
        };

        // Inject the downstream B3 headers so the upstream continues the trace.
        for (name, value) in build_b3_headers(&span) {
            ctx.request.headers.insert(name, vec![value]);
        }

        store_span(ctx, &span);
    }

    /// END: load the span and fire-and-forget the Zipkin export when sampled.
    fn run_end(&self, ctx: &Context) {
        let Some(span) = load_span(ctx) else {
            return;
        };
        if !span.sampled {
            return;
        }

        let payload = build_zipkin(&span, ctx, &self.service_name, self.server_addr.as_deref());
        let body = match serde_json::to_vec(&payload) {
            Ok(b) => b,
            Err(_) => return,
        };

        let req = OutboundRequest {
            method: http::Method::POST,
            url: self.endpoint.clone(),
            headers: vec![("Content-Type".to_string(), "application/json".to_string())],
            body: body.into(),
            timeout: self.timeout,
            ssl_verify: self.ssl_verify,
        };
        let client = self.client.clone();
        // Fire-and-forget: the export never blocks or affects the request path.
        tokio::spawn(async move {
            let _ = client.request(req).await;
        });
    }
}

/// Builds the Zipkin v2 JSON span array for a finished span (pure; no I/O).
///
/// `timestamp` and `duration` are microseconds (Zipkin's unit); `parentId` is
/// omitted for a root span.
fn build_zipkin(
    span: &SpanContext,
    ctx: &Context,
    service_name: &str,
    server_addr: Option<&str>,
) -> Value {
    let timestamp_micros = span.start_ms.saturating_mul(1_000);
    let duration_micros = span.duration_ms().saturating_mul(1_000);
    let name = format!("{} {}", ctx.request.method, ctx.request.path);

    let mut local_endpoint = json!({ "serviceName": service_name });
    if let Some(addr) = server_addr {
        local_endpoint["ipv4"] = json!(addr);
    }

    let mut span_json = json!({
        "traceId": span.trace_id,
        "id": span.span_id,
        "name": name,
        "kind": "SERVER",
        "timestamp": timestamp_micros,
        "duration": duration_micros,
        "localEndpoint": local_endpoint,
        "tags": {
            "http.method": ctx.request.method,
            "http.status_code": ctx.response.status_code.to_string(),
            "http.path": ctx.request.path,
        },
    });
    if let Some(parent) = &span.parent_span_id {
        span_json["parentId"] = json!(parent);
    }

    json!([span_json])
}

#[async_trait]
impl Plugin for ZipkinPlugin {
    fn plugin_type(&self) -> &str {
        "zipkin"
    }

    async fn execute(
        &self,
        mut ctx: Context,
        _named_inputs: &HashMap<String, Value>,
    ) -> PluginResult {
        match self.phase {
            Phase::Start => self.run_start(&mut ctx),
            Phase::End => self.run_end(&ctx),
        }
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

    fn plugin(config: Value) -> Result<ZipkinPlugin, String> {
        let map: HashMap<String, Value> = serde_json::from_value(config).unwrap();
        ZipkinPlugin::from_config(&map, &PluginResources::empty())
    }

    fn test_ctx() -> Context {
        Context {
            request: GatewayRequest {
                method: "POST".to_string(),
                path: "/api/users".to_string(),
                host: "example.com".to_string(),
                scheme: "http".to_string(),
                headers: HashMap::new(),
                query_params: HashMap::new(),
                body: Bytes::new(),
                remote_addr: "10.0.0.1:5555".to_string(),
                protocol: Protocol::Http1,
            },
            response: GatewayResponse {
                status_code: 201,
                headers: HashMap::new(),
                body: Bytes::new(),
            },
            message: HashMap::new(),
            errors: Vec::new(),
        }
    }

    fn a_span(sampled: bool, parent: Option<&str>) -> SpanContext {
        SpanContext {
            trace_id: "0af7651916cd43dd8448eb211c80319c".to_string(),
            span_id: "b7ad6b7169203331".to_string(),
            parent_span_id: parent.map(String::from),
            sampled,
            start_ms: 1_700_000_000_000,
        }
    }

    #[test]
    fn config_requires_phase_and_endpoint() {
        assert!(plugin(json!({})).is_err());
        assert!(plugin(json!({ "phase": "start" })).is_err()); // missing endpoint
        assert!(plugin(json!({ "phase": "bogus", "endpoint": "http://z" })).is_err());
        assert!(plugin(json!({ "phase": "start", "endpoint": "http://z" })).is_ok());
        // sample_ratio out of range
        assert!(
            plugin(json!({ "phase": "start", "endpoint": "http://z", "sample_ratio": 2 })).is_err()
        );
    }

    #[test]
    fn sample_ratio_extremes() {
        let never =
            plugin(json!({ "phase": "start", "endpoint": "http://z", "sample_ratio": 0 })).unwrap();
        let always =
            plugin(json!({ "phase": "start", "endpoint": "http://z", "sample_ratio": 1 })).unwrap();
        for _ in 0..50 {
            assert!(!never.sample_new("abc"));
            assert!(always.sample_new("abc"));
        }
    }

    #[test]
    fn zipkin_payload_shape() {
        let span = a_span(true, Some("0020000000000001"));
        let v = build_zipkin(&span, &test_ctx(), "svc", Some("10.0.0.5"));
        let s = &v[0];
        assert_eq!(s["traceId"], "0af7651916cd43dd8448eb211c80319c");
        assert_eq!(s["id"], "b7ad6b7169203331");
        assert_eq!(s["parentId"], "0020000000000001");
        assert_eq!(s["kind"], "SERVER");
        assert_eq!(s["name"], "POST /api/users");
        // micros
        assert_eq!(s["timestamp"], 1_700_000_000_000_000u64);
        assert_eq!(s["localEndpoint"]["serviceName"], "svc");
        assert_eq!(s["localEndpoint"]["ipv4"], "10.0.0.5");
        assert_eq!(s["tags"]["http.method"], "POST");
        assert_eq!(s["tags"]["http.status_code"], "201");
        assert_eq!(s["tags"]["http.path"], "/api/users");
    }

    #[test]
    fn zipkin_omits_parent_when_root() {
        let span = a_span(true, None);
        let v = build_zipkin(&span, &test_ctx(), "svc", None);
        assert!(v[0].get("parentId").is_none());
        assert!(v[0]["localEndpoint"].get("ipv4").is_none());
    }

    #[tokio::test]
    async fn start_creates_new_trace_and_injects_headers() {
        let p =
            plugin(json!({ "phase": "start", "endpoint": "http://z", "sample_ratio": 1 })).unwrap();
        let out = p.execute(test_ctx(), &HashMap::new()).await.unwrap();
        let span = load_span(&out.context).expect("span stored");
        assert!(span.parent_span_id.is_none());
        assert!(span.sampled);
        assert_eq!(span.trace_id.len(), 32);
        // downstream B3 headers injected, matching the span
        assert_eq!(
            out.context.request.headers["x-b3-traceid"][0],
            span.trace_id
        );
        assert_eq!(out.context.request.headers["x-b3-spanid"][0], span.span_id);
        assert_eq!(out.context.request.headers["x-b3-sampled"][0], "1");
    }

    #[tokio::test]
    async fn start_continues_incoming_b3_single() {
        let p = plugin(json!({ "phase": "start", "endpoint": "http://z" })).unwrap();
        let mut ctx = test_ctx();
        ctx.request
            .headers
            .insert("b3".to_string(), vec!["trace1-span1-1".to_string()]);
        let out = p.execute(ctx, &HashMap::new()).await.unwrap();
        let span = load_span(&out.context).unwrap();
        assert_eq!(span.trace_id, "trace1");
        assert_eq!(span.parent_span_id.as_deref(), Some("span1"));
        assert!(span.sampled);
        // our own span id replaces the caller's
        assert_ne!(span.span_id, "span1");
    }

    #[tokio::test]
    async fn start_continues_incoming_b3_multi() {
        let p = plugin(json!({ "phase": "start", "endpoint": "http://z" })).unwrap();
        let mut ctx = test_ctx();
        ctx.request
            .headers
            .insert("x-b3-traceid".to_string(), vec!["t9".to_string()]);
        ctx.request
            .headers
            .insert("x-b3-spanid".to_string(), vec!["s9".to_string()]);
        ctx.request
            .headers
            .insert("x-b3-sampled".to_string(), vec!["0".to_string()]);
        let out = p.execute(ctx, &HashMap::new()).await.unwrap();
        let span = load_span(&out.context).unwrap();
        assert_eq!(span.trace_id, "t9");
        assert_eq!(span.parent_span_id.as_deref(), Some("s9"));
        assert!(!span.sampled);
    }

    #[tokio::test]
    async fn end_passes_through_without_span() {
        let p = plugin(json!({ "phase": "end", "endpoint": "http://z" })).unwrap();
        let out = p.execute(test_ctx(), &HashMap::new()).await.unwrap();
        assert_eq!(out.context.response.status_code, 201);
    }

    #[tokio::test]
    async fn end_returns_ok_with_stored_span() {
        let p = plugin(json!({ "phase": "end", "endpoint": "http://127.0.0.1:1/api/v2/spans" }))
            .unwrap();
        let mut ctx = test_ctx();
        store_span(&mut ctx, &a_span(true, None));
        // The spawned export is best-effort; we only assert execute returns Ok.
        let out = p.execute(ctx, &HashMap::new()).await.unwrap();
        assert_eq!(out.context.request.path, "/api/users");
    }

    #[tokio::test]
    async fn end_skips_export_when_unsampled() {
        let p = plugin(json!({ "phase": "end", "endpoint": "http://z" })).unwrap();
        let mut ctx = test_ctx();
        store_span(&mut ctx, &a_span(false, None));
        let out = p.execute(ctx, &HashMap::new()).await.unwrap();
        assert_eq!(out.context.response.status_code, 201);
    }
}
