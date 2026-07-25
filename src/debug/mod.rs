//! Debug mode: per-request policy-execution tracing and the plugin sandbox.
//!
//! Policy development is otherwise blind — you can see the final response and
//! aggregate metrics, but not the [`Context`](crate::context::Context) as it
//! moves from node to node. This module records that walk.
//!
//! - [`trace`] — the [`Trace`]/[`NodeStep`] records and redacted snapshots.
//! - [`diff`] — derives "what this plugin changed" from two snapshots.
//! - [`store`] — resolved settings plus the bounded ring buffer.
//! - [`sandbox`] — runs plugins or a named policy against a synthetic context.
//!
//! Tracing is opt-in per request (a trigger header) and entirely off unless
//! `debug.enabled` is set in `system.yaml`. When it is off the only cost on the
//! request path is one `Option` check per node in the engine loop.

pub mod diff;
pub mod sandbox;
pub mod store;
pub mod trace;

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::context::Context;

// featherbit is a binary crate, so `pub` exports nothing externally; re-exports
// consumed only by later increments read as unused in an intermediate build.
#[allow(unused_imports)]
pub use store::{DebugState, TraceSummary};
pub use trace::{
    CaptureOptions, ContextSnapshot, EdgeKind, NodeStep, PreviousBodies, StepOutcome, Trace,
    TraceSource,
};

/// Accumulates steps while the engine walks a graph.
///
/// Created per traced execution and handed to
/// [`CompiledGraph::execute_traced`](crate::graph::CompiledGraph::execute_traced);
/// the engine calls [`record_step`](TraceRecorder::record_step) after each node
/// and the caller then [`finish`](TraceRecorder::finish)es it into a [`Trace`].
pub struct TraceRecorder {
    opts: CaptureOptions,
    max_steps: usize,
    /// Bodies from the previous step, so an unchanged body is not stored once
    /// per node. Cloning `Bytes` is a refcount bump, not a copy.
    prev_bodies: PreviousBodies,
    initial: Option<ContextSnapshot>,
    steps: Vec<NodeStep>,
    notes: Vec<String>,
    truncated: bool,
}

impl TraceRecorder {
    /// Starts a recording, capturing the context as it enters the graph.
    pub fn new(ctx: &Context, opts: CaptureOptions, max_steps: usize) -> Self {
        let initial = ContextSnapshot::capture(ctx, &opts, &PreviousBodies::default());
        Self {
            opts,
            max_steps,
            prev_bodies: PreviousBodies {
                request: Some(ctx.request.body.clone()),
                response: Some(ctx.response.body.clone()),
            },
            initial: Some(initial),
            steps: Vec::new(),
            notes: Vec::new(),
            truncated: false,
        }
    }

    /// Records one node execution. Called by the engine immediately after the
    /// plugin returns, while the context is still in hand.
    // Each parameter is a distinct fact the engine already has in hand;
    // bundling them into a struct would only move the noise to the call site.
    #[allow(clippy::too_many_arguments)]
    pub fn record_step(
        &mut self,
        node_id: &str,
        node_type: &str,
        outcome: StepOutcome,
        duration: Duration,
        edge: EdgeKind,
        next_node_id: Option<&str>,
        ctx: &Context,
    ) {
        if self.steps.len() >= self.max_steps {
            if !self.truncated {
                self.truncated = true;
                self.notes.push(format!(
                    "step limit of {} reached; later nodes are not recorded",
                    self.max_steps
                ));
            }
            return;
        }

        let after = ContextSnapshot::capture(ctx, &self.opts, &self.prev_bodies);
        self.prev_bodies = PreviousBodies {
            request: Some(ctx.request.body.clone()),
            response: Some(ctx.response.body.clone()),
        };
        if after.request.body.truncated || after.response.body.truncated {
            let note = "a captured body was truncated".to_string();
            if !self.notes.contains(&note) {
                self.notes.push(note);
            }
        }

        self.steps.push(NodeStep {
            index: self.steps.len(),
            node_id: node_id.to_string(),
            node_type: node_type.to_string(),
            outcome,
            duration_us: duration.as_micros() as u64,
            edge,
            next_node_id: next_node_id.map(str::to_string),
            after,
        });
    }

    /// Seals the recording into a [`Trace`].
    #[allow(clippy::too_many_arguments)]
    pub fn finish(
        mut self,
        id: String,
        seq: u64,
        source: TraceSource,
        route: Option<String>,
        policy: String,
        final_ctx: &Context,
        duration: Duration,
    ) -> Trace {
        Trace {
            id,
            seq,
            source,
            started_ms: now_ms(),
            route,
            policy,
            method: final_ctx.request.method.clone(),
            path: final_ctx.request.path.clone(),
            status: final_ctx.response.status_code,
            duration_us: duration.as_micros() as u64,
            captured_bodies: self.opts.capture_bodies,
            initial: self.initial.take().expect("initial snapshot taken once"),
            steps: std::mem::take(&mut self.steps),
            notes: std::mem::take(&mut self.notes),
        }
    }
}

/// Unix milliseconds, saturating to 0 if the clock is before the epoch.
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// A short, unique trace id.
pub fn new_trace_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::{GatewayRequest, GatewayResponse, Protocol};
    use bytes::Bytes;
    use std::collections::HashMap;

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
                status_code: 0,
                headers: HashMap::new(),
                body: Bytes::new(),
            },
            message: HashMap::new(),
            errors: Vec::new(),
        }
    }

    /// Records `n` successful steps against `c` — the same context the engine
    /// would hand back, so body dedup sees the real sequence.
    fn record_n_with(rec: &mut TraceRecorder, c: &Context, n: usize) {
        for i in 0..n {
            rec.record_step(
                &format!("n{i}"),
                "cors",
                StepOutcome::Success,
                Duration::from_micros(5),
                EdgeKind::Success,
                None,
                c,
            );
        }
    }

    fn record_n(rec: &mut TraceRecorder, n: usize) {
        record_n_with(rec, &ctx(), n);
    }

    #[test]
    fn test_steps_are_indexed_in_order() {
        let c = ctx();
        let mut rec = TraceRecorder::new(&c, CaptureOptions::default(), 100);
        record_n(&mut rec, 3);
        let t = rec.finish(
            "id".to_string(),
            0,
            TraceSource::Request,
            Some("r".to_string()),
            "p".to_string(),
            &c,
            Duration::from_millis(1),
        );
        assert_eq!(t.steps.len(), 3);
        let ids: Vec<&str> = t.steps.iter().map(|s| s.node_id.as_str()).collect();
        assert_eq!(ids, vec!["n0", "n1", "n2"]);
        assert_eq!(t.steps[2].index, 2);
        assert!(t.notes.is_empty());
    }

    #[test]
    fn test_step_limit_truncates_with_one_note() {
        let c = ctx();
        let mut rec = TraceRecorder::new(&c, CaptureOptions::default(), 2);
        record_n(&mut rec, 6);
        let t = rec.finish(
            "id".to_string(),
            0,
            TraceSource::Request,
            None,
            "p".to_string(),
            &c,
            Duration::from_millis(1),
        );
        assert_eq!(t.steps.len(), 2);
        // The note is added once, not once per dropped step.
        assert_eq!(t.notes.len(), 1);
        assert!(t.notes[0].contains("step limit"));
    }

    /// The recorder must carry the body forward so consecutive identical
    /// bodies are not stored repeatedly.
    #[test]
    fn test_body_dedup_across_steps() {
        let mut c = ctx();
        c.request.body = Bytes::from_static(b"payload");
        let opts = CaptureOptions {
            capture_bodies: true,
            ..Default::default()
        };
        let mut rec = TraceRecorder::new(&c, opts, 100);
        // The same body flows through both nodes, as it would when no plugin
        // touches it.
        record_n_with(&mut rec, &c, 2);
        let t = rec.finish(
            "id".to_string(),
            0,
            TraceSource::Request,
            None,
            "p".to_string(),
            &c,
            Duration::from_millis(1),
        );
        // Captured once at the start...
        assert_eq!(t.initial.request.body.text.as_deref(), Some("payload"));
        // ...then marked unchanged rather than repeated per node.
        assert!(t.steps[0].after.request.body.unchanged);
        assert!(t.steps[1].after.request.body.unchanged);
        assert_eq!(t.steps[0].after.request.body.len, 7);
    }

    #[test]
    fn test_finish_reports_final_status() {
        let c = ctx();
        let mut end = ctx();
        end.response.status_code = 403;
        let mut rec = TraceRecorder::new(&c, CaptureOptions::default(), 10);
        record_n(&mut rec, 1);
        let t = rec.finish(
            "id".to_string(),
            7,
            TraceSource::Sandbox,
            None,
            "p".to_string(),
            &end,
            Duration::from_micros(250),
        );
        assert_eq!(t.status, 403);
        assert_eq!(t.seq, 7);
        assert_eq!(t.source, TraceSource::Sandbox);
        assert_eq!(t.duration_us, 250);
        assert!(t.route.is_none());
    }
}
