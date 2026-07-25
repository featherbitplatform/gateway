//! Distributed tracing with Apache SkyWalking — a **start/end node pair**
//! wrapped around the `upstream` node.
//!
//! Ported from APISIX's `skywalking.lua`. The wire propagation format is the
//! SkyWalking `sw8` header (see [`crate::plugins::util::trace`]).
//!
//! - The **start** node (`phase: start`, placed **before** `upstream`) extracts
//!   an incoming `sw8` header to continue an existing trace, or begins a new one
//!   and makes the sampling decision. It creates this hop's
//!   [`SpanContext`](crate::plugins::util::trace::SpanContext), stores it in
//!   `context.message`, and injects a fresh downstream `sw8` header so the
//!   upstream service joins the trace.
//! - The **end** node (`phase: end`, placed **after** `upstream`) loads the
//!   span, and — when sampled — fire-and-forget POSTs a SkyWalking trace
//!   *segment* to `<endpoint_addr>/v3/segments` on a detached task. The request
//!   path is never blocked by the export.
//!
//! Both nodes always continue through the `success` port; neither ever routes
//! to the error port.
//!
//! ## Wiring
//! ```text
//! ... -> skywalking(start) -> upstream -> skywalking(end) -> ...
//! ```
//!
//! ## Deviations / simplifications from APISIX
//! Full SkyWalking correlation (segment references, multi-span segments, the
//! `ngx.ctx` entry/exit span pair, the background report timer) is
//! intentionally reduced to a **faithful subset**:
//! - Each request exports a **single-span** segment (`spanId: 0`,
//!   `parentSpanId: -1`, `spanType: "Entry"`, `spanLayer: "Http"`). The
//!   incoming `sw8`'s parent segment/service are honored for propagation
//!   (trace id + sampling flag are continued) but are **not** emitted as a
//!   formal `refs` segment-reference on the exported span.
//! - Export is per-request and immediate (a detached `tokio::spawn`), not
//!   buffered on APISIX's `report_interval` timer.
//! - The segment is POSTed as a single JSON object to `/v3/segments`; the real
//!   OAP endpoint also accepts a batch array — the single-object form is the
//!   documented subset here.
//! - `componentId` is reported as `49` (generic HTTP) rather than APISIX's
//!   `6002`.

use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};

use crate::context::Context;
use crate::outbound::{OutboundClient, OutboundRequest};
use crate::plugins::resources::PluginResources;
use crate::plugins::util::trace::{
    load_span, new_span_id, new_trace_id, now_ms, parse_sw8, store_span, sw8_encode, SpanContext,
};
use crate::plugins::{Plugin, PluginOutput, PluginResult};

/// SkyWalking `componentId` reported for the entry span (49 = generic HTTP).
const COMPONENT_ID_HTTP: i64 = 49;

/// Which node of the start/end pair this instance is.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Phase {
    /// Before `upstream`: extract/create the span and inject `sw8`.
    Start,
    /// After `upstream`: load the span and export the segment.
    End,
}

/// SkyWalking tracing node (a start or end half, selected by `phase`).
pub struct SkywalkingPlugin {
    phase: Phase,
    /// OAP HTTP base, e.g. `http://127.0.0.1:12800` (no trailing slash).
    endpoint_addr: String,
    service_name: String,
    service_instance_name: String,
    /// Fraction of new traces to sample, in `0.0..=1.0`.
    sample_ratio: f64,
    ssl_verify: bool,
    timeout: Duration,
    /// Shared pooled outbound HTTP client (used by the end node's export).
    client: Arc<OutboundClient>,
}

/// Draws a pseudo-random fraction in `[0.0, 1.0)` for sampling — the same
/// cheap, non-cryptographic source `proxy-mirror`/`fault-injection` use.
fn roll_fraction() -> f64 {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};
    let n = RandomState::new().build_hasher().finish();
    n as f64 / (u64::MAX as f64 + 1.0)
}

/// Builds a downstream `sw8` header value for `span` (8 hyphen-separated
/// fields, matching the SkyWalking cross-process propagation header):
/// `sample-traceId-parentSegmentId-parentSpanId-service-instance-endpoint-peer`.
/// All fields except the sample flag and the plain-integer parent span id are
/// base64-encoded.
fn build_sw8(span: &SpanContext, service: &str, instance: &str, endpoint: &str) -> String {
    format!(
        "{}-{}-{}-{}-{}-{}-{}-{}",
        if span.sampled { "1" } else { "0" },
        sw8_encode(&span.trace_id),
        sw8_encode(&span.span_id), // this hop's segment id
        "0",                       // parent span id within our segment (plain int)
        sw8_encode(service),
        sw8_encode(instance),
        sw8_encode(endpoint),
        sw8_encode(endpoint), // target address (peer); we reuse the endpoint
    )
}

/// Builds the SkyWalking trace *segment* JSON for a finished span (pure; does
/// no I/O). A single Entry/Http span mirroring the OAP HTTP segment protocol.
fn build_segment(
    span: &SpanContext,
    service: &str,
    instance: &str,
    method: &str,
    path: &str,
    status: u16,
    end_ms: u64,
) -> Value {
    json!({
        "traceId": span.trace_id,
        "traceSegmentId": span.span_id,
        "service": service,
        "serviceInstance": instance,
        "spans": [{
            "spanId": 0,
            "parentSpanId": -1,
            "startTime": span.start_ms,
            "endTime": end_ms,
            "operationName": path,
            "spanType": "Entry",
            "spanLayer": "Http",
            "componentId": COMPONENT_ID_HTTP,
            "isError": status >= 400,
            "tags": [
                { "key": "http.method", "value": method },
                { "key": "http.path", "value": path },
                { "key": "http.status_code", "value": status.to_string() },
            ],
        }],
    })
}

impl SkywalkingPlugin {
    /// Builds the plugin from node config.
    ///
    /// Config keys:
    ///
    /// | Key | Type | Default | Description |
    /// |---|---|---|---|
    /// | `phase` | string | — (**required**) | `start` (before `upstream`) or `end` (after `upstream`). |
    /// | `endpoint_addr` | string | `http://127.0.0.1:12800` | SkyWalking OAP HTTP base. |
    /// | `service_name` | string | `"featherbit"` | Service name reported on the segment / `sw8`. |
    /// | `service_instance_name` | string | `"featherbit Instance Name"` | Service instance name. |
    /// | `sample_ratio` | number | `1.0` | Fraction of *new* traces to sample, in `0.0..=1.0`. Requests arriving with a sampled `sw8` are always continued. |
    /// | `ssl_verify` | bool | `true` | Verify the OAP TLS certificate on export. |
    /// | `timeout` | int (seconds) | `3` | Per-export HTTP timeout (end node). |
    ///
    /// ```yaml
    /// # before upstream
    /// - id: sw-start
    ///   type: skywalking
    ///   config: { phase: start, service_name: my-gateway, sample_ratio: 1.0 }
    /// # after upstream
    /// - id: sw-end
    ///   type: skywalking
    ///   config: { phase: end, endpoint_addr: http://127.0.0.1:12800, service_name: my-gateway }
    /// ```
    pub fn from_config(
        config: &HashMap<String, Value>,
        resources: &Arc<PluginResources>,
    ) -> Result<Self, String> {
        let phase = match config.get("phase").and_then(|v| v.as_str()) {
            Some("start") => Phase::Start,
            Some("end") => Phase::End,
            _ => {
                return Err(
                    "skywalking requires `phase: start` (before upstream) or `phase: end` (after upstream)"
                        .to_string(),
                )
            }
        };

        let endpoint_addr = config
            .get("endpoint_addr")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .unwrap_or("http://127.0.0.1:12800")
            .trim_end_matches('/')
            .to_string();

        let service_name = config
            .get("service_name")
            .and_then(|v| v.as_str())
            .unwrap_or("featherbit")
            .to_string();

        let service_instance_name = config
            .get("service_instance_name")
            .and_then(|v| v.as_str())
            .unwrap_or("featherbit Instance Name")
            .to_string();

        let sample_ratio = match config.get("sample_ratio") {
            None => 1.0,
            Some(v) => v
                .as_f64()
                .filter(|r| (0.0..=1.0).contains(r))
                .ok_or("skywalking `sample_ratio` must be a number in 0.0..=1.0")?,
        };

        let ssl_verify = config
            .get("ssl_verify")
            .and_then(|v| v.as_bool())
            .unwrap_or(true);

        let timeout =
            Duration::from_secs(config.get("timeout").and_then(|v| v.as_u64()).unwrap_or(3));

        Ok(Self {
            phase,
            endpoint_addr,
            service_name,
            service_instance_name,
            sample_ratio,
            ssl_verify,
            timeout,
            client: resources.outbound.clone(),
        })
    }

    /// Sampling decision for a *new* trace (no inbound `sw8`).
    fn should_sample(&self) -> bool {
        if self.sample_ratio >= 1.0 {
            true
        } else if self.sample_ratio <= 0.0 {
            false
        } else {
            roll_fraction() < self.sample_ratio
        }
    }

    /// Start-node logic: continue or begin the trace, store the span, and
    /// inject the downstream `sw8` header.
    fn run_start(&self, mut ctx: Context) -> Context {
        let incoming = ctx
            .request
            .headers
            .get("sw8")
            .and_then(|v| v.first())
            .and_then(|s| parse_sw8(s));

        let (trace_id, parent_span_id, sampled) = match incoming {
            Some((trace_id, parent, sampled)) => (trace_id, Some(parent), sampled),
            None => (new_trace_id(), None, self.should_sample()),
        };

        let span = SpanContext {
            trace_id,
            span_id: new_span_id(),
            parent_span_id,
            sampled,
            start_ms: now_ms(),
        };
        store_span(&mut ctx, &span);

        let sw8 = build_sw8(
            &span,
            &self.service_name,
            &self.service_instance_name,
            &ctx.request.path,
        );
        ctx.request.headers.insert("sw8".to_string(), vec![sw8]);

        ctx
    }

    /// End-node logic: load the span and, when sampled, fire-and-forget export
    /// the segment. Returns the context unchanged.
    fn run_end(&self, ctx: Context) -> Context {
        if let Some(span) = load_span(&ctx) {
            if span.sampled {
                let segment = build_segment(
                    &span,
                    &self.service_name,
                    &self.service_instance_name,
                    &ctx.request.method,
                    &ctx.request.path,
                    ctx.response.status_code,
                    now_ms(),
                );
                let body = serde_json::to_vec(&segment).unwrap_or_default();
                let req = OutboundRequest {
                    method: http::Method::POST,
                    url: format!("{}/v3/segments", self.endpoint_addr),
                    headers: vec![("Content-Type".to_string(), "application/json".to_string())],
                    body: body.into(),
                    timeout: self.timeout,
                    ssl_verify: self.ssl_verify,
                };
                let client = self.client.clone();
                tokio::spawn(async move {
                    let _ = client.request(req).await;
                });
            }
        }
        ctx
    }
}

#[async_trait]
impl Plugin for SkywalkingPlugin {
    fn plugin_type(&self) -> &str {
        "skywalking"
    }

    async fn execute(&self, ctx: Context, _named_inputs: &HashMap<String, Value>) -> PluginResult {
        let ctx = match self.phase {
            Phase::Start => self.run_start(ctx),
            Phase::End => self.run_end(ctx),
        };
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

    fn test_ctx() -> Context {
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
            message: HashMap::new(),
            errors: Vec::new(),
        }
    }

    fn plugin(config: Value) -> Result<SkywalkingPlugin, String> {
        let map: HashMap<String, Value> = serde_json::from_value(config).unwrap();
        SkywalkingPlugin::from_config(&map, &PluginResources::empty())
    }

    #[test]
    fn from_config_requires_phase() {
        assert!(plugin(json!({})).is_err());
        assert!(plugin(json!({ "phase": "middle" })).is_err());
        assert!(plugin(json!({ "phase": "start" })).is_ok());
        assert!(plugin(json!({ "phase": "end" })).is_ok());
        // sample_ratio out of range
        assert!(plugin(json!({ "phase": "start", "sample_ratio": 2 })).is_err());
    }

    #[test]
    fn from_config_defaults() {
        let p = plugin(json!({ "phase": "end" })).unwrap();
        assert_eq!(p.endpoint_addr, "http://127.0.0.1:12800");
        assert_eq!(p.service_name, "featherbit");
        assert_eq!(p.service_instance_name, "featherbit Instance Name");
        assert_eq!(p.sample_ratio, 1.0);
        assert!(p.ssl_verify);
    }

    #[test]
    fn sampler_decision() {
        let never = plugin(json!({ "phase": "start", "sample_ratio": 0 })).unwrap();
        let always = plugin(json!({ "phase": "start", "sample_ratio": 1 })).unwrap();
        for _ in 0..100 {
            assert!(!never.should_sample(), "ratio 0 must never sample");
            assert!(always.should_sample(), "ratio 1 must always sample");
        }
    }

    #[test]
    fn start_stores_span_and_injects_sw8() {
        let p = plugin(json!({ "phase": "start", "sample_ratio": 1 })).unwrap();
        let ctx = p.run_start(test_ctx());

        // span stored for the end node
        let span = load_span(&ctx).expect("start node must store a span");
        assert_eq!(span.trace_id.len(), 32);
        assert_eq!(span.span_id.len(), 16);
        assert!(span.sampled);

        // downstream sw8 header injected and round-trips back to our span
        let header = ctx.request.headers.get("sw8").unwrap().first().unwrap();
        let (trace_id, parent_span, sampled) = parse_sw8(header).expect("injected sw8 must parse");
        assert_eq!(trace_id, span.trace_id);
        assert_eq!(parent_span, "0");
        assert!(sampled);
    }

    #[test]
    fn start_continues_incoming_trace() {
        let p = plugin(json!({ "phase": "start", "sample_ratio": 0 })).unwrap();
        let incoming_trace = "1f2d4bf47bf711eab794acde48001122";
        let incoming = format!(
            "1-{}-{}-7-{}-{}-{}-{}",
            sw8_encode(incoming_trace),
            sw8_encode("parent-segment"),
            sw8_encode("caller-svc"),
            sw8_encode("caller-inst"),
            sw8_encode("/caller"),
            sw8_encode("peer:80"),
        );
        let mut ctx = test_ctx();
        ctx.request
            .headers
            .insert("sw8".to_string(), vec![incoming]);

        let ctx = p.run_start(ctx);
        let span = load_span(&ctx).unwrap();
        // trace id + sampled flag continued from the incoming header, despite
        // sample_ratio 0 (which only governs *new* traces)
        assert_eq!(span.trace_id, incoming_trace);
        assert!(span.sampled);
        assert_eq!(span.parent_span_id.as_deref(), Some("7"));
    }

    #[test]
    fn segment_payload_shape() {
        let span = SpanContext {
            trace_id: "1f2d4bf47bf711eab794acde48001122".to_string(),
            span_id: "b7ad6b7169203331".to_string(),
            parent_span_id: None,
            sampled: true,
            start_ms: 1_700_000_000_000,
        };
        let seg = build_segment(
            &span,
            "svc",
            "inst",
            "POST",
            "/api/x",
            500,
            1_700_000_000_123,
        );
        assert_eq!(seg["traceId"], span.trace_id);
        assert_eq!(seg["traceSegmentId"], span.span_id);
        assert_eq!(seg["service"], "svc");
        assert_eq!(seg["serviceInstance"], "inst");
        let s = &seg["spans"][0];
        assert_eq!(s["spanId"], 0);
        assert_eq!(s["parentSpanId"], -1);
        assert_eq!(s["spanType"], "Entry");
        assert_eq!(s["spanLayer"], "Http");
        assert_eq!(s["componentId"], COMPONENT_ID_HTTP);
        assert_eq!(s["operationName"], "/api/x");
        assert_eq!(s["startTime"], 1_700_000_000_000u64);
        assert_eq!(s["endTime"], 1_700_000_000_123u64);
        // status 500 -> isError true
        assert_eq!(s["isError"], true);
        assert_eq!(s["tags"][0]["key"], "http.method");
        assert_eq!(s["tags"][0]["value"], "POST");
        assert_eq!(s["tags"][2]["value"], "500");
    }

    #[tokio::test]
    async fn end_returns_ok_without_span() {
        // No start ran -> no span -> nothing exported, still Ok.
        let p = plugin(json!({ "phase": "end" })).unwrap();
        let out = p.execute(test_ctx(), &HashMap::new()).await.unwrap();
        assert_eq!(out.context.response.status_code, 200);
    }

    #[tokio::test]
    async fn end_returns_ok_and_spawns_export_when_sampled() {
        // Point at a dead port; the spawned export fails and is ignored.
        let p = plugin(json!({ "phase": "end", "endpoint_addr": "http://127.0.0.1:1" })).unwrap();
        let start = plugin(json!({ "phase": "start", "sample_ratio": 1 })).unwrap();
        let ctx = start.run_start(test_ctx());
        let out = p.execute(ctx, &HashMap::new()).await.unwrap();
        // context passes through unchanged
        assert_eq!(out.context.request.path, "/api/users");
    }

    #[tokio::test]
    async fn end_skips_export_when_not_sampled() {
        let p = plugin(json!({ "phase": "end" })).unwrap();
        let start = plugin(json!({ "phase": "start", "sample_ratio": 0 })).unwrap();
        let ctx = start.run_start(test_ctx());
        assert!(!load_span(&ctx).unwrap().sampled);
        let out = p.execute(ctx, &HashMap::new()).await.unwrap();
        assert_eq!(out.context.request.path, "/api/users");
    }
}
