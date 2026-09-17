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
    // Agents read this tool's output as evidence. Without `retention`, an
    // empty result after a filter reads as "the filter matched nothing",
    // which has already produced a confident and wrong "the filter is
    // broken" conclusion when the traces had simply aged out.
    Ok(serde_json::json!({
        "traces": traces,
        "retention": state.debug.retention(),
    }))
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

/// `run_sandbox` takes the same body as `POST /api/debug/sandbox`. Shape
/// errors come back with a hint showing the accepted payload, because a model
/// that guessed wrong needs the correct shape, not just the field name.
pub async fn run_sandbox_tool(state: &SharedState, a: Value) -> Result<Value, ToolError> {
    let req: SandboxRequest = serde_json::from_value(a)
        .map_err(|e| ToolError::sandbox_bad_request(format!("invalid sandbox request: {e}")))?;
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
        Err(SandboxError::BadRequest(m)) => Err(ToolError::sandbox_bad_request(m)),
        Err(SandboxError::UnknownPolicy(n)) => Err(ToolError::not_found("policy", &n)),
        Err(SandboxError::Timeout(s)) => Err(ToolError::internal(format!(
            "run exceeded debug.sandbox_timeout_seconds ({s}s)"
        ))),
    }
}

/// Schema for `run_sandbox`. Typed and documented field by field so an agent
/// sees the exact payload shape; the tool still hands the raw JSON to the
/// sandbox runner, which validates (and forgives a few common variants:
/// `query`/`uri` aliases, JSON bodies written as objects, numeric header
/// values, `response.status`).
// Only ever used through `schema_of::<SandboxArgs>()`.
#[allow(dead_code)]
#[derive(Debug, Deserialize, JsonSchema)]
pub struct SandboxArgs {
    /// Ad-hoc nodes to run in order, chained through their success ports; a
    /// `listener` and `client` are added for you. Exclusive with `policy`.
    pub nodes: Option<Vec<SandboxNodeArgs>>,
    /// Name of a stored policy to run. Exclusive with `nodes`.
    pub policy: Option<String>,
    /// Nodes mode only: `stop` (default) leaves error ports unwired so a
    /// failing node shows `edge: "unhandled"`; `client` wires them to `client`.
    pub on_error: Option<SandboxOnError>,
    /// The synthetic request. Every field is optional; `{}` is `GET /`.
    /// Flat shape — do NOT nest under `request` (a trace snapshot's nested
    /// `{request, response, message}` object is also accepted).
    pub context: Option<SandboxContextArgs>,
}

#[allow(dead_code)]
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum SandboxOnError {
    Stop,
    Client,
}

/// One ad-hoc node: `{ "id": "rw", "type": "proxy-rewrite", "config": {...} }`.
#[allow(dead_code)]
#[derive(Debug, Deserialize, JsonSchema)]
pub struct SandboxNodeArgs {
    /// Node id; defaults to `<type>-<index>` when omitted.
    pub id: Option<String>,
    /// Node type as in YAML `type:` (see list_node_types / get_node_type).
    #[serde(rename = "type")]
    pub node_type: String,
    /// The node's config object; keys per get_node_type(<type>).
    pub config: Option<Value>,
}

/// Header / query-parameter values: a string, or a list of strings for
/// repeated values. Numbers and booleans are accepted and stringified.
#[allow(dead_code)]
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum SandboxValue {
    One(String),
    Many(Vec<String>),
}

#[allow(dead_code)]
#[derive(Debug, Deserialize, JsonSchema)]
pub struct SandboxContextArgs {
    /// HTTP method. Default `GET`.
    pub method: Option<String>,
    /// Request path without the query string, e.g. `/hello/frenk`. Default `/`.
    pub path: Option<String>,
    /// Host header value. Default `sandbox.local`.
    pub host: Option<String>,
    /// `http` (default) or `https`.
    pub scheme: Option<String>,
    /// Request headers as an object: `{"authorization": "Bearer x", "accept": ["a", "b"]}`.
    pub headers: Option<std::collections::HashMap<String, SandboxValue>>,
    /// Query parameters as an object (not a query string): `{"page": "2"}`.
    pub query_params: Option<std::collections::HashMap<String, SandboxValue>>,
    /// Request body as text. A JSON body may be passed as a JSON string
    /// (`"{\"a\":1}"`) or directly as an object — it is serialized for you.
    /// Add a `content-type` header yourself when a plugin needs it.
    pub body: Option<Value>,
    /// Base64 body for binary payloads. Exclusive with `body`.
    pub body_base64: Option<String>,
    /// Client address `ip:port`. Default `127.0.0.1:0`.
    pub remote_addr: Option<String>,
    /// `http1` (default) or `http2`.
    pub protocol: Option<String>,
    /// Pre-seeded `context.message` entries (e.g. what an earlier node would
    /// have set), keyed by name.
    pub message: Option<std::collections::HashMap<String, Value>>,
    /// Seed the response to exercise response-phase plugins (response-rewrite,
    /// loggers): `{"status_code": 200, "headers": {...}, "body": "..."}`.
    pub response: Option<SandboxResponseArgs>,
}

#[allow(dead_code)]
#[derive(Debug, Deserialize, JsonSchema)]
pub struct SandboxResponseArgs {
    /// Response status, e.g. 200.
    pub status_code: Option<u16>,
    /// Response headers, same shape as request headers.
    pub headers: Option<std::collections::HashMap<String, SandboxValue>>,
    /// Response body as text (a JSON object is serialized for you).
    pub body: Option<Value>,
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

        // An empty result must arrive with the retention window attached.
        // This is the agent-facing half of the ambiguity: without it, "no
        // traces matched this filter" and "the matching traces were evicted"
        // are the same JSON, and the difference decides whether a reader
        // concludes the filter is broken.
        let r = &list["retention"];
        assert!(!r.is_null(), "a listing must report its retention window");
        assert_eq!(r["truncated"], false, "nothing was evicted in this test");
        assert_eq!(r["evicted"], 0);
        assert!(
            r["retained"].as_u64().unwrap() >= 1,
            "the sandbox trace is still held: {r}"
        );

        // The positive case: filtering by the policy the trace actually ran
        // must return it. Without this, a filter that always matched nothing
        // would satisfy the negative assertion above and look correct.
        let list = call(
            &s,
            "list_traces",
            obj(serde_json::json!({"policy": "echo-policy"})),
        )
        .await
        .unwrap();
        assert_eq!(
            list["traces"].as_array().unwrap().len(),
            1,
            "policy filter dropped a trace that ran under that policy: {}",
            list["traces"]
        );
        assert_eq!(list["traces"][0]["id"], id);

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
        // Shape errors carry the accepted payload so an agent can self-correct.
        assert!(
            err.hint
                .as_deref()
                .unwrap_or("")
                .contains("FLAT \"context\""),
            "{err:?}"
        );
    }

    #[tokio::test]
    async fn run_sandbox_forgives_common_agent_shapes_and_explains_typos() {
        let s = state("debug:\n  enabled: true\n", ECHO_GATEWAY);
        // `uri`/`query` aliases, an object body and a numeric header value.
        let v = call(
            &s,
            "run_sandbox",
            obj(serde_json::json!({
                "policy": "echo-policy",
                "context": {"uri": "/hello", "query": {"page": 2}, "headers": {"x-n": 1}, "body": {"a": 1}}
            })),
        )
        .await
        .unwrap();
        assert_eq!(v["trace"]["path"], "/hello");
        assert_eq!(
            v["trace"]["initial"]["request"]["query_params"]["page"][0],
            "2"
        );

        // A genuine typo is still rejected, with the shape hint attached.
        let err = call(
            &s,
            "run_sandbox",
            obj(serde_json::json!({"policy": "echo-policy", "context": {"paths": "/x"}})),
        )
        .await
        .unwrap_err();
        assert_eq!(err.code, "invalid_input");
        assert!(err.message.contains("paths"), "{err:?}");
        assert!(err.hint.is_some());

        // The tool schema documents the context fields for the model.
        let schema = serde_json::to_value(super::super::schema_of::<super::SandboxArgs>()).unwrap();
        let ctx_ref = schema["properties"]["context"].to_string();
        assert!(
            ctx_ref.contains("SandboxContextArgs") || ctx_ref.contains("query_params"),
            "{ctx_ref}"
        );
        let defs = schema
            .get("$defs")
            .or_else(|| schema.get("definitions"))
            .cloned()
            .unwrap_or_default();
        let all = format!("{schema}{defs}");
        for key in ["query_params", "status_code", "body_base64", "on_error"] {
            assert!(all.contains(key), "schema should mention {key}");
        }
    }
}
