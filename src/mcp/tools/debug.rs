//! Read tools over debug mode: trace listing/inspection and the sandbox.

use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

use super::ToolError;
use crate::debug::render::{apply_filter, render_trace, TraceFilter};
use crate::debug::sandbox::{run_sandbox, SandboxError, SandboxRequest};
use crate::state::SharedState;

#[derive(Debug, Default, Deserialize, JsonSchema)]
pub struct ListTracesArgs {
    /// Only traces recorded for this route name.
    pub route: Option<String>,
    /// Only traces of this policy.
    pub policy: Option<String>,
    /// Only traces whose final response had this status.
    pub status: Option<u16>,
    /// `request` (real traffic) or `sandbox`.
    pub source: Option<String>,
    /// Maximum rows (newest first). Default 20.
    pub limit: Option<usize>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GetTraceArgs {
    /// Trace id from list_traces.
    pub id: String,
    /// Include the full context snapshot after every step (large). Default false;
    /// use get_trace_step for one node's before/after.
    #[serde(default)]
    pub include_snapshots: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GetTraceStepArgs {
    /// Trace id from list_traces.
    pub id: String,
    /// Node id of the step (as shown in the trace). Either this or `index`.
    pub node_id: Option<String>,
    /// Zero-based step index. Either this or `node_id`.
    pub index: Option<usize>,
}

fn require_debug(state: &SharedState) -> Result<(), ToolError> {
    if state.debug.enabled {
        Ok(())
    } else {
        Err(ToolError::debug_disabled())
    }
}

pub async fn list_traces(state: &SharedState, a: ListTracesArgs) -> Result<Value, ToolError> {
    require_debug(state)?;
    let f = TraceFilter {
        route: a.route,
        policy: a.policy,
        status: a.status,
        source: a.source,
        limit: Some(a.limit.unwrap_or(20)),
    };
    let traces = apply_filter(state.debug.list(), &f);
    Ok(serde_json::json!({ "traces": traces }))
}

pub async fn get_trace(state: &SharedState, a: GetTraceArgs) -> Result<Value, ToolError> {
    require_debug(state)?;
    let trace = state
        .debug
        .get(&a.id)
        .ok_or_else(|| ToolError::not_found("trace", &a.id))?;
    let mut v = render_trace(&trace);
    if !a.include_snapshots {
        if let Some(obj) = v.as_object_mut() {
            obj.remove("initial");
        }
        if let Some(steps) = v["steps"].as_array_mut() {
            for s in steps {
                if let Some(o) = s.as_object_mut() {
                    o.remove("after");
                }
            }
        }
        v["snapshots_omitted"] = Value::Bool(true);
    }
    Ok(v)
}

pub async fn get_trace_step(state: &SharedState, a: GetTraceStepArgs) -> Result<Value, ToolError> {
    require_debug(state)?;
    let trace = state
        .debug
        .get(&a.id)
        .ok_or_else(|| ToolError::not_found("trace", &a.id))?;
    let idx = match (&a.node_id, a.index) {
        (Some(id), _) => trace
            .steps
            .iter()
            .position(|s| &s.node_id == id)
            .ok_or_else(|| ToolError::not_found("step for node", id))?,
        (None, Some(i)) if i < trace.steps.len() => i,
        (None, Some(i)) => return Err(ToolError::not_found("step index", &i.to_string())),
        (None, None) => return Err(ToolError::invalid_input("provide node_id or index")),
    };
    let rendered = render_trace(&trace);
    let step = rendered["steps"][idx].clone();
    let before = if idx == 0 {
        &trace.initial
    } else {
        &trace.steps[idx - 1].after
    };
    let node_config = {
        let gw = state.gateway.read().await;
        gw.policies
            .iter()
            .find(|p| p.name == trace.policy)
            .and_then(|p| p.nodes.iter().find(|n| n.id == trace.steps[idx].node_id))
            .map(|n| serde_json::to_value(n).unwrap_or(Value::Null))
            .unwrap_or(Value::Null)
    };
    Ok(serde_json::json!({
        "trace_id": trace.id,
        "policy": trace.policy,
        "step": step,
        "before": serde_json::to_value(before).map_err(|e| ToolError::internal(e.to_string()))?,
        "after": serde_json::to_value(&trace.steps[idx].after).map_err(|e| ToolError::internal(e.to_string()))?,
        "node_config": node_config,
    }))
}

/// `run_sandbox` takes the same body as `POST /api/debug/sandbox`.
pub async fn run_sandbox_tool(state: &SharedState, a: Value) -> Result<Value, ToolError> {
    let req: SandboxRequest = serde_json::from_value(a)
        .map_err(|e| ToolError::invalid_input(format!("invalid sandbox request: {e}")))?;
    match run_sandbox(state, req).await {
        Ok(r) => Ok(serde_json::json!({
            "mode": r.mode,
            "policy": r.policy,
            "warning": "plugins executed for real: outbound calls were made and shared rate-limit/breaker state was mutated",
            "stored_trace_id": r.stored_trace_id,
            "trace": r.trace,
        })),
        Err(SandboxError::Disabled) => Err(ToolError::debug_disabled()),
        Err(SandboxError::SandboxDisabled) => Err(ToolError::sandbox_disabled()),
        Err(SandboxError::BadRequest(m)) => Err(ToolError::invalid_input(m)),
        Err(SandboxError::UnknownPolicy(n)) => Err(ToolError::not_found("policy", &n)),
        Err(SandboxError::Timeout(s)) => Err(ToolError::internal(format!(
            "run exceeded debug.sandbox_timeout_seconds ({s}s)"
        ))),
    }
}

/// Schema for `run_sandbox`: a permissive object (the body is documented by
/// the sandbox docs; nodes/policy are mutually exclusive).
#[derive(Debug, Deserialize, JsonSchema)]
pub struct SandboxArgs {
    /// Ad-hoc node list to run in order (exclusive with `policy`).
    pub nodes: Option<Vec<Value>>,
    /// Name of a stored policy to run (exclusive with `nodes`).
    pub policy: Option<String>,
    /// `stop` (default) or `client`: what an error port does in nodes mode.
    pub on_error: Option<String>,
    /// Synthetic request: {method, path, host, headers, query_params, body, message, response}.
    pub context: Option<Value>,
}

#[cfg(test)]
mod tests {
    use crate::mcp::tools::call;
    use crate::mcp::tools::test_support::{obj, state, ECHO_GATEWAY};

    #[tokio::test]
    async fn debug_off_is_a_tool_error() {
        let s = state("{}", ECHO_GATEWAY);
        for (tool, a) in [
            ("list_traces", serde_json::json!({})),
            ("get_trace", serde_json::json!({"id": "x"})),
            ("get_trace_step", serde_json::json!({"id": "x", "index": 0})),
            (
                "run_sandbox",
                serde_json::json!({"policy": "echo-policy", "context": {}}),
            ),
        ] {
            let err = call(&s, tool, obj(a)).await.unwrap_err();
            assert_eq!(err.code, "debug_disabled", "{tool}");
            assert!(err.hint.as_deref().unwrap().contains("debug.enabled"));
        }
    }

    #[tokio::test]
    async fn sandbox_then_inspect_trace() {
        let s = state("debug:\n  enabled: true\n", ECHO_GATEWAY);
        let run = call(
            &s,
            "run_sandbox",
            obj(serde_json::json!({"policy": "echo-policy", "context": {"path": "/hello"}})),
        )
        .await
        .unwrap();
        let id = run["stored_trace_id"].as_str().unwrap().to_string();

        let list = call(
            &s,
            "list_traces",
            obj(serde_json::json!({"source": "sandbox"})),
        )
        .await
        .unwrap();
        assert_eq!(list["traces"][0]["id"], id);
        let list = call(
            &s,
            "list_traces",
            obj(serde_json::json!({"policy": "other"})),
        )
        .await
        .unwrap();
        assert!(list["traces"].as_array().unwrap().is_empty());

        let t = call(&s, "get_trace", obj(serde_json::json!({"id": id})))
            .await
            .unwrap();
        assert_eq!(t["snapshots_omitted"], true);
        assert!(t.get("initial").is_none());
        assert!(t["steps"][0].get("after").is_none());
        assert!(t["steps"][0]["changes"].is_array());
        let t = call(
            &s,
            "get_trace",
            obj(serde_json::json!({"id": id, "include_snapshots": true})),
        )
        .await
        .unwrap();
        assert!(t["initial"].is_object() && t["steps"][0]["after"].is_object());

        let st = call(
            &s,
            "get_trace_step",
            obj(serde_json::json!({"id": id, "node_id": "e"})),
        )
        .await
        .unwrap();
        assert_eq!(st["step"]["node_id"], "e");
        assert_eq!(st["node_config"]["type"], "echo");
        assert!(st["before"].is_object() && st["after"].is_object());
        let err = call(
            &s,
            "get_trace_step",
            obj(serde_json::json!({"id": id, "node_id": "zz"})),
        )
        .await
        .unwrap_err();
        assert_eq!(err.code, "not_found");
        let err = call(&s, "get_trace_step", obj(serde_json::json!({"id": id})))
            .await
            .unwrap_err();
        assert_eq!(err.code, "invalid_input");

        let err = call(
            &s,
            "run_sandbox",
            obj(serde_json::json!({"policy": "nope", "context": {}})),
        )
        .await
        .unwrap_err();
        assert_eq!(err.code, "not_found");
        let err = call(&s, "run_sandbox", obj(serde_json::json!({"context": {}})))
            .await
            .unwrap_err();
        assert_eq!(err.code, "invalid_input");
    }
}
