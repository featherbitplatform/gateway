//! Distributed-tracing span context and propagation codecs.
//!
//! Shared by the tracing plugins (`opentelemetry`, `zipkin`, `skywalking`),
//! which are each a **start/end node pair** around `upstream`: the start node
//! extracts or creates a [`SpanContext`], stores it in `context.message`
//! (per-request state), and injects the downstream propagation header so the
//! upstream service continues the trace; the end node loads the span, computes
//! its duration, and exports it to the collector (fire-and-forget).
//!
//! Provides trace/span id generation and codecs for the three wire formats:
//! W3C `traceparent` (OpenTelemetry), B3 (Zipkin), and SkyWalking `sw8`.

use serde::{Deserialize, Serialize};
use std::time::{SystemTime, UNIX_EPOCH};

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use ring::rand::{SecureRandom, SystemRandom};

use crate::context::Context;

/// The reserved `context.message` key under which the active span is stored.
pub const SPAN_KEY: &str = "__trace_span";

/// A span in flight: the trace it belongs to, this hop's span, its parent, the
/// sampling decision, and when it started (unix millis).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpanContext {
    /// 32-hex-char trace id, shared across the whole distributed trace.
    pub trace_id: String,
    /// 16-hex-char span id for this gateway hop.
    pub span_id: String,
    /// The caller's span id, when the request arrived already traced.
    pub parent_span_id: Option<String>,
    /// Whether this trace is sampled (exported).
    pub sampled: bool,
    /// Span start time (unix millis).
    pub start_ms: u64,
}

impl SpanContext {
    /// Milliseconds elapsed since the span started.
    pub fn duration_ms(&self) -> u64 {
        now_ms().saturating_sub(self.start_ms)
    }
}

/// Stores the span in `context.message`.
pub fn store_span(ctx: &mut Context, span: &SpanContext) {
    if let Ok(v) = serde_json::to_value(span) {
        ctx.message.insert(SPAN_KEY.to_string(), v);
    }
}

/// Loads the span previously stored by the start node.
pub fn load_span(ctx: &Context) -> Option<SpanContext> {
    serde_json::from_value(ctx.message.get(SPAN_KEY)?.clone()).ok()
}

/// A random 32-hex-char (128-bit) trace id.
pub fn new_trace_id() -> String {
    let mut b = [0u8; 16];
    SystemRandom::new().fill(&mut b).expect("system RNG");
    hex(&b)
}

/// A random 16-hex-char (64-bit) span id.
pub fn new_span_id() -> String {
    let mut b = [0u8; 8];
    SystemRandom::new().fill(&mut b).expect("system RNG");
    hex(&b)
}

/// Current unix time in milliseconds.
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

// ---- W3C traceparent (OpenTelemetry) --------------------------------------

/// Parses a W3C `traceparent` header (`00-<trace>-<parent>-<flags>`) into
/// `(trace_id, parent_span_id, sampled)`.
pub fn parse_traceparent(value: &str) -> Option<(String, String, bool)> {
    let parts: Vec<&str> = value.trim().split('-').collect();
    if parts.len() != 4 {
        return None;
    }
    let trace_id = parts[1];
    let parent = parts[2];
    if trace_id.len() != 32 || parent.len() != 16 {
        return None;
    }
    if trace_id.bytes().all(|b| b == b'0') || parent.bytes().all(|b| b == b'0') {
        return None;
    }
    let sampled = u8::from_str_radix(parts[3], 16)
        .map(|f| f & 1 == 1)
        .unwrap_or(false);
    Some((trace_id.to_string(), parent.to_string(), sampled))
}

/// Builds a W3C `traceparent` header value for a span.
pub fn build_traceparent(span: &SpanContext) -> String {
    format!(
        "00-{}-{}-{:02x}",
        span.trace_id,
        span.span_id,
        if span.sampled { 1 } else { 0 }
    )
}

// ---- B3 (Zipkin) -----------------------------------------------------------

/// Parses B3 propagation from either the single `b3` header
/// (`trace-span-sampled-parent`) or the multi-header form, given a lookup of
/// the relevant request headers, into `(trace_id, parent_span_id, sampled)`.
pub fn parse_b3(
    b3_single: Option<&str>,
    trace_id: Option<&str>,
    span_id: Option<&str>,
    sampled: Option<&str>,
) -> Option<(String, String, bool)> {
    if let Some(single) = b3_single {
        let parts: Vec<&str> = single.trim().split('-').collect();
        if parts.len() >= 2 && !parts[0].is_empty() {
            let s = parts.get(2).map(|v| *v == "1" || *v == "d").unwrap_or(true);
            return Some((parts[0].to_string(), parts[1].to_string(), s));
        }
        return None;
    }
    let t = trace_id?;
    let s = span_id?;
    if t.is_empty() || s.is_empty() {
        return None;
    }
    let sampled = sampled.map(|v| v == "1" || v == "true").unwrap_or(true);
    Some((t.to_string(), s.to_string(), sampled))
}

/// Builds the B3 multi-headers for a span as `(name, value)` pairs to set on
/// the outgoing request.
pub fn build_b3_headers(span: &SpanContext) -> Vec<(String, String)> {
    let mut h = vec![
        ("x-b3-traceid".to_string(), span.trace_id.clone()),
        ("x-b3-spanid".to_string(), span.span_id.clone()),
        (
            "x-b3-sampled".to_string(),
            if span.sampled { "1" } else { "0" }.to_string(),
        ),
    ];
    if let Some(parent) = &span.parent_span_id {
        h.push(("x-b3-parentspanid".to_string(), parent.clone()));
    }
    h
}

// ---- SkyWalking sw8 --------------------------------------------------------

/// Parses a SkyWalking `sw8` header (hyphen-separated, base64 fields) into
/// `(trace_id, parent_segment_span_id, sampled)`. Field 0 is the sample flag,
/// field 1 the base64 trace id, field 3 the parent span id.
pub fn parse_sw8(value: &str) -> Option<(String, String, bool)> {
    let parts: Vec<&str> = value.trim().split('-').collect();
    if parts.len() < 8 {
        return None;
    }
    let sampled = parts[0] == "1";
    let trace_id = String::from_utf8(BASE64.decode(parts[1]).ok()?).ok()?;
    // Field 3 is the parent span id — a plain integer, not a base64 field
    // (only the trace/segment ids and the service/instance/endpoint names are
    // base64-encoded in sw8).
    let parent_span = parts[3].to_string();
    Some((trace_id, parent_span, sampled))
}

/// Base64-encodes a field for an sw8 header.
pub fn sw8_encode(field: &str) -> String {
    BASE64.encode(field.as_bytes())
}

fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{:02x}", b));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn span() -> SpanContext {
        SpanContext {
            trace_id: "0af7651916cd43dd8448eb211c80319c".to_string(),
            span_id: "b7ad6b7169203331".to_string(),
            parent_span_id: Some("0020000000000001".to_string()),
            sampled: true,
            start_ms: now_ms(),
        }
    }

    #[test]
    fn test_id_lengths() {
        assert_eq!(new_trace_id().len(), 32);
        assert_eq!(new_span_id().len(), 16);
        // hex and non-zero-ish
        assert!(new_trace_id().chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn test_traceparent_round_trip() {
        let s = span();
        let header = build_traceparent(&s);
        assert_eq!(
            header,
            "00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01"
        );
        let (trace, parent, sampled) = parse_traceparent(&header).unwrap();
        assert_eq!(trace, s.trace_id);
        assert_eq!(parent, s.span_id);
        assert!(sampled);

        // All-zero trace id is invalid.
        assert!(
            parse_traceparent("00-00000000000000000000000000000000-b7ad6b7169203331-01").is_none()
        );
        // Wrong shape.
        assert!(parse_traceparent("garbage").is_none());
    }

    #[test]
    fn test_b3() {
        let s = span();
        let headers = build_b3_headers(&s);
        assert!(headers
            .iter()
            .any(|(k, v)| k == "x-b3-traceid" && v == &s.trace_id));
        assert!(headers.iter().any(|(k, v)| k == "x-b3-sampled" && v == "1"));

        // single-header form
        let (t, sp, sampled) = parse_b3(Some("abc-def-1-ghi"), None, None, None).unwrap();
        assert_eq!(t, "abc");
        assert_eq!(sp, "def");
        assert!(sampled);
        // multi-header form
        let (t, sp, sampled) = parse_b3(None, Some("t1"), Some("s1"), Some("0")).unwrap();
        assert_eq!((t.as_str(), sp.as_str(), sampled), ("t1", "s1", false));
        // missing → None
        assert!(parse_b3(None, None, Some("s"), None).is_none());
    }

    #[test]
    fn test_sw8_round_trip() {
        let trace = "1f2d4bf47bf711eab794acde48001122";
        let header = format!(
            "1-{}-{}-3-5-2-{}-{}-{}",
            sw8_encode(trace),
            sw8_encode("segment-id"),
            sw8_encode("service"),
            sw8_encode("instance"),
            sw8_encode("/endpoint")
        );
        let (t, parent_span, sampled) = parse_sw8(&header).unwrap();
        assert_eq!(t, trace);
        assert_eq!(parent_span, "3");
        assert!(sampled);
        assert!(parse_sw8("too-short").is_none());
    }

    #[test]
    fn test_store_load_span() {
        use crate::context::{GatewayRequest, GatewayResponse, Protocol};
        use bytes::Bytes;
        use std::collections::HashMap;
        let mut ctx = Context {
            request: GatewayRequest {
                method: "GET".into(),
                path: "/".into(),
                host: "h".into(),
                scheme: "http".into(),
                headers: HashMap::new(),
                query_params: HashMap::new(),
                body: Bytes::new(),
                remote_addr: "1.2.3.4:5".into(),
                protocol: Protocol::Http1,
            },
            response: GatewayResponse {
                status_code: 0,
                headers: HashMap::new(),
                body: Bytes::new(),
            },
            message: HashMap::new(),
            errors: Vec::new(),
        };
        assert!(load_span(&ctx).is_none());
        let s = span();
        store_span(&mut ctx, &s);
        let back = load_span(&ctx).unwrap();
        assert_eq!(back.trace_id, s.trace_id);
        assert_eq!(back.span_id, s.span_id);
    }
}
