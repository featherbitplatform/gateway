//! The plugin sandbox: run plugins against a synthetic [`Context`].
//!
//! Two modes, both producing the same [`Trace`](super::Trace) shape a live
//! request produces — so the UI viewer, the diff, and the redaction tests are
//! written once and serve both:
//!
//! - **ad-hoc nodes** — a list of plugin nodes run in order, for testing one
//!   plugin (or a handful) in isolation;
//! - **named policy** — a configured policy replayed against a synthetic
//!   request, for testing a whole pipeline.
//!
//! Ad-hoc mode does **not** get its own executor. It synthesises a
//! [`PolicyConfig`] — prepending a `listener` and appending a `client`, which
//! [`validate_policy`](crate::graph::validate_policy) requires — and reuses
//! [`compile_policy`](crate::graph::compile_policy). One executor means a
//! sandbox run can never diverge from what the gateway really does.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use bytes::Bytes;
use serde::Deserialize;

use crate::config::{EdgeConfig, NodeConfig, PolicyConfig};
use crate::context::{Context, GatewayRequest, GatewayResponse, Protocol};
use crate::debug::render::render_trace;
use crate::debug::{new_trace_id, TraceRecorder, TraceSource};
use crate::graph::{compile_policy, prepare_policy};
use crate::state::SharedState;

/// A header/query value given as either a bare string or a list.
///
/// Ergonomics: `{"headers": {"apikey": "abc"}}` should work without forcing the
/// caller to write `["abc"]` to satisfy the multi-valued representation.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum StringOrList {
    One(String),
    Many(Vec<String>),
}

impl StringOrList {
    fn into_vec(self) -> Vec<String> {
        match self {
            Self::One(s) => vec![s],
            Self::Many(v) => v,
        }
    }
}

/// Optional seed for `Context.response`, so response-phase plugins
/// (`response-rewrite`, the loggers) have something to act on.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ResponseInput {
    pub status_code: Option<u16>,
    pub headers: HashMap<String, StringOrList>,
    pub body: Option<String>,
}

/// A synthetic context. Every field is optional: posting `{}` yields a valid
/// `GET /` run.
///
/// `deny_unknown_fields` is deliberate here even though it is unusual — a
/// typo'd `"paths"` must fail loudly rather than silently default to `/` and
/// produce a baffling trace.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SandboxContextInput {
    pub method: Option<String>,
    pub path: Option<String>,
    pub host: Option<String>,
    pub scheme: Option<String>,
    pub headers: HashMap<String, StringOrList>,
    pub query_params: HashMap<String, StringOrList>,
    /// Plain UTF-8 body. Mutually exclusive with `body_base64`.
    pub body: Option<String>,
    /// Base64 body, for binary payloads. Mutually exclusive with `body`.
    pub body_base64: Option<String>,
    pub remote_addr: Option<String>,
    pub protocol: Option<Protocol>,
    pub message: HashMap<String, serde_json::Value>,
    pub response: Option<ResponseInput>,
}

impl SandboxContextInput {
    /// Materialises a full [`Context`], filling in defaults.
    pub fn into_context(self) -> Result<Context, String> {
        if self.body.is_some() && self.body_base64.is_some() {
            return Err("provide only one of 'body' or 'body_base64'".to_string());
        }
        let body = match (self.body, self.body_base64) {
            (Some(text), _) => Bytes::from(text.into_bytes()),
            (None, Some(b64)) => Bytes::from(
                BASE64
                    .decode(b64.as_bytes())
                    .map_err(|e| format!("body_base64 is not valid base64: {e}"))?,
            ),
            (None, None) => Bytes::new(),
        };

        let to_map = |m: HashMap<String, StringOrList>| -> HashMap<String, Vec<String>> {
            m.into_iter().map(|(k, v)| (k, v.into_vec())).collect()
        };

        let response = self.response.unwrap_or_default();
        Ok(Context {
            request: GatewayRequest {
                method: self.method.unwrap_or_else(|| "GET".to_string()),
                path: self.path.unwrap_or_else(|| "/".to_string()),
                host: self.host.unwrap_or_else(|| "sandbox.local".to_string()),
                scheme: self.scheme.unwrap_or_else(|| "http".to_string()),
                headers: to_map(self.headers),
                query_params: to_map(self.query_params),
                body,
                remote_addr: self
                    .remote_addr
                    .unwrap_or_else(|| "127.0.0.1:0".to_string()),
                protocol: self.protocol.unwrap_or(Protocol::Http1),
            },
            response: GatewayResponse {
                status_code: response.status_code.unwrap_or(0),
                headers: to_map(response.headers),
                body: response
                    .body
                    .map(|b| Bytes::from(b.into_bytes()))
                    .unwrap_or_default(),
                stream: None,
            },
            message: self.message,
            errors: Vec::new(),
        })
    }
}

/// What to do when an ad-hoc node fails.
#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum OnError {
    /// Leave error ports unwired, so a failing node hits the engine's
    /// no-error-handling branch and the trace records `edge: "unhandled"`.
    /// This makes the consequence of an unwired error port *visible* — the
    /// single thing policy authors most often get wrong.
    #[default]
    Stop,
    /// Wire every node's error port to `client`, preserving the plugin's own
    /// status code (the wiring a real auth/redirect node needs).
    Client,
}

/// A sandbox request: exactly one of `nodes` or `policy`.
///
/// `context` is taken as a raw value and normalised by [`materialize_context`],
/// so it accepts **both** the flat hand-authored shape *and* a context copied
/// straight out of a trace (the nested `{request, response, message, errors}`
/// snapshot shape). Typos in the flat shape are still rejected — validation
/// runs after normalisation.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SandboxRequest {
    pub nodes: Option<Vec<NodeConfig>>,
    pub policy: Option<String>,
    pub on_error: OnError,
    pub context: serde_json::Value,
}

/// Turns a sandbox `context` value — flat or the nested trace-snapshot shape —
/// into a [`Context`].
///
/// A context copied from `GET /api/debug/traces/{id}` (a step's `after`, or the
/// trace's `initial`) is nested (`request`/`response` objects, bodies as
/// `{len, text, …}` objects) and carries display-only fields (`errors`, body
/// `len`/`truncated`/`binary`). This flattens that into the flat input shape
/// before deserialising, so pasting a trace context "just works".
///
/// Note: snapshots are **redacted** — a header shown as `<redacted>` replays
/// literally as that string, and a truncated or binary body cannot be
/// reconstructed. The sandbox replays what the trace could show, not the
/// original secret bytes.
pub fn materialize_context(raw: serde_json::Value) -> Result<Context, String> {
    let normalized = normalize_context(raw);
    if normalized.is_null() {
        return SandboxContextInput::default().into_context();
    }
    let input: SandboxContextInput =
        serde_json::from_value(normalized).map_err(|e| format!("invalid sandbox context: {e}"))?;
    input.into_context()
}

/// Flattens the nested trace-snapshot shape into the flat input shape. A value
/// that is already flat passes through essentially unchanged.
fn normalize_context(v: serde_json::Value) -> serde_json::Value {
    use serde_json::Value;
    let Value::Object(mut obj) = v else { return v };

    // Nested `request` object (snapshot shape) -> lift its fields to the top,
    // converting the body object to a plain string.
    if let Some(Value::Object(req)) = obj.remove("request") {
        for (k, val) in req {
            if k == "body" {
                let is_snapshot = val.as_object().is_some_and(|o| o.contains_key("len"));
                if let Some(text) = body_text(&val) {
                    obj.insert("body".to_string(), Value::String(text));
                } else if !is_snapshot {
                    // A plain JSON body written as an object: keep it for the
                    // coercion below. (A binary or uncaptured snapshot body has
                    // no faithful text to replay and is dropped.)
                    obj.insert("body".to_string(), val);
                }
            } else {
                obj.insert(k, val);
            }
        }
    }
    // Response body in the snapshot shape (`{len, text, …}`) -> string, or
    // dropped when the snapshot has no replayable text. A plain JSON object
    // body (no `len`) is left for the coercion below.
    if let Some(resp) = obj.get_mut("response").and_then(Value::as_object_mut) {
        if let Some(body) = resp.get("body").cloned() {
            let is_snapshot = body.as_object().is_some_and(|o| o.contains_key("len"));
            match body_text(&body) {
                Some(text) => {
                    resp.insert("body".to_string(), Value::String(text));
                }
                None if is_snapshot => {
                    resp.remove("body");
                }
                None => {}
            }
        }
    }
    // Display-only fields the sandbox does not model.
    obj.remove("errors");

    // Forgiving aliases and coercions for the shapes agents and humans most
    // often produce. Strict `deny_unknown_fields` still catches real typos.
    for (alias, canonical) in [("query", "query_params"), ("uri", "path")] {
        if let Some(v) = obj.remove(alias) {
            obj.entry(canonical.to_string()).or_insert(v);
        }
    }
    // A JSON body given as an object/array (or a number/bool) → its text.
    if let Some(body) = obj.get("body") {
        if let Some(text) = scalar_or_json_text(body) {
            obj.insert("body".to_string(), Value::String(text));
        }
    }
    // Header / query values: numbers and bools become strings; lists stay lists.
    for map_key in ["headers", "query_params"] {
        if let Some(Value::Object(map)) = obj.get_mut(map_key) {
            for v in map.values_mut() {
                if matches!(v, Value::Number(_) | Value::Bool(_)) {
                    *v = Value::String(v.to_string());
                }
            }
        }
    }
    if let Some(resp) = obj.get_mut("response").and_then(Value::as_object_mut) {
        if let Some(v) = resp.remove("status") {
            resp.entry("status_code".to_string()).or_insert(v);
        }
        if let Some(body) = resp.get("body") {
            if let Some(text) = scalar_or_json_text(body) {
                resp.insert("body".to_string(), Value::String(text));
            }
        }
    }
    Value::Object(obj)
}

/// Text for a non-string scalar (`42` → `"42"`, `true` → `"true"`) or a
/// JSON object/array (serialized compactly). `None` leaves strings, nulls
/// and anything else untouched.
fn scalar_or_json_text(v: &serde_json::Value) -> Option<String> {
    use serde_json::Value;
    match v {
        Value::Number(_) | Value::Bool(_) => Some(v.to_string()),
        Value::Object(_) | Value::Array(_) => Some(v.to_string()),
        _ => None,
    }
}

/// Extracts replayable body text: a plain string, or a snapshot body object's
/// `text` field. `None` for a binary or uncaptured body.
fn body_text(v: &serde_json::Value) -> Option<String> {
    match v {
        serde_json::Value::String(s) => Some(s.clone()),
        serde_json::Value::Object(o) => o.get("text").and_then(|t| t.as_str()).map(String::from),
        _ => None,
    }
}

/// Synthesises a runnable policy from an ad-hoc node list.
///
/// Prepends `listener`, appends `client`, and chains the user's nodes through
/// their success ports.
pub fn synthesize_policy(
    nodes: Vec<NodeConfig>,
    on_error: OnError,
) -> Result<PolicyConfig, String> {
    if nodes.is_empty() {
        return Err("'nodes' must contain at least one node".to_string());
    }

    let mut seen = std::collections::HashSet::new();
    let mut user_nodes = Vec::with_capacity(nodes.len());
    for (i, mut n) in nodes.into_iter().enumerate() {
        if n.node_type == "listener" || n.node_type == "client" {
            return Err(format!(
                "node '{}': '{}' nodes are added automatically and cannot be supplied",
                n.id, n.node_type
            ));
        }
        if n.id.trim().is_empty() {
            n.id = format!("{}-{}", n.node_type, i);
        }
        if !seen.insert(n.id.clone()) {
            return Err(format!("duplicate node id '{}'", n.id));
        }
        user_nodes.push(n);
    }

    let mut edges = vec![EdgeConfig {
        from: "listener.out".to_string(),
        to: format!("{}.in", user_nodes[0].id),
    }];
    for pair in user_nodes.windows(2) {
        edges.push(EdgeConfig {
            from: format!("{}.success", pair[0].id),
            to: format!("{}.in", pair[1].id),
        });
    }
    edges.push(EdgeConfig {
        from: format!("{}.success", user_nodes[user_nodes.len() - 1].id),
        to: "client.in".to_string(),
    });
    if on_error == OnError::Client {
        for n in &user_nodes {
            edges.push(EdgeConfig {
                from: format!("{}.error", n.id),
                to: "client.in".to_string(),
            });
        }
    }

    // Any output port the chain wiring above didn't already cover — an
    // "outcome" port on a node that isn't last in the chain — must still be
    // wired, or compilation will reject it as an unwired mandatory port.
    // Route it straight to the client: the sandbox has no meaningful "next
    // node" for a short-circuit outcome like "denied".
    let already_wired: std::collections::HashSet<String> =
        edges.iter().map(|e| e.from.clone()).collect();
    for n in &user_nodes {
        let spec =
            crate::plugins::port_spec(&n.node_type).unwrap_or(&crate::plugins::ports::DEFAULT_SPEC);
        for p in spec.outputs {
            if matches!(p.kind, crate::plugins::ports::PortKind::Error) {
                continue;
            }
            let from = format!("{}.{}", n.id, p.name);
            if !already_wired.contains(&from) {
                edges.push(EdgeConfig {
                    from,
                    to: "client.in".to_string(),
                });
            }
        }
    }

    let mut all = Vec::with_capacity(user_nodes.len() + 2);
    all.push(NodeConfig {
        id: "listener".to_string(),
        node_type: "listener".to_string(),
        config: HashMap::new(),
        config_ref: None,
        position: None,
    });
    all.extend(user_nodes);
    all.push(NodeConfig {
        id: "client".to_string(),
        node_type: "client".to_string(),
        config: HashMap::new(),
        config_ref: None,
        position: None,
    });

    Ok(PolicyConfig {
        name: "__sandbox".to_string(),
        error_handler: None,
        nodes: all,
        edges,
    })
}

/// Why a sandbox run did not happen. The Admin handler maps these to HTTP;
/// the MCP tool maps them to tool-error codes.
#[derive(Debug)]
pub enum SandboxError {
    /// `debug.enabled` is false.
    Disabled,
    /// `debug.sandbox` is false.
    SandboxDisabled,
    /// Malformed request (both/neither of `nodes`/`policy`, bad node list,
    /// invalid policy, unmaterializable context) — message is user-facing.
    BadRequest(String),
    /// `policy` names no stored policy.
    UnknownPolicy(String),
    /// The run exceeded `debug.sandbox_timeout_seconds` (the value carried).
    Timeout(u64),
}

/// A completed sandbox run.
pub struct SandboxRun {
    /// `"nodes"` or `"policy"`.
    pub mode: &'static str,
    /// The policy name executed (`__sandbox` for ad-hoc node lists).
    pub policy: String,
    /// Id of the trace stored in the debug ring buffer.
    pub stored_trace_id: String,
    /// The rendered trace (`render_trace` shape).
    pub trace: serde_json::Value,
}

/// Runs plugins or a stored policy against a synthetic request, for real,
/// recording a trace. Shared by `POST /api/debug/sandbox` and the MCP
/// `run_sandbox` tool.
pub async fn run_sandbox(
    state: &SharedState,
    req: SandboxRequest,
) -> Result<SandboxRun, SandboxError> {
    if !state.debug.enabled {
        return Err(SandboxError::Disabled);
    }
    if !state.debug.sandbox_enabled {
        return Err(SandboxError::SandboxDisabled);
    }

    let (mode, policy_name, policy) = match (req.nodes, req.policy) {
        (Some(_), Some(_)) | (None, None) => {
            return Err(SandboxError::BadRequest(
                "provide exactly one of 'nodes' or 'policy'".into(),
            ))
        }
        (Some(nodes), None) => match synthesize_policy(nodes, req.on_error) {
            Ok(p) => ("nodes", "__sandbox".to_string(), p),
            Err(e) => return Err(SandboxError::BadRequest(e)),
        },
        (None, Some(name)) => {
            let gw = state.gateway.read().await;
            match gw.policies.iter().find(|p| p.name == name) {
                // Recompile rather than reusing a route's graph: a policy with
                // no route attached is exactly the one being iterated on.
                Some(p) => ("policy", name.clone(), p.clone()),
                None => return Err(SandboxError::UnknownPolicy(name)),
            }
        }
    };

    // Resolve shared plugin configs and inline supernode references the same
    // way compile_routes does — the sandbox must never diverge from what the
    // data plane executes. Resolution runs on a synthetic single-policy
    // gateway so ad-hoc nodes and stored policies behave identically.
    let (supernodes, plugin_configs) = {
        let gw = state.gateway.read().await;
        (gw.supernodes.clone(), gw.plugin_configs.clone())
    };
    let policy =
        prepare_policy(policy, &supernodes, &plugin_configs).map_err(SandboxError::BadRequest)?;

    let graph =
        compile_policy(&policy, state.resources.clone()).map_err(SandboxError::BadRequest)?;

    let ctx = materialize_context(req.context).map_err(SandboxError::BadRequest)?;

    tracing::warn!(
        "sandbox run ({}): plugins execute for real against live resources",
        mode
    );

    let recorder = TraceRecorder::new(&ctx, state.debug.capture_options(), state.debug.max_steps);
    let started = Instant::now();
    let timeout = Duration::from_secs(state.debug.sandbox_timeout_seconds.max(1));

    let run = graph.execute_traced(ctx, recorder);
    let (out_ctx, recorder) = tokio::time::timeout(timeout, run)
        .await
        .map_err(|_| SandboxError::Timeout(state.debug.sandbox_timeout_seconds))?;

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

    Ok(SandboxRun {
        mode,
        policy: policy_name,
        stored_trace_id: id,
        trace: rendered,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(id: &str, node_type: &str) -> NodeConfig {
        NodeConfig {
            id: id.to_string(),
            node_type: node_type.to_string(),
            config: HashMap::new(),
            config_ref: None,
            position: None,
        }
    }

    fn parse(json: serde_json::Value) -> Result<SandboxRequest, serde_json::Error> {
        serde_json::from_value(json)
    }

    /// Posting `{}` must produce a runnable context — the sandbox should not
    /// demand a fully-specified request just to try one plugin.
    #[test]
    fn test_empty_input_yields_sensible_defaults() {
        let ctx = SandboxContextInput::default().into_context().unwrap();
        assert_eq!(ctx.request.method, "GET");
        assert_eq!(ctx.request.path, "/");
        assert_eq!(ctx.request.host, "sandbox.local");
        assert_eq!(ctx.request.scheme, "http");
        assert_eq!(ctx.request.remote_addr, "127.0.0.1:0");
        assert_eq!(ctx.request.protocol, Protocol::Http1);
        assert!(ctx.request.body.is_empty());
        assert_eq!(ctx.response.status_code, 0);
        assert!(ctx.errors.is_empty());
    }

    #[test]
    fn test_single_string_header_coerces_to_list() {
        let req = parse(serde_json::json!({
            "context": { "headers": { "apikey": "abc" }, "query_params": { "q": ["a", "b"] } }
        }))
        .unwrap();
        let ctx = materialize_context(req.context).unwrap();
        assert_eq!(ctx.request.headers["apikey"], vec!["abc"]);
        assert_eq!(ctx.request.query_params["q"], vec!["a", "b"]);
    }

    #[test]
    fn test_body_and_body_base64_are_mutually_exclusive() {
        let input = SandboxContextInput {
            body: Some("a".to_string()),
            body_base64: Some("YQ==".to_string()),
            ..Default::default()
        };
        assert!(input.into_context().is_err());
    }

    #[test]
    fn test_body_base64_decoded() {
        let input = SandboxContextInput {
            body_base64: Some(BASE64.encode("binary")),
            ..Default::default()
        };
        let ctx = input.into_context().unwrap();
        assert_eq!(ctx.request.body, Bytes::from_static(b"binary"));
    }

    #[test]
    fn test_response_seed_supports_response_phase_plugins() {
        let req = parse(serde_json::json!({
            "context": { "response": { "status_code": 200, "body": "hi", "headers": { "x": "1" } } }
        }))
        .unwrap();
        let ctx = materialize_context(req.context).unwrap();
        assert_eq!(ctx.response.status_code, 200);
        assert_eq!(ctx.response.body, Bytes::from_static(b"hi"));
        assert_eq!(ctx.response.headers["x"], vec!["1"]);
    }

    /// A typo must fail loudly rather than silently defaulting — now at
    /// materialisation, since `context` is taken raw and validated after
    /// normalisation.
    #[test]
    fn test_unknown_context_field_is_rejected() {
        let req = parse(serde_json::json!({ "context": { "paths": "/x" } })).unwrap();
        assert!(materialize_context(req.context).is_err());
    }

    /// A context copied straight from a trace (nested request/response, body as
    /// an object, plus `errors`) must replay without a shape error.
    #[test]
    fn test_accepts_trace_snapshot_shape() {
        let snapshot = serde_json::json!({
            "request": {
                "method": "POST",
                "path": "/api/items",
                "host": "h",
                "scheme": "http",
                "headers": { "x-consumer": ["alice"] },
                "query_params": { "page": ["2"] },
                "body": { "len": 7, "text": "payload" }
            },
            "response": {
                "status_code": 200,
                "headers": { "x-powered-by": ["php"] },
                "body": { "len": 2, "text": "ok" }
            },
            "message": { "user_id": "alice" },
            "errors": [ { "node_id": "auth", "code": "X", "message": "y" } ]
        });
        let ctx = materialize_context(snapshot).unwrap();
        assert_eq!(ctx.request.method, "POST");
        assert_eq!(ctx.request.path, "/api/items");
        assert_eq!(ctx.request.headers["x-consumer"], vec!["alice"]);
        assert_eq!(ctx.request.query_params["page"], vec!["2"]);
        assert_eq!(ctx.request.body, Bytes::from_static(b"payload"));
        assert_eq!(ctx.response.status_code, 200);
        assert_eq!(ctx.response.body, Bytes::from_static(b"ok"));
        assert_eq!(ctx.message["user_id"], serde_json::json!("alice"));
        // `errors` is display-only and must not leak into the replayed context.
        assert!(ctx.errors.is_empty());
    }

    /// A snapshot whose body was binary/uncaptured (no `text`) replays with an
    /// empty body rather than failing.
    #[test]
    fn test_snapshot_binary_or_uncaptured_body_becomes_empty() {
        let snapshot = serde_json::json!({
            "request": { "path": "/x", "body": { "len": 1024, "binary": true } }
        });
        let ctx = materialize_context(snapshot).unwrap();
        assert_eq!(ctx.request.path, "/x");
        assert!(ctx.request.body.is_empty());
    }

    #[test]
    fn test_agent_friendly_aliases_and_coercions() {
        // `query` and `uri` aliases; a JSON body given as an object; numeric
        // header/query values; `response.status`.
        let ctx = materialize_context(serde_json::json!({
            "method": "POST",
            "uri": "/orders",
            "query": {"page": 2, "tags": ["a", "b"]},
            "headers": {"x-retry": 3, "accept": ["text/plain", "application/json"]},
            "body": {"order": {"id": 42}},
            "response": {"status": 201, "body": {"ok": true}}
        }))
        .unwrap();
        assert_eq!(ctx.request.path, "/orders");
        assert_eq!(ctx.request.query_params["page"], vec!["2"]);
        assert_eq!(ctx.request.query_params["tags"], vec!["a", "b"]);
        assert_eq!(ctx.request.headers["x-retry"], vec!["3"]);
        assert_eq!(ctx.request.headers["accept"].len(), 2);
        assert_eq!(ctx.request.body.as_ref(), br#"{"order":{"id":42}}"#);
        assert_eq!(ctx.response.status_code, 201);
        assert_eq!(ctx.response.body.as_ref(), br#"{"ok":true}"#);
    }

    #[test]
    fn test_nested_request_with_plain_object_body_is_kept() {
        let ctx = materialize_context(serde_json::json!({
            "request": {"path": "/x", "body": {"a": 1}}
        }))
        .unwrap();
        assert_eq!(ctx.request.body.as_ref(), br#"{"a":1}"#);
        // A real snapshot body without text is still dropped.
        let ctx = materialize_context(serde_json::json!({
            "request": {"path": "/x", "body": {"len": 5, "binary": true}}
        }))
        .unwrap();
        assert!(ctx.request.body.is_empty());
    }

    #[test]
    fn test_synthesized_policy_chains_nodes() {
        let p = synthesize_policy(
            vec![node("a", "cors"), node("b", "proxy-rewrite")],
            OnError::Stop,
        )
        .unwrap();
        let ids: Vec<&str> = p.nodes.iter().map(|n| n.id.as_str()).collect();
        assert_eq!(ids, vec!["listener", "a", "b", "client"]);

        let edges: Vec<String> = p
            .edges
            .iter()
            .map(|e| format!("{}->{}", e.from, e.to))
            .collect();
        assert_eq!(
            edges,
            vec![
                "listener.out->a.in",
                "a.success->b.in",
                "b.success->client.in",
                // `a` (cors) has a mandatory outcome port not covered by the
                // chain wiring above — auto-wired straight to `client`.
                "a.preflight->client.in"
            ]
        );
        // The policy must satisfy the same validation the editor applies.
        assert!(crate::graph::validate_policy(&p).is_ok());
    }

    /// `on_error: "client"` wires every error port so a rejecting plugin's own
    /// status survives instead of becoming the engine's generic 500.
    #[test]
    fn test_on_error_client_wires_error_edges() {
        let p = synthesize_policy(vec![node("a", "key-auth")], OnError::Client).unwrap();
        assert!(p
            .edges
            .iter()
            .any(|e| e.from == "a.error" && e.to == "client.in"));

        let stop = synthesize_policy(vec![node("a", "key-auth")], OnError::Stop).unwrap();
        assert!(!stop.edges.iter().any(|e| e.from == "a.error"));
    }

    #[test]
    fn test_missing_id_is_defaulted_from_type() {
        let p = synthesize_policy(vec![node("", "cors")], OnError::Stop).unwrap();
        assert_eq!(p.nodes[1].id, "cors-0");
    }

    #[test]
    fn test_duplicate_ids_rejected() {
        let err = synthesize_policy(vec![node("a", "cors"), node("a", "csrf")], OnError::Stop)
            .unwrap_err();
        assert!(err.contains("duplicate node id"), "got: {err}");
    }

    #[test]
    fn test_reserved_node_types_rejected() {
        for t in ["listener", "client"] {
            let err = synthesize_policy(vec![node("x", t)], OnError::Stop).unwrap_err();
            assert!(err.contains("added automatically"), "got: {err}");
        }
    }

    #[test]
    fn test_empty_node_list_rejected() {
        assert!(synthesize_policy(Vec::new(), OnError::Stop).is_err());
    }

    #[test]
    fn test_on_error_defaults_to_stop() {
        let req = parse(serde_json::json!({ "policy": "p" })).unwrap();
        assert_eq!(req.on_error, OnError::Stop);
    }

    #[tokio::test]
    async fn run_sandbox_reports_disabled_and_unknown_policy() {
        use crate::config::{GatewayConfig, SystemConfig};
        use crate::config_store::FileConfigStore;
        use std::sync::Arc;

        let off: SystemConfig = serde_yaml::from_str("{}").unwrap();
        let gw: GatewayConfig = serde_yaml::from_str("{}").unwrap();
        let store = Arc::new(FileConfigStore::new(std::path::PathBuf::from(
            "gateway.yaml",
        )));
        let state = crate::state::SharedState::new(off, gw.clone(), None, store.clone()).unwrap();
        let req = SandboxRequest {
            policy: Some("p".into()),
            ..Default::default()
        };
        assert!(matches!(
            run_sandbox(&state, req).await,
            Err(SandboxError::Disabled)
        ));

        let on: SystemConfig = serde_yaml::from_str("debug:\n  enabled: true\n").unwrap();
        let state = crate::state::SharedState::new(on, gw, None, store).unwrap();
        let req = SandboxRequest {
            policy: Some("missing".into()),
            ..Default::default()
        };
        assert!(matches!(
            run_sandbox(&state, req).await,
            Err(SandboxError::UnknownPolicy(_))
        ));
        let req = SandboxRequest::default();
        assert!(matches!(
            run_sandbox(&state, req).await,
            Err(SandboxError::BadRequest(_))
        ));
    }

    #[tokio::test]
    async fn run_sandbox_executes_nodes_mode() {
        use crate::config::{GatewayConfig, SystemConfig};
        use crate::config_store::FileConfigStore;
        use std::sync::Arc;
        let on: SystemConfig = serde_yaml::from_str("debug:\n  enabled: true\n").unwrap();
        let gw: GatewayConfig = serde_yaml::from_str("{}").unwrap();
        let store = Arc::new(FileConfigStore::new(std::path::PathBuf::from(
            "gateway.yaml",
        )));
        let state = crate::state::SharedState::new(on, gw, None, store).unwrap();
        // `echo` requires at least one of body/before_body/after_body — an
        // empty config is rejected by EchoPlugin::from_config, so the
        // smallest accepted config (`body`) is used here instead of `{}`.
        let req: SandboxRequest = serde_json::from_value(serde_json::json!({
            "nodes": [{"id": "e", "type": "echo", "config": {"body": "hi"}}],
            "context": {"method": "GET", "path": "/x"}
        }))
        .unwrap();
        let run = run_sandbox(&state, req).await.unwrap();
        assert_eq!(run.mode, "nodes");
        assert_eq!(run.policy, "__sandbox");
        assert!(state.debug.get(&run.stored_trace_id).is_some());
        assert!(!run.trace["steps"].as_array().unwrap().is_empty());
    }
}
