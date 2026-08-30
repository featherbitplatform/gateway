//! Trace presentation shared by the Admin API and the MCP tools: list
//! filtering and the rendered trace shape (each step with its `changes`).

use serde::{Deserialize, Serialize};

use crate::debug::diff::{diff, Change};
use crate::debug::store::TraceSummary;
use crate::debug::{NodeStep, Trace};

/// Optional filters for the trace list. All are ANDed; blank strings are
/// ignored (a bare `?route=` is not a filter).
#[derive(Debug, Default, Deserialize)]
pub struct TraceFilter {
    pub route: Option<String>,
    pub policy: Option<String>,
    pub status: Option<u16>,
    /// `request` or `sandbox` (case-insensitive).
    pub source: Option<String>,
    /// Cap on the number of rows returned, applied after filtering.
    pub limit: Option<usize>,
}

fn non_empty(s: &Option<String>) -> Option<&str> {
    s.as_deref().map(str::trim).filter(|s| !s.is_empty())
}

fn source_str(source: crate::debug::TraceSource) -> &'static str {
    match source {
        crate::debug::TraceSource::Request => "request",
        crate::debug::TraceSource::Sandbox => "sandbox",
    }
}

/// Applies `f` to a newest-first summary list.
pub fn apply_filter(mut traces: Vec<TraceSummary>, f: &TraceFilter) -> Vec<TraceSummary> {
    if let Some(route) = non_empty(&f.route) {
        traces.retain(|t| t.route.as_deref() == Some(route));
    }
    if let Some(policy) = non_empty(&f.policy) {
        traces.retain(|t| t.policy == policy);
    }
    if let Some(status) = f.status {
        traces.retain(|t| t.status == status);
    }
    if let Some(source) = non_empty(&f.source) {
        traces.retain(|t| source_str(t.source).eq_ignore_ascii_case(source));
    }
    if let Some(limit) = f.limit {
        traces.truncate(limit);
    }
    traces
}

/// A step plus the changes derived from the preceding snapshot.
#[derive(Serialize)]
struct StepWithChanges<'a> {
    #[serde(flatten)]
    step: &'a NodeStep,
    changes: Vec<Change>,
}

/// Renders a trace with per-step `changes` computed at read time.
///
/// The diff is derived here rather than stored because the context flows
/// linearly: `before(step N) == after(step N-1)`, so the snapshots already hold
/// everything needed.
pub fn render_trace(trace: &Trace) -> serde_json::Value {
    let mut prev = &trace.initial;
    let mut steps = Vec::with_capacity(trace.steps.len());
    for step in &trace.steps {
        steps.push(StepWithChanges {
            step,
            changes: diff(prev, &step.after),
        });
        prev = &step.after;
    }
    let mut out = serde_json::to_value(trace).unwrap_or_else(|_| serde_json::json!({}));
    out["steps"] = serde_json::to_value(steps).unwrap_or_else(|_| serde_json::json!([]));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::debug::{StepOutcome, TraceSource};

    fn summary(policy: &str, status: u16, source: TraceSource) -> TraceSummary {
        TraceSummary {
            id: format!("{policy}-{status}"),
            seq: 1,
            source,
            started_ms: 0,
            route: Some("r".into()),
            policy: policy.into(),
            method: "GET".into(),
            path: "/".into(),
            status,
            duration_us: 1,
            step_count: 1,
            error_count: 0,
            captured_bodies: false,
        }
    }

    #[test]
    fn filter_ands_fields_and_ignores_blank_strings() {
        let all = vec![
            summary("a", 200, TraceSource::Request),
            summary("a", 500, TraceSource::Sandbox),
            summary("b", 200, TraceSource::Request),
        ];
        let f = TraceFilter {
            policy: Some("a".into()),
            status: Some(200),
            ..Default::default()
        };
        assert_eq!(apply_filter(all.clone(), &f).len(), 1);
        let f = TraceFilter {
            source: Some("SANDBOX".into()),
            ..Default::default()
        };
        assert_eq!(apply_filter(all.clone(), &f)[0].id, "a-500");
        let f = TraceFilter {
            route: Some("  ".into()),
            limit: Some(2),
            ..Default::default()
        };
        assert_eq!(apply_filter(all, &f).len(), 2);
    }

    #[test]
    fn render_attaches_changes_per_step() {
        let ctx = crate::debug::sandbox::SandboxContextInput::default()
            .into_context()
            .unwrap();
        let opts = crate::debug::CaptureOptions::default();
        let mut rec = crate::debug::TraceRecorder::new(&ctx, opts, 10);
        let mut after = crate::debug::sandbox::SandboxContextInput::default()
            .into_context()
            .unwrap();
        after.response.status_code = 418;
        rec.record_step(
            "n1",
            "echo",
            StepOutcome::Success,
            std::time::Duration::from_micros(5),
            crate::debug::EdgeKind::Success,
            Some("success"),
            None,
            &after,
        );
        let trace = rec.finish(
            "t1".into(),
            1,
            TraceSource::Request,
            None,
            "p".into(),
            &after,
            std::time::Duration::from_micros(7),
        );
        let v = render_trace(&trace);
        let steps = v["steps"].as_array().unwrap();
        assert_eq!(steps.len(), 1);
        let changes = steps[0]["changes"].as_array().unwrap();
        assert!(
            changes.iter().any(|c| c["path"] == "response.status_code"),
            "{changes:?}"
        );
        assert_eq!(steps[0]["node_id"], "n1");
    }
}
