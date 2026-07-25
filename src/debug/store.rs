//! Debug settings and the bounded in-memory trace buffer.
//!
//! One [`DebugState`] is built at startup from `system.yaml` and hung off
//! `SharedState`. It answers "should this request be traced?" on the hot path
//! and owns the ring buffer that the Admin API reads.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use serde::Serialize;

use crate::config::DebugConfig;

use super::trace::{CaptureOptions, RedactionPolicy, Trace, TraceSource};

/// Resolved debug settings plus the trace buffer.
///
/// Every field is immutable after construction — `system.yaml` is not
/// hot-reloaded, so debug mode cannot be switched on in a running process.
/// That is deliberate: it means a compromised Admin API credential cannot
/// start capturing request contexts.
pub struct DebugState {
    pub enabled: bool,
    pub sandbox_enabled: bool,
    /// Lowercased, because `GatewayRequest` stores header names as hyper
    /// normalised them.
    pub trigger_header: String,
    pub trace_all: bool,
    pub capture_bodies: bool,
    pub max_body_bytes: usize,
    pub max_traces: usize,
    pub max_steps: usize,
    pub sandbox_timeout_seconds: u64,
    redaction: RedactionPolicy,
    // `std::sync::Mutex`, not tokio's: the critical section is a `push_back`
    // plus at most one `pop_front` and is never held across an `.await`, so the
    // std mutex is both correct and cheaper. Clippy's `await_holding_lock`
    // keeps it that way.
    traces: Mutex<VecDeque<Arc<Trace>>>,
    seq: AtomicU64,
}

/// Row shape for the trace list view.
#[derive(Debug, Clone, Serialize)]
pub struct TraceSummary {
    pub id: String,
    pub seq: u64,
    pub source: TraceSource,
    pub started_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub route: Option<String>,
    pub policy: String,
    pub method: String,
    pub path: String,
    pub status: u16,
    pub duration_us: u64,
    pub step_count: usize,
    pub error_count: usize,
    pub captured_bodies: bool,
}

impl DebugState {
    /// Builds the runtime state from config.
    pub fn new(cfg: &DebugConfig) -> Self {
        Self {
            enabled: cfg.enabled,
            sandbox_enabled: cfg.sandbox,
            trigger_header: cfg.trigger_header.to_lowercase(),
            trace_all: cfg.trace_all,
            capture_bodies: cfg.capture_bodies,
            max_body_bytes: cfg.max_body_bytes,
            max_traces: cfg.max_traces,
            max_steps: cfg.max_steps,
            sandbox_timeout_seconds: cfg.sandbox_timeout_seconds,
            redaction: RedactionPolicy::new(
                &cfg.redact_headers,
                &cfg.redact_query_params,
                &cfg.redact_message_keys,
            ),
            traces: Mutex::new(VecDeque::new()),
            seq: AtomicU64::new(0),
        }
    }

    /// Capture knobs for a snapshot.
    pub fn capture_options(&self) -> CaptureOptions {
        CaptureOptions {
            capture_bodies: self.capture_bodies,
            max_body_bytes: self.max_body_bytes,
            redaction: self.redaction.clone(),
        }
    }

    /// Whether this request should be traced.
    ///
    /// The whole cost on an untraced request: one `HashMap::get` against the
    /// already-built header map, and only when debug is enabled at all.
    pub fn should_trace(&self, headers: &HashMap<String, Vec<String>>) -> bool {
        if !self.enabled {
            return false;
        }
        self.trace_all || headers.contains_key(&self.trigger_header)
    }

    /// Whether the request explicitly asked to be traced via the trigger
    /// header, as opposed to being swept up by `trace_all`.
    ///
    /// The `x-featherbit-trace-id` response header is returned only in this
    /// case: the id is a reply to a caller who opted in. Under `trace_all` the
    /// traffic is anonymous, so stamping every response with a debug id would
    /// leak it to clients that never asked — those traces are found in the
    /// panel/list instead.
    pub fn header_opt_in(&self, headers: &HashMap<String, Vec<String>>) -> bool {
        self.enabled && headers.contains_key(&self.trigger_header)
    }

    /// Next monotonic sequence number.
    pub fn next_seq(&self) -> u64 {
        self.seq.fetch_add(1, Ordering::Relaxed)
    }

    /// Stores a trace, evicting the oldest entries beyond `max_traces`.
    pub fn record(&self, trace: Trace) {
        if self.max_traces == 0 {
            return;
        }
        let mut buf = self.traces.lock().unwrap_or_else(|e| e.into_inner());
        while buf.len() >= self.max_traces {
            buf.pop_front();
        }
        buf.push_back(Arc::new(trace));
    }

    /// Summaries, newest first.
    pub fn list(&self) -> Vec<TraceSummary> {
        let buf = self.traces.lock().unwrap_or_else(|e| e.into_inner());
        buf.iter()
            .rev()
            .map(|t| TraceSummary {
                id: t.id.clone(),
                seq: t.seq,
                source: t.source,
                started_ms: t.started_ms,
                route: t.route.clone(),
                policy: t.policy.clone(),
                method: t.method.clone(),
                path: t.path.clone(),
                status: t.status,
                duration_us: t.duration_us,
                step_count: t.steps.len(),
                error_count: t.steps.last().map(|s| s.after.errors.len()).unwrap_or(0),
                captured_bodies: t.captured_bodies,
            })
            .collect()
    }

    /// Fetches one trace. Clones an `Arc` under the lock so serialization
    /// happens outside it.
    pub fn get(&self, id: &str) -> Option<Arc<Trace>> {
        let buf = self.traces.lock().unwrap_or_else(|e| e.into_inner());
        buf.iter().find(|t| t.id == id).cloned()
    }

    /// Empties the buffer, returning how many traces were dropped.
    pub fn clear(&self) -> usize {
        let mut buf = self.traces.lock().unwrap_or_else(|e| e.into_inner());
        let n = buf.len();
        buf.clear();
        n
    }

    /// Number of traces currently held.
    pub fn len(&self) -> usize {
        self.traces.lock().unwrap_or_else(|e| e.into_inner()).len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::{Context, GatewayRequest, GatewayResponse, Protocol};
    use crate::debug::trace::{ContextSnapshot, PreviousBodies};
    use bytes::Bytes;

    fn ctx() -> Context {
        Context {
            request: GatewayRequest {
                method: "GET".to_string(),
                path: "/x".to_string(),
                host: "h".to_string(),
                scheme: "http".to_string(),
                headers: HashMap::new(),
                query_params: HashMap::new(),
                body: Bytes::new(),
                remote_addr: "1.2.3.4:5".to_string(),
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

    fn trace(state: &DebugState, id: &str) -> Trace {
        Trace {
            id: id.to_string(),
            seq: state.next_seq(),
            source: TraceSource::Request,
            started_ms: 0,
            route: Some("r".to_string()),
            policy: "p".to_string(),
            method: "GET".to_string(),
            path: "/x".to_string(),
            status: 200,
            duration_us: 1,
            captured_bodies: false,
            initial: ContextSnapshot::capture(
                &ctx(),
                &CaptureOptions::default(),
                &PreviousBodies::default(),
            ),
            steps: Vec::new(),
            notes: Vec::new(),
        }
    }

    fn enabled_state(max_traces: usize) -> DebugState {
        DebugState::new(&DebugConfig {
            enabled: true,
            max_traces,
            ..Default::default()
        })
    }

    #[test]
    fn test_should_trace_requires_enabled() {
        let off = DebugState::new(&DebugConfig::default());
        let mut headers = HashMap::new();
        headers.insert("x-featherbit-debug".to_string(), vec!["1".to_string()]);
        // The header alone must never be enough.
        assert!(!off.should_trace(&headers));
    }

    #[test]
    fn test_should_trace_on_trigger_header() {
        let s = enabled_state(10);
        let mut headers = HashMap::new();
        assert!(
            !s.should_trace(&headers),
            "untriggered request is not traced"
        );
        headers.insert("x-featherbit-debug".to_string(), vec!["1".to_string()]);
        assert!(s.should_trace(&headers));
    }

    /// Presence is the trigger, not truthiness -- `: 0` still traces, which is
    /// what a developer poking at it expects.
    #[test]
    fn test_trigger_is_presence_not_value() {
        let s = enabled_state(10);
        let mut headers = HashMap::new();
        headers.insert("x-featherbit-debug".to_string(), vec!["0".to_string()]);
        assert!(s.should_trace(&headers));
    }

    #[test]
    fn test_trace_all_ignores_header() {
        let s = DebugState::new(&DebugConfig {
            enabled: true,
            trace_all: true,
            ..Default::default()
        });
        assert!(s.should_trace(&HashMap::new()));
    }

    /// The response trace-id header is only for callers who opted in — a
    /// trace_all-captured request is traced but not `header_opt_in`, so its
    /// response is not stamped with a debug id it never asked for.
    #[test]
    fn test_header_opt_in_distinguishes_from_trace_all() {
        let s = DebugState::new(&DebugConfig {
            enabled: true,
            trace_all: true,
            ..Default::default()
        });
        let empty = HashMap::new();
        // trace_all sweeps it up...
        assert!(s.should_trace(&empty));
        // ...but it did not opt in, so no trace-id header.
        assert!(!s.header_opt_in(&empty));

        let mut with_header = HashMap::new();
        with_header.insert("x-featherbit-debug".to_string(), vec!["1".to_string()]);
        assert!(s.header_opt_in(&with_header));
    }

    #[test]
    fn test_custom_trigger_header_is_lowercased() {
        let s = DebugState::new(&DebugConfig {
            enabled: true,
            trigger_header: "X-My-Debug".to_string(),
            ..Default::default()
        });
        assert_eq!(s.trigger_header, "x-my-debug");
        let mut headers = HashMap::new();
        headers.insert("x-my-debug".to_string(), vec!["1".to_string()]);
        assert!(s.should_trace(&headers));
    }

    #[test]
    fn test_ring_buffer_evicts_oldest() {
        let s = enabled_state(3);
        for i in 0..8 {
            s.record(trace(&s, &format!("t{i}")));
        }
        assert_eq!(s.len(), 3);
        // Oldest are gone, newest retained.
        assert!(s.get("t0").is_none());
        assert!(s.get("t4").is_none());
        assert!(s.get("t7").is_some());
        // Listing is newest-first.
        let ids: Vec<String> = s.list().into_iter().map(|t| t.id).collect();
        assert_eq!(ids, vec!["t7", "t6", "t5"]);
    }

    #[test]
    fn test_clear_reports_count() {
        let s = enabled_state(10);
        s.record(trace(&s, "a"));
        s.record(trace(&s, "b"));
        assert_eq!(s.clear(), 2);
        assert_eq!(s.len(), 0);
        assert!(s.get("a").is_none());
    }

    #[test]
    fn test_zero_capacity_disables_storage_without_panicking() {
        let s = enabled_state(0);
        s.record(trace(&s, "a"));
        assert_eq!(s.len(), 0);
        assert!(s.list().is_empty());
    }

    #[test]
    fn test_seq_is_monotonic() {
        let s = enabled_state(10);
        assert_eq!(s.next_seq(), 0);
        assert_eq!(s.next_seq(), 1);
        assert_eq!(s.next_seq(), 2);
    }
}
