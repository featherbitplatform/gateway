//! The MCP tool layer: typed functions over [`SharedState`] plus the registry
//! the server advertises. Transport-agnostic — the `rmcp` adapter in
//! `server.rs` and the Admin API's prompt renderer both call into here.

pub mod catalog;
pub mod config;
pub mod debug;
pub mod writes;

use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::config::McpScope;
use crate::state::SharedState;

/// A JSON object, as MCP tool arguments arrive.
pub type JsonObject = serde_json::Map<String, Value>;

/// A domain failure surfaced to the agent as an MCP tool error (never a
/// JSON-RPC error): `{code, message, errors?, hint?}`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolError {
    pub code: &'static str,
    pub message: String,
    pub errors: Vec<String>,
    pub hint: Option<String>,
}

impl ToolError {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            errors: Vec::new(),
            hint: None,
        }
    }
    fn with_hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }
    pub fn not_found(what: &str, name: &str) -> Self {
        Self::new("not_found", format!("{what} '{name}' does not exist"))
    }
    pub fn invalid_input(msg: impl Into<String>) -> Self {
        Self::new("invalid_input", msg)
    }
    pub fn invalid_config(errors: Vec<String>) -> Self {
        let mut e = Self::new("invalid_config", "the configuration failed validation");
        e.errors = errors;
        e.with_hint(
            "Every success/outcome port must be wired. Use get_node_type(<type>) to see a node's ports and config keys.",
        )
    }
    pub fn debug_disabled() -> Self {
        Self::new(
            "debug_disabled",
            "debug mode is off; traces and the sandbox are unavailable",
        )
        .with_hint("set debug.enabled: true in system.yaml (or FEATHERBIT_DEBUG=true) and restart")
    }
    pub fn sandbox_disabled() -> Self {
        Self::new("sandbox_disabled", "the plugin sandbox is disabled")
            .with_hint("set debug.sandbox: true in system.yaml and restart")
    }
    // Only raised by the scope check in `src/mcp/server.rs`.
    #[cfg_attr(not(feature = "mcp"), allow(dead_code))]
    pub fn forbidden(have: McpScope) -> Self {
        Self::new("forbidden", "this token may not use write tools").with_hint(format!(
            "this token has scope {}; write tools need a token with scope write. Return the YAML for a human to apply instead.",
            have.as_str()
        ))
    }
    /// `reload_config` would revert live edits that were never written to
    /// gateway.yaml; `errors` lists them.
    pub fn unsaved_changes(pending: Vec<String>) -> Self {
        let mut e = Self::new(
            "unsaved_changes",
            "the live config has edits that are not in gateway.yaml; reloading would discard them",
        );
        e.errors = pending;
        e.with_hint(
            "put_*/delete_* changes are already live — no reload is needed. To really revert to the file, call reload_config with discard_unsaved=true; to keep the edits, ask the operator to write them to gateway.yaml (export_config gives the YAML).",
        )
    }
    pub fn store_error(msg: impl Into<String>) -> Self {
        Self::new("store_error", msg)
    }
    pub fn unknown_tool(name: &str) -> Self {
        Self::new("unknown_tool", format!("no tool named '{name}'"))
    }
    pub fn internal(msg: impl Into<String>) -> Self {
        Self::new("internal", msg)
    }
    /// The body of the `isError` tool result. Only called from
    /// `src/mcp/server.rs` (outside `#[cfg(test)]`, which uses it too).
    #[cfg_attr(not(feature = "mcp"), allow(dead_code))]
    pub fn to_json(&self) -> Value {
        let mut v = serde_json::json!({"code": self.code, "message": self.message});
        if !self.errors.is_empty() {
            v["errors"] = serde_json::json!(self.errors);
        }
        if let Some(h) = &self.hint {
            v["hint"] = Value::String(h.clone());
        }
        v
    }
}

/// Deserializes tool arguments, reporting schema mismatches as `invalid_input`.
pub fn args<T: DeserializeOwned>(a: JsonObject) -> Result<T, ToolError> {
    serde_json::from_value(Value::Object(a))
        .map_err(|e| ToolError::invalid_input(format!("invalid arguments: {e}")))
}

/// Accepts a payload either as a JSON object or as a YAML document in a
/// string — agents naturally write the YAML the docs show.
pub fn parse_payload<T: DeserializeOwned>(v: Value, what: &str) -> Result<T, ToolError> {
    match v {
        Value::String(yaml) => serde_yaml::from_str(&yaml)
            .map_err(|e| ToolError::invalid_input(format!("{what}: YAML did not parse: {e}"))),
        other => serde_json::from_value(other).map_err(|e| {
            ToolError::invalid_input(format!("{what}: JSON did not deserialize: {e}"))
        }),
    }
}

/// JSON Schema (draft 2020-12, as schemars 1 emits) for a tool's arguments,
/// with the `$schema`/`title` noise removed. Only used to build `TOOLS`
/// below, which is `mcp`-only (the Admin API exposes prompts, not tools).
#[cfg_attr(not(feature = "mcp"), allow(dead_code))]
pub fn schema_of<T: schemars::JsonSchema>() -> JsonObject {
    let schema = schemars::schema_for!(T);
    let mut v = serde_json::to_value(schema).expect("schema serializes");
    let obj = v.as_object_mut().expect("schema is an object");
    obj.remove("$schema");
    obj.remove("title");
    obj.clone()
}

/// Static description of one tool. The MCP tool registry, advertised only
/// by the `mcp` transport (`src/mcp/server.rs`) — the Admin API exposes
/// prompts, not tools.
#[cfg_attr(not(feature = "mcp"), allow(dead_code))]
pub struct ToolDef {
    pub name: &'static str,
    pub scope: McpScope,
    pub description: &'static str,
    pub input_schema: fn() -> JsonObject,
}

/// Every tool, in the order clients see them.
#[cfg_attr(not(feature = "mcp"), allow(dead_code))]
pub fn tool_defs() -> &'static [ToolDef] {
    &TOOLS
}

/// Looks up a tool by name.
#[cfg_attr(not(feature = "mcp"), allow(dead_code))]
pub fn tool_def(name: &str) -> Option<&'static ToolDef> {
    TOOLS.iter().find(|t| t.name == name)
}

/// Executes a tool. Scope is **not** checked here (see `server.rs`).
pub async fn call(state: &SharedState, name: &str, a: JsonObject) -> Result<Value, ToolError> {
    match name {
        "list_node_types" => catalog::list_node_types().await,
        "get_node_type" => catalog::get_node_type(args(a)?).await,
        "list_vars" => catalog::list_vars().await,
        "get_status" => catalog::get_status(state).await,
        "export_config" => catalog::export_config(state).await,
        "list_routes" => config::list_routes(state).await,
        "get_route" => config::get_route(state, args(a)?).await,
        "list_policies" => config::list_policies(state).await,
        "get_policy" => config::get_policy(state, args(a)?).await,
        "list_supernodes" => config::list_supernodes(state).await,
        "get_supernode" => config::get_supernode(state, args(a)?).await,
        "list_plugin_configs" => config::list_plugin_configs(state).await,
        "get_plugin_config" => config::get_plugin_config(state, args(a)?).await,
        "list_stores" => config::list_stores(state).await,
        "list_consumers" => config::list_consumers(state).await,
        "get_consumer" => config::get_consumer(state, args(a)?).await,
        "validate_policy" => config::validate_policy(state, args(a)?).await,
        "validate_supernode" => config::validate_supernode(args(a)?).await,
        "list_traces" => debug::list_traces(state, args(a)?).await,
        "get_trace" => debug::get_trace(state, args(a)?).await,
        "get_trace_step" => debug::get_trace_step(state, args(a)?).await,
        "run_sandbox" => debug::run_sandbox_tool(state, Value::Object(a)).await,
        "put_route" => writes::put_route(state, args(a)?).await,
        "delete_route" => writes::delete_route(state, args(a)?).await,
        "put_policy" => writes::put_policy(state, args(a)?).await,
        "delete_policy" => writes::delete_policy(state, args(a)?).await,
        "put_supernode" => writes::put_supernode(state, args(a)?).await,
        "delete_supernode" => writes::delete_supernode(state, args(a)?).await,
        "put_plugin_config" => writes::put_plugin_config(state, args(a)?).await,
        "delete_plugin_config" => writes::delete_plugin_config(state, args(a)?).await,
        "put_store" => writes::put_store(state, args(a)?).await,
        "delete_store" => writes::delete_store(state, args(a)?).await,
        "reload_config" => writes::reload_config(state, args(a)?).await,
        _ => Err(ToolError::unknown_tool(name)),
    }
}

use McpScope::Read;
use McpScope::Write;

#[cfg_attr(not(feature = "mcp"), allow(dead_code))]
static TOOLS: [ToolDef; 33] = [
    ToolDef { name: "list_node_types", scope: Read, description: "List every node (plugin) type with its description and declared ports. Start here when designing a policy.", input_schema: schema_of::<catalog::NoArgs> },
    ToolDef { name: "get_node_type", scope: Read, description: "Full reference for one node type: description, input/output ports (which must be wired), and its documentation page with every config key and a YAML example.", input_schema: schema_of::<catalog::TypeArgs> },
    ToolDef { name: "list_vars", scope: Read, description: "The $var catalog usable in plugin config (e.g. $remote_addr, $http_<header>, $consumer_name) with examples.", input_schema: schema_of::<catalog::NoArgs> },
    ToolDef { name: "get_status", scope: Read, description: "Gateway version, route/policy counts, and whether debug mode and the sandbox are on.", input_schema: schema_of::<catalog::NoArgs> },
    ToolDef { name: "export_config", scope: Read, description: "The whole gateway.yaml as YAML text (routes, policies, supernodes, plugin_configs, stores, consumers). ${ENV} placeholders stay unresolved. Consumer credential secrets are masked (as in list_consumers/get_consumer).", input_schema: schema_of::<catalog::NoArgs> },
    ToolDef { name: "list_routes", scope: Read, description: "All routes: name, match rule (path/methods/host/headers) and the policy each references.", input_schema: schema_of::<catalog::NoArgs> },
    ToolDef { name: "get_route", scope: Read, description: "One route by name.", input_schema: schema_of::<config::NameArgs> },
    ToolDef { name: "list_policies", scope: Read, description: "All policies (node graphs) with their nodes and edges.", input_schema: schema_of::<catalog::NoArgs> },
    ToolDef { name: "get_policy", scope: Read, description: "One policy by name, plus the routes that reference it.", input_schema: schema_of::<config::NameArgs> },
    ToolDef { name: "list_supernodes", scope: Read, description: "All supernode definitions (reusable subgraphs with input/output/error boundary nodes).", input_schema: schema_of::<catalog::NoArgs> },
    ToolDef { name: "get_supernode", scope: Read, description: "One supernode definition by name, plus the policies that use it.", input_schema: schema_of::<config::NameArgs> },
    ToolDef { name: "list_plugin_configs", scope: Read, description: "Named shared plugin config profiles referenced by nodes via config_ref.", input_schema: schema_of::<catalog::NoArgs> },
    ToolDef { name: "get_plugin_config", scope: Read, description: "One plugin config profile by name.", input_schema: schema_of::<config::NameArgs> },
    ToolDef { name: "list_stores", scope: Read, description: "Named redis/valkey stores referenced by plugin config (`store:`) and sessions.", input_schema: schema_of::<catalog::NoArgs> },
    ToolDef { name: "list_consumers", scope: Read, description: "API consumers (name, group, labels, credential kinds). Credential secrets are masked.", input_schema: schema_of::<catalog::NoArgs> },
    ToolDef { name: "get_consumer", scope: Read, description: "One consumer by name; credential secrets are masked.", input_schema: schema_of::<config::NameArgs> },
    ToolDef { name: "validate_policy", scope: Read, description: "Validate and compile an unsaved policy (JSON object or YAML string) against the live gateway: structure, port wiring, config_ref/store references, and every node's config. Returns {valid, errors}. Persists nothing.", input_schema: schema_of::<config::ValidatePolicyArgs> },
    ToolDef { name: "validate_supernode", scope: Read, description: "Structurally validate an unsaved supernode definition (boundary nodes, reserved ids, inner wiring). Node config errors surface when a policy using it is validated or saved with dry_run.", input_schema: schema_of::<config::ValidateSupernodeArgs> },
    ToolDef { name: "list_traces", scope: Read, description: "Recent debug traces (newest first): id, route, policy, method, path, status, step and error counts. Filter by route/policy/status/source. Requires debug.enabled.", input_schema: schema_of::<debug::ListTracesArgs> },
    ToolDef { name: "get_trace", scope: Read, description: "One trace: the request, final response, and every node step with outcome, exit port, edge taken and the context changes it made. Snapshots omitted unless include_snapshots.", input_schema: schema_of::<debug::GetTraceArgs> },
    ToolDef { name: "get_trace_step", scope: Read, description: "One step of a trace in full: context before and after the node, the diff, outcome/port, and the node's stored config. Use to answer 'why did this node exit on this port?'.", input_schema: schema_of::<debug::GetTraceStepArgs> },
    ToolDef { name: "run_sandbox", scope: Read, description: "Run a stored policy or an ad-hoc node list against a synthetic request, for real (outbound calls happen), and get the resulting trace. Requires debug.enabled and debug.sandbox.", input_schema: schema_of::<debug::SandboxArgs> },
    ToolDef { name: "put_route", scope: Write, description: "Create or replace a route {match: {path, methods?, host?, headers?}, policy}. Set dry_run=true first to validate the whole resulting config without applying.", input_schema: schema_of::<writes::PutArgs> },
    ToolDef { name: "delete_route", scope: Write, description: "Delete a route by name (dry_run supported).", input_schema: schema_of::<writes::DeleteArgs> },
    ToolDef { name: "put_policy", scope: Write, description: "Create or replace a policy {nodes, edges, error_handler?}. Every success/outcome port must be wired. Use dry_run=true first.", input_schema: schema_of::<writes::PutArgs> },
    ToolDef { name: "delete_policy", scope: Write, description: "Delete a policy by name; fails while a route still references it (dry_run supported).", input_schema: schema_of::<writes::DeleteArgs> },
    ToolDef { name: "put_supernode", scope: Write, description: "Create or replace a supernode definition {nodes, edges, description?} with input/output/error boundary nodes. Use dry_run=true first.", input_schema: schema_of::<writes::PutArgs> },
    ToolDef { name: "delete_supernode", scope: Write, description: "Delete a supernode by name; fails while a policy still uses it (dry_run supported).", input_schema: schema_of::<writes::DeleteArgs> },
    ToolDef { name: "put_plugin_config", scope: Write, description: "Create or replace a shared plugin config profile {type, config, description?} referenced by nodes via config_ref.", input_schema: schema_of::<writes::PutArgs> },
    ToolDef { name: "delete_plugin_config", scope: Write, description: "Delete a plugin config profile by name; fails while referenced (dry_run supported).", input_schema: schema_of::<writes::DeleteArgs> },
    ToolDef { name: "put_store", scope: Write, description: "Create or replace a redis/valkey store {type, url, password?, key_prefix?, tls?}. Keep secrets as ${ENV_VAR} placeholders.", input_schema: schema_of::<writes::PutArgs> },
    ToolDef { name: "delete_store", scope: Write, description: "Delete a store by name; fails with the list of referrers while in use (dry_run supported).", input_schema: schema_of::<writes::DeleteArgs> },
    ToolDef { name: "reload_config", scope: Write, description: "Re-read gateway.yaml from disk and apply it (file config source only). NOT needed after put_*/delete_* — those are live immediately. Use it only when the file was edited by hand: it DISCARDS every API/MCP edit that was never written to the file, so it refuses with `unsaved_changes` (listing what would be lost) unless discard_unsaved=true.", input_schema: schema_of::<writes::ReloadArgs> },
];

#[cfg(test)]
pub(crate) mod test_support {
    use std::sync::Arc;

    use super::*;
    use crate::config::{GatewayConfig, SystemConfig};
    use crate::config_store::FileConfigStore;

    /// A state with the given system/gateway YAML (both default to `{}`).
    pub fn state(system_yaml: &str, gateway_yaml: &str) -> Arc<SharedState> {
        let system: SystemConfig = serde_yaml::from_str(system_yaml).unwrap();
        let gateway: GatewayConfig = serde_yaml::from_str(gateway_yaml).unwrap();
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

    /// A minimal valid gateway: one route → one echo policy.
    pub const ECHO_GATEWAY: &str = r#"
routes:
  - name: hello
    match: { path: /hello }
    policy: echo-policy
policies:
  - name: echo-policy
    nodes:
      - { id: l, type: listener }
      - { id: e, type: echo, config: { body: hi } }
      - { id: c, type: client }
    edges:
      - { from: l.out, to: e.in }
      - { from: e.out, to: c.in }
"#;

    pub fn obj(v: Value) -> JsonObject {
        v.as_object().cloned().unwrap()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_names_are_unique_and_scopes_follow_naming() {
        let mut seen = std::collections::HashSet::new();
        for t in tool_defs() {
            assert!(seen.insert(t.name), "duplicate tool {}", t.name);
            let mutating = t.name.starts_with("put_")
                || t.name.starts_with("delete_")
                || t.name == "reload_config";
            assert_eq!(
                t.scope == Write,
                mutating,
                "scope/name mismatch for {}",
                t.name
            );
            assert!(!t.description.is_empty());
            let schema = (t.input_schema)();
            assert_eq!(
                schema.get("type").and_then(Value::as_str),
                Some("object"),
                "{}",
                t.name
            );
        }
    }

    #[tokio::test]
    async fn unknown_tool_is_reported() {
        let state = test_support::state("{}", "{}");
        let err = call(&state, "nope", JsonObject::new()).await.unwrap_err();
        assert_eq!(err.code, "unknown_tool");
    }

    #[test]
    fn payload_accepts_yaml_or_json() {
        let r: crate::config::RouteConfig = parse_payload(
            Value::String("name: r\nmatch: {path: /x}\npolicy: p\n".into()),
            "route",
        )
        .unwrap();
        assert_eq!(r.name, "r");
        let r: crate::config::RouteConfig = parse_payload(
            serde_json::json!({"name": "r2", "match": {"path": "/y"}, "policy": "p"}),
            "route",
        )
        .unwrap();
        assert_eq!(r.name, "r2");
        let err =
            parse_payload::<crate::config::RouteConfig>(Value::String("name: [".into()), "route")
                .unwrap_err();
        assert_eq!(err.code, "invalid_input");
        assert!(err.message.contains("YAML"));
    }

    #[test]
    fn error_json_shape() {
        let e = ToolError::invalid_config(vec!["a".into(), "b".into()]);
        let v = e.to_json();
        assert_eq!(v["code"], "invalid_config");
        assert_eq!(v["errors"].as_array().unwrap().len(), 2);
        assert!(v["hint"].as_str().unwrap().contains("get_node_type"));
        assert!(ToolError::not_found("policy", "x")
            .to_json()
            .get("errors")
            .is_none());
    }
}
