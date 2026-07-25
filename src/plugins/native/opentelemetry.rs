//! Distributed tracing via OpenTelemetry (`opentelemetry`).
//!
//! The featherbit port of Apache APISIX's `opentelemetry` plugin. Unlike the
//! single-phase APISIX plugin, this is a **start/end node pair** wired around
//! the `upstream` node, using the shared [`trace`](crate::plugins::util::trace)
//! helper to carry a [`SpanContext`] through `context.message`:
//!
//! - **start node** (`phase: start`, placed right after `listener`, before
//!   `upstream`): extracts the W3C `traceparent` header. If present, this hop
//!   *continues* the incoming trace (same `trace_id`, the caller's span becomes
//!   our parent, and the incoming sampled flag is honored). If absent, a *new*
//!   trace is started (`new_trace_id()`, no parent, sampled per the `sampler`
//!   config). Either way this hop gets a fresh `span_id` and start time, the
//!   span is stored via [`store_span`], and the outgoing `traceparent` header is
//!   injected so the upstream service continues the trace.
//! - **end node** (`phase: end`, placed after `upstream`, right before
//!   `client`): loads the span, computes its duration, and — when sampled —
//!   builds an OTLP/HTTP JSON payload and **fire-and-forgets** a POST to the
//!   collector's `/v1/traces` endpoint on a detached `tokio::spawn` task (the
//!   same best-effort pattern as `proxy-mirror`). The export result is ignored
//!   and never blocks or fails the request.
//!
//! Both nodes always return `Ok`: tracing is observability, so it never
//! short-circuits the request through an error port. If the end node finds no
//! stored span (start node absent or misordered) it passes through untouched.
//!
//! ## Deviations from APISIX
//! - Spans are exported one-per-request (fire-and-forget) rather than through a
//!   batch span processor; `batch_span_processor` config is not supported.
//! - Sampler strategies are `always_on`, `always_off`, and `trace_id_ratio`
//!   (`parent_base` is not supported). The new-trace sampling draw is a
//!   *pseudo-random*, per-trace-consistent hash of the trace id (no `rand`
//!   crate), so a given trace id always samples the same way in a process.
//! - OTLP/JSON id fields (`traceId`, `spanId`, `parentSpanId`) are emitted as
//!   lowercase hex strings, which is the canonical OTLP/JSON encoding accepted
//!   by modern collectors.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{json, Value};

use crate::context::Context;
use crate::outbound::{OutboundClient, OutboundRequest};
use crate::plugins::resources::PluginResources;
use crate::plugins::util::trace::{
    build_traceparent, load_span, new_span_id, new_trace_id, now_ms, parse_traceparent, store_span,
    SpanContext,
};
use crate::plugins::{Plugin, PluginOutput, PluginResult};

/// Which node of the tracing pair this instance is.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Phase {
    /// Before `upstream`: extract/create the span and inject `traceparent`.
    Start,
    /// After `upstream`: export the finished span (fire-and-forget).
    End,
}

/// New-trace sampling strategy (used only when no incoming `traceparent`).
#[derive(Debug, Clone, Copy)]
enum Sampler {
    AlwaysOn,
    AlwaysOff,
    /// Sample a fraction of new traces, in `0.0..=1.0`.
    TraceIdRatio(f64),
}

impl Sampler {
    /// Decides whether a brand-new trace should be sampled.
    ///
    /// `always_on`/`always_off` are unconditional; `trace_id_ratio` compares a
    /// pseudo-random, per-trace-consistent draw derived from `trace_id` against
    /// the fraction (see [`ratio_draw`]).
    fn sample_new(&self, trace_id: &str) -> bool {
        match self {
            Sampler::AlwaysOn => true,
            Sampler::AlwaysOff => false,
            Sampler::TraceIdRatio(f) => ratio_draw(trace_id) < *f,
        }
    }
}

/// A pseudo-random draw in `[0.0, 1.0)` derived deterministically from a trace
/// id, so the same trace id always samples the same way within a process.
/// Not cryptographic; good enough for ratio sampling without a `rand` crate.
fn ratio_draw(trace_id: &str) -> f64 {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut h = DefaultHasher::new();
    trace_id.hash(&mut h);
    h.finish() as f64 / (u64::MAX as f64 + 1.0)
}

/// One node of the OpenTelemetry tracing pair.
pub struct OpenTelemetryPlugin {
    phase: Phase,
    /// OTLP/HTTP collector base URL (no trailing slash); traces POST to
    /// `<collector>/v1/traces`.
    collector: String,
    service_name: String,
    sampler: Sampler,
    ssl_verify: bool,
    timeout: Duration,
    client: Arc<OutboundClient>,
}

/// Parses the `sampler` config object into a [`Sampler`], failing fast on an
/// unknown strategy name or a malformed `fraction`.
fn parse_sampler(config: &HashMap<String, Value>) -> Result<Sampler, String> {
    let Some(s) = config.get("sampler") else {
        return Ok(Sampler::AlwaysOn);
    };
    let name = s
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or("always_on");
    let fraction = s
        .get("options")
        .and_then(|o| o.get("fraction"))
        .map(|f| {
            f.as_f64()
                .filter(|f| (0.0..=1.0).contains(f))
                .ok_or("opentelemetry `sampler.options.fraction` must be a number in 0.0..=1.0")
        })
        .transpose()?
        .unwrap_or(1.0);
    match name {
        "always_on" => Ok(Sampler::AlwaysOn),
        "always_off" => Ok(Sampler::AlwaysOff),
        "trace_id_ratio" => Ok(Sampler::TraceIdRatio(fraction)),
        other => Err(format!(
            "opentelemetry unknown sampler `{other}` (expected always_on, always_off, trace_id_ratio)"
        )),
    }
}

impl OpenTelemetryPlugin {
    /// Builds the plugin from node config.
    ///
    /// Config keys:
    ///
    /// | Key | Type | Default | Description |
    /// |---|---|---|---|
    /// | `phase` | string | — (**required**) | `start` (before upstream) or `end` (after upstream). |
    /// | `endpoint` / `collector` | string | `http://localhost:4318` | OTLP/HTTP collector base URL; traces POST to `<collector>/v1/traces`. |
    /// | `service_name` | string | `"featherbit"` | `service.name` resource attribute reported on each span. |
    /// | `sampler.name` | string | `always_on` | `always_on`, `always_off`, or `trace_id_ratio`. |
    /// | `sampler.options.fraction` | number | `1.0` | Fraction of new traces to sample for `trace_id_ratio`, in `0.0..=1.0`. |
    /// | `ssl_verify` | bool | `true` | Verify the collector's TLS certificate. |
    /// | `timeout` | int (seconds) | `3` | Export request timeout (end node). |
    ///
    /// Fails fast on an unknown `phase`, an unknown sampler name, or a
    /// `fraction` outside `0.0..=1.0`.
    ///
    /// ```yaml
    /// # start node — right after listener, before upstream
    /// - id: otel-start
    ///   type: opentelemetry
    ///   config:
    ///     phase: start
    ///     service_name: my-gateway
    ///     sampler: { name: trace_id_ratio, options: { fraction: 0.1 } }
    /// # end node — after upstream, right before client
    /// - id: otel-end
    ///   type: opentelemetry
    ///   config:
    ///     phase: end
    ///     endpoint: http://localhost:4318
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
                    "opentelemetry `phase` must be `start` or `end` (got `{other}`)"
                ))
            }
            None => return Err("opentelemetry requires `phase` (`start` or `end`)".to_string()),
        };

        let collector = config
            .get("endpoint")
            .or_else(|| config.get("collector"))
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .unwrap_or("http://localhost:4318")
            .trim_end_matches('/')
            .to_string();

        let service_name = config
            .get("service_name")
            .and_then(|v| v.as_str())
            .unwrap_or("featherbit")
            .to_string();

        let sampler = parse_sampler(config)?;

        let ssl_verify = config
            .get("ssl_verify")
            .and_then(|v| v.as_bool())
            .unwrap_or(true);
        let timeout =
            Duration::from_secs(config.get("timeout").and_then(|v| v.as_u64()).unwrap_or(3));

        Ok(Self {
            phase,
            collector,
            service_name,
            sampler,
            ssl_verify,
            timeout,
            client: resources.outbound.clone(),
        })
    }

    /// START: extract or create the span, store it, and inject `traceparent`.
    fn run_start(&self, ctx: &mut Context) {
        let incoming = ctx
            .request
            .headers
            .get("traceparent")
            .and_then(|v| v.first())
            .and_then(|v| parse_traceparent(v));

        let (trace_id, parent_span_id, sampled) = match incoming {
            Some((trace_id, parent, sampled)) => (trace_id, Some(parent), sampled),
            None => {
                let trace_id = new_trace_id();
                let sampled = self.sampler.sample_new(&trace_id);
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

        // Inject the downstream header so the upstream continues the trace.
        ctx.request
            .headers
            .insert("traceparent".to_string(), vec![build_traceparent(&span)]);

        store_span(ctx, &span);
    }

    /// END: load the span and fire-and-forget the OTLP export when sampled.
    fn run_end(&self, ctx: &Context) {
        let Some(span) = load_span(ctx) else {
            return;
        };
        if !span.sampled {
            return;
        }

        let payload = build_otlp(&span, ctx, &self.service_name);
        let body = match serde_json::to_vec(&payload) {
            Ok(b) => b,
            Err(_) => return,
        };

        let req = OutboundRequest {
            method: http::Method::POST,
            url: format!("{}/v1/traces", self.collector),
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

/// Builds the OTLP/HTTP JSON payload for a finished span (pure; no I/O).
///
/// Ids are emitted as lowercase hex strings (the canonical OTLP/JSON encoding).
/// `status.code` is `2` (ERROR) for a 5xx response, else `0` (UNSET).
fn build_otlp(span: &SpanContext, ctx: &Context, service_name: &str) -> Value {
    let start_nanos = span.start_ms.saturating_mul(1_000_000);
    let end_nanos = (span.start_ms + span.duration_ms()).saturating_mul(1_000_000);
    let status = ctx.response.status_code;
    let name = format!("{} {}", ctx.request.method, ctx.request.path);

    let mut span_json = json!({
        "traceId": span.trace_id,
        "spanId": span.span_id,
        "name": name,
        "kind": 2, // SERVER
        "startTimeUnixNano": start_nanos.to_string(),
        "endTimeUnixNano": end_nanos.to_string(),
        "attributes": [
            str_attr("http.method", &ctx.request.method),
            int_attr("http.status_code", status as i64),
            str_attr("http.target", &ctx.request.path),
            str_attr("http.host", &ctx.request.host),
        ],
        "status": { "code": if status >= 500 { 2 } else { 0 } },
    });
    if let Some(parent) = &span.parent_span_id {
        span_json["parentSpanId"] = json!(parent);
    }

    json!({
        "resourceSpans": [{
            "resource": {
                "attributes": [ str_attr("service.name", service_name) ]
            },
            "scopeSpans": [{
                "spans": [ span_json ]
            }]
        }]
    })
}

fn str_attr(key: &str, value: &str) -> Value {
    json!({ "key": key, "value": { "stringValue": value } })
}

fn int_attr(key: &str, value: i64) -> Value {
    json!({ "key": key, "value": { "intValue": value.to_string() } })
}

#[async_trait]
impl Plugin for OpenTelemetryPlugin {
    fn plugin_type(&self) -> &str {
        "opentelemetry"
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

    fn plugin(config: Value) -> Result<OpenTelemetryPlugin, String> {
        let map: HashMap<String, Value> = serde_json::from_value(config).unwrap();
        OpenTelemetryPlugin::from_config(&map, &PluginResources::empty())
    }

    fn test_ctx() -> Context {
        Context {
            request: GatewayRequest {
                method: "GET".to_string(),
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
                status_code: 200,
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
    fn config_requires_valid_phase() {
        assert!(plugin(json!({})).is_err());
        assert!(plugin(json!({ "phase": "middle" })).is_err());
        assert!(plugin(json!({ "phase": "start" })).is_ok());
        assert!(plugin(json!({ "phase": "end" })).is_ok());
    }

    #[test]
    fn config_rejects_bad_sampler() {
        assert!(plugin(json!({ "phase": "start", "sampler": { "name": "bogus" } })).is_err());
        assert!(plugin(json!({
            "phase": "start", "sampler": { "name": "trace_id_ratio", "options": { "fraction": 2 } }
        }))
        .is_err());
        assert!(plugin(json!({
            "phase": "start", "sampler": { "name": "trace_id_ratio", "options": { "fraction": 0.5 } }
        }))
        .is_ok());
    }

    #[test]
    fn sampler_extremes() {
        assert!(!Sampler::AlwaysOff.sample_new("abc"));
        assert!(Sampler::AlwaysOn.sample_new("abc"));
        assert!(!Sampler::TraceIdRatio(0.0).sample_new("abc"));
        assert!(Sampler::TraceIdRatio(1.0).sample_new("abc"));
    }

    #[test]
    fn otlp_payload_shape() {
        let mut ctx = test_ctx();
        ctx.response.status_code = 503;
        let span = a_span(true, Some("0020000000000001"));
        let v = build_otlp(&span, &ctx, "svc");
        let s = &v["resourceSpans"][0]["scopeSpans"][0]["spans"][0];
        assert_eq!(s["traceId"], "0af7651916cd43dd8448eb211c80319c");
        assert_eq!(s["spanId"], "b7ad6b7169203331");
        assert_eq!(s["parentSpanId"], "0020000000000001");
        assert_eq!(s["kind"], 2);
        assert_eq!(s["name"], "GET /api/users");
        // 5xx maps to ERROR (2)
        assert_eq!(s["status"]["code"], 2);
        // service.name resource attribute
        assert_eq!(
            v["resourceSpans"][0]["resource"]["attributes"][0]["value"]["stringValue"],
            "svc"
        );
    }

    #[test]
    fn otlp_omits_parent_when_root() {
        let span = a_span(true, None);
        let v = build_otlp(&span, &test_ctx(), "svc");
        let s = &v["resourceSpans"][0]["scopeSpans"][0]["spans"][0];
        assert!(s.get("parentSpanId").is_none());
        // 2xx maps to UNSET (0)
        assert_eq!(s["status"]["code"], 0);
    }

    #[tokio::test]
    async fn start_creates_new_trace_and_injects_header() {
        let p = plugin(json!({ "phase": "start", "sampler": { "name": "always_on" } })).unwrap();
        let out = p.execute(test_ctx(), &HashMap::new()).await.unwrap();
        // A span was stored...
        let span = load_span(&out.context).expect("span stored");
        assert!(span.parent_span_id.is_none());
        assert!(span.sampled);
        assert_eq!(span.trace_id.len(), 32);
        // ...and the downstream traceparent was injected, matching the span.
        let hdr = &out.context.request.headers["traceparent"][0];
        assert_eq!(*hdr, build_traceparent(&span));
    }

    #[tokio::test]
    async fn start_continues_incoming_trace() {
        let p = plugin(json!({ "phase": "start" })).unwrap();
        let mut ctx = test_ctx();
        ctx.request.headers.insert(
            "traceparent".to_string(),
            vec!["00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01".to_string()],
        );
        let out = p.execute(ctx, &HashMap::new()).await.unwrap();
        let span = load_span(&out.context).unwrap();
        assert_eq!(span.trace_id, "0af7651916cd43dd8448eb211c80319c");
        assert_eq!(span.parent_span_id.as_deref(), Some("b7ad6b7169203331"));
        assert!(span.sampled);
        // Our own span id replaces the caller's in the injected header.
        assert_ne!(span.span_id, "b7ad6b7169203331");
    }

    #[tokio::test]
    async fn end_passes_through_without_span() {
        let p = plugin(json!({ "phase": "end" })).unwrap();
        let out = p.execute(test_ctx(), &HashMap::new()).await.unwrap();
        assert_eq!(out.context.response.status_code, 200);
    }

    #[tokio::test]
    async fn end_returns_ok_with_stored_span() {
        let p = plugin(json!({ "phase": "end", "endpoint": "http://127.0.0.1:1" })).unwrap();
        let mut ctx = test_ctx();
        store_span(&mut ctx, &a_span(true, None));
        // The spawned export is best-effort; we only assert execute returns Ok.
        let out = p.execute(ctx, &HashMap::new()).await.unwrap();
        assert_eq!(out.context.request.path, "/api/users");
    }

    #[tokio::test]
    async fn end_skips_export_when_unsampled() {
        let p = plugin(json!({ "phase": "end" })).unwrap();
        let mut ctx = test_ctx();
        store_span(&mut ctx, &a_span(false, None));
        let out = p.execute(ctx, &HashMap::new()).await.unwrap();
        assert_eq!(out.context.response.status_code, 200);
    }
}
