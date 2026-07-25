//! Admin API for debug mode: trace listing/retrieval and the plugin sandbox.
//!
//! Every route except `GET /api/debug/config` responds `404` while
//! `debug.enabled` is false. A `403` would confirm the surface exists, and
//! these endpoints dump request contexts and execute plugins — the highest
//! value target on the box. The `404` is defence in depth behind the Basic Auth
//! layer, not a replacement for it.
//!
//! Discoverability is preserved two ways: each gated rejection logs a warning
//! naming the exact config key, and `GET /api/debug/config` always answers so
//! the web UI can render an explanatory empty state instead of a mystery.

use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use crate::debug::diff::{diff, Change};
use crate::debug::sandbox::{synthesize_policy, SandboxRequest};
use crate::debug::store::TraceSummary;
use crate::debug::{new_trace_id, NodeStep, Trace, TraceRecorder, TraceSource};
use crate::graph::{compile_policy, validate_policy};
use crate::state::SharedState;

/// Builds the router for the `/api/debug/*` endpoints.
pub fn router() -> Router<Arc<SharedState>> {
    Router::new()
        .route("/api/debug/config", get(get_config))
        .route("/api/debug/traces", get(list_traces).delete(clear_traces))
        .route("/api/debug/traces/{id}", get(get_trace))
        .route("/api/debug/sandbox", post(run_sandbox))
}

/// The `404` returned when debug mode is off, matching the shape the rest of
/// the Admin API uses for a missing resource.
fn disabled(what: &str) -> axum::response::Response {
    tracing::warn!(
        "{} was requested but debug mode is off; set `debug.enabled: true` in \
         system.yaml (or FEATHERBIT_DEBUG=true) and restart",
        what
    );
    (
        StatusCode::NOT_FOUND,
        Json(serde_json::json!({"error": "not_found"})),
    )
        .into_response()
}

/// `GET /api/debug/config` — the effective debug settings.
///
/// Deliberately **not** gated: it returns only booleans the UI needs to explain
/// itself, and answering it is what turns "the button does nothing" into "debug
/// mode is off, here is the key to set".
async fn get_config(State(state): State<Arc<SharedState>>) -> impl IntoResponse {
    let d = &state.debug;
    Json(serde_json::json!({
        "enabled": d.enabled,
        "sandbox": d.sandbox_enabled,
        "trigger_header": d.trigger_header,
        "trace_all": d.trace_all,
        "capture_bodies": d.capture_bodies,
        "max_traces": d.max_traces,
        "max_steps": d.max_steps,
        "max_body_bytes": d.max_body_bytes,
        "traces_stored": d.len(),
    }))
}

/// Optional filters for the trace list, so a developer can pull just the recent
/// requests on the policy or route they are working on. All are ANDed; empty
/// strings are ignored (a bare `?route=` is not a filter).
#[derive(Debug, Default, Deserialize)]
struct TraceFilter {
    route: Option<String>,
    policy: Option<String>,
    status: Option<u16>,
    /// `request` or `sandbox`.
    source: Option<String>,
    /// Cap on the number of rows returned, applied after filtering.
    limit: Option<usize>,
}

fn non_empty(s: &Option<String>) -> Option<&str> {
    s.as_deref().map(str::trim).filter(|s| !s.is_empty())
}

fn source_str(source: TraceSource) -> &'static str {
    match source {
        TraceSource::Request => "request",
        TraceSource::Sandbox => "sandbox",
    }
}

/// Keeps only the summaries matching every supplied filter.
fn apply_filter(mut traces: Vec<TraceSummary>, f: &TraceFilter) -> Vec<TraceSummary> {
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

/// `GET /api/debug/traces` — summaries, newest first.
///
/// Optional query filters (`route`, `policy`, `status`, `source`, `limit`) let
/// you narrow the buffer to the recent requests on one policy or route, then
/// select one to inspect via `GET /api/debug/traces/{id}`.
async fn list_traces(
    State(state): State<Arc<SharedState>>,
    Query(filter): Query<TraceFilter>,
) -> impl IntoResponse {
    if !state.debug.enabled {
        return disabled("trace listing");
    }
    let traces = apply_filter(state.debug.list(), &filter);
    Json(serde_json::json!({ "traces": traces })).into_response()
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
fn render_trace(trace: &Trace) -> serde_json::Value {
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

/// `GET /api/debug/traces/{id}` — one trace with computed changes.
///
/// Errors: `404` if the id is unknown or has been evicted from the ring buffer.
async fn get_trace(
    State(state): State<Arc<SharedState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    if !state.debug.enabled {
        return disabled("trace retrieval");
    }
    match state.debug.get(&id) {
        Some(trace) => Json(render_trace(&trace)).into_response(),
        None => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "not_found"})),
        )
            .into_response(),
    }
}

/// `DELETE /api/debug/traces` — empties the buffer.
async fn clear_traces(State(state): State<Arc<SharedState>>) -> impl IntoResponse {
    if !state.debug.enabled {
        return disabled("trace clearing");
    }
    let removed = state.debug.clear();
    Json(serde_json::json!({ "status": "cleared", "removed": removed })).into_response()
}

fn bad_request(msg: impl Into<String>) -> axum::response::Response {
    (
        StatusCode::BAD_REQUEST,
        Json(serde_json::json!({"error": msg.into()})),
    )
        .into_response()
}

/// `POST /api/debug/sandbox` — run ad-hoc nodes or a named policy against a
/// synthetic context.
///
/// Plugins execute **for real**: outbound callouts are made, rate-limit
/// counters decrement, breakers trip, loggers fire. That is the point (a
/// `key-auth` node that did not resolve real consumers would be a lie), but it
/// is why the endpoint is gated behind `debug.enabled` + `debug.sandbox` on top
/// of admin auth, and why every response carries a `warning`.
///
/// Errors: `400` for a malformed request or a plugin config the compiler
/// rejects; `404` for an unknown policy; `504` if the run exceeds
/// `debug.sandbox_timeout_seconds`.
async fn run_sandbox(
    State(state): State<Arc<SharedState>>,
    Json(req): Json<SandboxRequest>,
) -> impl IntoResponse {
    if !state.debug.enabled {
        return disabled("sandbox");
    }
    if !state.debug.sandbox_enabled {
        return disabled("sandbox (debug.sandbox is false)");
    }

    let (mode, policy_name, policy) = match (req.nodes, req.policy) {
        (Some(_), Some(_)) | (None, None) => {
            return bad_request("provide exactly one of 'nodes' or 'policy'")
        }
        (Some(nodes), None) => match synthesize_policy(nodes, req.on_error) {
            Ok(p) => ("nodes", "__sandbox".to_string(), p),
            Err(e) => return bad_request(e),
        },
        (None, Some(name)) => {
            let gw = state.gateway.read().await;
            match gw.policies.iter().find(|p| p.name == name) {
                // Recompile rather than reusing a route's graph: a policy with
                // no route attached is exactly the one being iterated on.
                Some(p) => ("policy", name.clone(), p.clone()),
                None => {
                    return (
                        StatusCode::NOT_FOUND,
                        Json(serde_json::json!({"error": format!("unknown policy '{name}'")})),
                    )
                        .into_response()
                }
            }
        }
    };

    if let Err(errors) = validate_policy(&policy) {
        return bad_request(errors.join("; "));
    }
    let graph = match compile_policy(&policy, state.resources.clone()) {
        Ok(g) => g,
        Err(e) => return bad_request(e),
    };

    let ctx = match crate::debug::sandbox::materialize_context(req.context) {
        Ok(c) => c,
        Err(e) => return bad_request(e),
    };

    tracing::warn!(
        "sandbox run ({}): plugins execute for real against live resources",
        mode
    );

    let recorder = TraceRecorder::new(&ctx, state.debug.capture_options(), state.debug.max_steps);
    let started = Instant::now();
    let timeout = Duration::from_secs(state.debug.sandbox_timeout_seconds.max(1));

    let run = graph.execute_traced(ctx, recorder);
    let (out_ctx, recorder) = match tokio::time::timeout(timeout, run).await {
        Ok(pair) => pair,
        Err(_) => {
            return (
                StatusCode::GATEWAY_TIMEOUT,
                Json(serde_json::json!({
                    "error": "sandbox_timeout",
                    "message": format!(
                        "run exceeded debug.sandbox_timeout_seconds ({}s)",
                        state.debug.sandbox_timeout_seconds
                    ),
                })),
            )
                .into_response()
        }
    };

    let id = new_trace_id();
    let trace = recorder.finish(
        id.clone(),
        state.debug.next_seq(),
        TraceSource::Sandbox,
        None,
        policy_name.clone(),
        &out_ctx,
        started.elapsed(),
    );
    let rendered = render_trace(&trace);
    // Stored alongside live traces so a sandbox run and a real request can be
    // compared side by side in the UI.
    state.debug.record(trace);

    Json(serde_json::json!({
        "mode": mode,
        "policy": policy_name,
        "warning": "plugins executed for real: outbound calls were made and shared \
                    rate-limit/breaker state was mutated",
        "stored_trace_id": id,
        "trace": rendered,
    }))
    .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{DebugConfig, GatewayConfig, SystemConfig};
    use crate::config_store::FileConfigStore;

    fn state_with(debug: DebugConfig) -> Arc<SharedState> {
        // Every section of both configs has a serde default, so an empty
        // document is the cheapest way to get a valid baseline.
        let system = SystemConfig {
            debug,
            ..serde_yaml::from_str::<SystemConfig>("{}").unwrap()
        };
        let gateway: GatewayConfig = serde_yaml::from_str("{}").unwrap();
        Arc::new(
            SharedState::new(
                system,
                gateway,
                None,
                Arc::new(FileConfigStore::new(std::path::PathBuf::from(
                    "gateway.yaml",
                ))),
            )
            .unwrap(),
        )
    }

    /// The gate must not depend on the handler being reached — assert the
    /// predicate the handlers share.
    #[test]
    fn test_disabled_by_default() {
        let s = state_with(DebugConfig::default());
        assert!(!s.debug.enabled);
    }

    #[tokio::test]
    async fn test_config_endpoint_answers_even_when_disabled() {
        let s = state_with(DebugConfig::default());
        let resp = get_config(State(s)).await.into_response();
        // This is the one endpoint that must not 404: the UI reads it to
        // explain *why* debugging is unavailable.
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_trace_routes_404_when_disabled() {
        let s = state_with(DebugConfig::default());
        assert_eq!(
            list_traces(State(s.clone()), Query(TraceFilter::default()))
                .await
                .into_response()
                .status(),
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            get_trace(State(s.clone()), Path("any".to_string()))
                .await
                .into_response()
                .status(),
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            clear_traces(State(s.clone()))
                .await
                .into_response()
                .status(),
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            run_sandbox(State(s), Json(SandboxRequest::default()))
                .await
                .into_response()
                .status(),
            StatusCode::NOT_FOUND
        );
    }

    #[tokio::test]
    async fn test_sandbox_404s_when_only_sandbox_is_off() {
        let s = state_with(DebugConfig {
            enabled: true,
            sandbox: false,
            ..Default::default()
        });
        // Tracing still works...
        assert_eq!(
            list_traces(State(s.clone()), Query(TraceFilter::default()))
                .await
                .into_response()
                .status(),
            StatusCode::OK
        );
        // ...but plugin execution is separately withheld.
        assert_eq!(
            run_sandbox(State(s), Json(SandboxRequest::default()))
                .await
                .into_response()
                .status(),
            StatusCode::NOT_FOUND
        );
    }

    #[tokio::test]
    async fn test_sandbox_requires_exactly_one_mode() {
        let s = state_with(DebugConfig {
            enabled: true,
            ..Default::default()
        });
        // Neither.
        assert_eq!(
            run_sandbox(State(s.clone()), Json(SandboxRequest::default()))
                .await
                .into_response()
                .status(),
            StatusCode::BAD_REQUEST
        );
        // Both.
        let both = SandboxRequest {
            nodes: Some(Vec::new()),
            policy: Some("p".to_string()),
            ..Default::default()
        };
        assert_eq!(
            run_sandbox(State(s), Json(both))
                .await
                .into_response()
                .status(),
            StatusCode::BAD_REQUEST
        );
    }

    #[tokio::test]
    async fn test_sandbox_unknown_policy_404s() {
        let s = state_with(DebugConfig {
            enabled: true,
            ..Default::default()
        });
        let req = SandboxRequest {
            policy: Some("nope".to_string()),
            ..Default::default()
        };
        assert_eq!(
            run_sandbox(State(s), Json(req))
                .await
                .into_response()
                .status(),
            StatusCode::NOT_FOUND
        );
    }

    fn summary(route: &str, policy: &str, status: u16, source: TraceSource) -> TraceSummary {
        TraceSummary {
            id: format!("{route}-{status}"),
            seq: 0,
            source,
            started_ms: 0,
            route: Some(route.to_string()),
            policy: policy.to_string(),
            method: "GET".to_string(),
            path: "/x".to_string(),
            status,
            duration_us: 1,
            step_count: 1,
            error_count: 0,
            captured_bodies: false,
        }
    }

    fn sample() -> Vec<TraceSummary> {
        vec![
            summary("echo-api", "echo-policy", 200, TraceSource::Request),
            summary("secure-api", "secure-policy", 401, TraceSource::Request),
            summary("echo-api", "echo-policy", 502, TraceSource::Request),
            summary("s", "echo-policy", 200, TraceSource::Sandbox),
        ]
    }

    fn ids(traces: Vec<TraceSummary>) -> Vec<String> {
        traces.into_iter().map(|t| t.id).collect()
    }

    #[test]
    fn test_no_filter_returns_all() {
        assert_eq!(apply_filter(sample(), &TraceFilter::default()).len(), 4);
    }

    #[test]
    fn test_filter_by_route() {
        let f = TraceFilter {
            route: Some("echo-api".to_string()),
            ..Default::default()
        };
        assert_eq!(apply_filter(sample(), &f).len(), 2);
    }

    #[test]
    fn test_filter_by_policy_spans_routes() {
        // echo-policy appears on a route and a sandbox run.
        let f = TraceFilter {
            policy: Some("echo-policy".to_string()),
            ..Default::default()
        };
        assert_eq!(apply_filter(sample(), &f).len(), 3);
    }

    #[test]
    fn test_filters_are_anded() {
        let f = TraceFilter {
            policy: Some("echo-policy".to_string()),
            status: Some(200),
            source: Some("request".to_string()),
            ..Default::default()
        };
        assert_eq!(ids(apply_filter(sample(), &f)), vec!["echo-api-200"]);
    }

    #[test]
    fn test_empty_filter_string_is_ignored() {
        // A bare `?route=` must not filter everything out.
        let f = TraceFilter {
            route: Some("  ".to_string()),
            ..Default::default()
        };
        assert_eq!(apply_filter(sample(), &f).len(), 4);
    }

    #[test]
    fn test_limit_applies_after_filtering() {
        let f = TraceFilter {
            policy: Some("echo-policy".to_string()),
            limit: Some(2),
            ..Default::default()
        };
        assert_eq!(apply_filter(sample(), &f).len(), 2);
    }

    #[test]
    fn test_filter_by_source() {
        let f = TraceFilter {
            source: Some("sandbox".to_string()),
            ..Default::default()
        };
        assert_eq!(ids(apply_filter(sample(), &f)), vec!["s-200"]);
    }

    #[tokio::test]
    async fn test_sandbox_rejects_unknown_plugin_type() {
        let s = state_with(DebugConfig {
            enabled: true,
            ..Default::default()
        });
        let req = SandboxRequest {
            nodes: Some(vec![crate::config::NodeConfig {
                id: "x".to_string(),
                node_type: "no-such-plugin".to_string(),
                config: Default::default(),
                position: None,
            }]),
            ..Default::default()
        };
        assert_eq!(
            run_sandbox(State(s), Json(req))
                .await
                .into_response()
                .status(),
            StatusCode::BAD_REQUEST
        );
    }
}
