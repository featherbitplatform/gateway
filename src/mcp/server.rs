//! `rmcp` adapter: exposes the tool/resource/prompt layer over MCP and builds
//! the Streamable HTTP tower service mounted on the admin router.

use std::sync::Arc;
use std::time::Instant;

use rmcp::handler::server::ServerHandler;
use rmcp::model::*;
use rmcp::service::RequestContext;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use rmcp::{ErrorData as McpError, RoleServer};

use crate::mcp::auth::McpPrincipal;
use crate::mcp::tools::{self, JsonObject, ToolError};
use crate::mcp::{docs, prompts};
use crate::state::SharedState;

/// Orientation sent to every client at `initialize`.
const INSTRUCTIONS: &str = "\
featherbit is an API gateway whose behavior is declared as node-graph POLICIES referenced by ROUTES.
- A policy is a graph of typed nodes (plugins) joined by edges `from: node.port` → `to: node.in`. It has exactly one `listener` (entry) and one `client` (exit).
- Every node's `success`/`out` port AND every declared outcome port (`denied`, `redirect`, `limited`, `broken`, `preflight`, `abort`, `routed`, `hit`, `true`/`false`, …) MUST be wired, or the policy fails to compile. Only `error` ports may be left unwired (they fall back to the policy's `error_handler`).
- Plugin config keys are documented per node type: call `get_node_type(<type>)` before writing a node's `config`. `list_node_types` lists them all; `list_vars` lists the `$var` names usable inside config.
- Authoring loop: get_node_type → write YAML → validate_policy → put_*(dry_run=true) → put_*. Payloads may be JSON objects or YAML strings.
- Debugging: list_traces / get_trace / get_trace_step (context before/after a node, its exit port, the diff) and run_sandbox (execute a policy against a synthetic request). They need `debug.enabled` in system.yaml; the tools tell you if it is off.
- Supernodes are reusable subgraphs with input/output/error boundary nodes; `featherbit://docs/concepts/supernodes` explains the rules.
- `${ENV_VAR}` placeholders in config are intentional and stay unresolved; never replace them with literal secrets.
- If your token is read-only, write tools are hidden (or return `forbidden`): finish by returning validated YAML for a human to apply.
Use the prompts (troubleshoot_trace, explain_trace, why_this_port, why_this_response, review_policy, design_policy, design_supernode, design_route, diagnose_route) for the common questions.";

/// The MCP server: a cheap handle over the shared state.
#[derive(Clone)]
pub struct McpServer {
    state: Arc<SharedState>,
}

impl McpServer {
    pub fn new(state: Arc<SharedState>) -> Self {
        Self { state }
    }
}

/// Builds the tower service to mount at `admin.mcp.path`. Host validation is
/// disabled (agents reach the admin listener by any name; our own middleware
/// enforces `Origin`), sessions are rmcp's default in-memory manager.
pub fn build_service(
    state: Arc<SharedState>,
) -> StreamableHttpService<McpServer, LocalSessionManager> {
    let server = McpServer::new(state);
    StreamableHttpService::new(
        move || Ok(server.clone()),
        Arc::new(LocalSessionManager::default()),
        StreamableHttpServerConfig::default()
            .disable_allowed_hosts()
            .disable_allowed_origins(),
    )
}

/// The principal the bearer middleware attached to this HTTP request.
fn principal(ctx: &RequestContext<RoleServer>) -> Result<McpPrincipal, McpError> {
    ctx.extensions
        .get::<http::request::Parts>()
        .and_then(|parts| parts.extensions.get::<McpPrincipal>().cloned())
        .ok_or_else(|| {
            McpError::invalid_request(
                "request carries no MCP principal (auth middleware missing)",
                None,
            )
        })
}

fn tool_from_def(def: &tools::ToolDef) -> Tool {
    Tool::new(def.name, def.description, Arc::new((def.input_schema)()))
}

fn error_result(e: &ToolError) -> CallToolResult {
    CallToolResult::error(vec![ContentBlock::text(e.to_json().to_string())])
}

fn tools_obj(v: serde_json::Value) -> JsonObject {
    v.as_object().cloned().unwrap_or_default()
}

impl ServerHandler for McpServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_resources()
                .enable_prompts()
                .build(),
        )
        .with_server_info(Implementation::new("featherbit", env!("CARGO_PKG_VERSION")))
        .with_instructions(INSTRUCTIONS)
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        let p = principal(&ctx)?;
        let tools = tools::tool_defs()
            .iter()
            .filter(|d| p.scope.allows(d.scope))
            .map(tool_from_def)
            .collect();
        // SEP-2549 cache hints are REQUIRED on list results for peers on
        // protocol 2026-07-28+ (Claude Code rejects the list without them).
        // Every list here is token-scoped (tools filtered by scope, resources
        // by config), so: private, and `0` = do not cache.
        Ok(ListToolsResult::with_all_items(tools)
            .with_ttl_ms(0)
            .with_cache_scope(CacheScope::Private))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, McpError> {
        let p = principal(&ctx)?;
        let name = request.name.to_string();
        let def = tools::tool_def(&name)
            .ok_or_else(|| McpError::invalid_params(format!("tool not found: {name}"), None))?;
        let started = Instant::now();
        let outcome = if !p.scope.allows(def.scope) {
            Err(ToolError::forbidden(p.scope))
        } else {
            let args: JsonObject = request.arguments.unwrap_or_default();
            tools::call(&self.state, &name, args).await
        };
        tracing::info!(
            "mcp tool call token={} scope={} tool={} outcome={} duration_ms={}",
            p.name.as_deref().unwrap_or("unnamed"),
            p.scope.as_str(),
            name,
            match &outcome {
                Ok(_) => "ok".to_string(),
                Err(e) => format!("error:{}", e.code),
            },
            started.elapsed().as_millis()
        );
        let result = match outcome {
            Ok(v) => CallToolResult::success(vec![ContentBlock::text(v.to_string())]),
            Err(e) => error_result(&e),
        };
        Ok(result.into())
    }

    async fn list_resources(
        &self,
        _request: Option<PaginatedRequestParams>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, McpError> {
        principal(&ctx)?;
        let mut resources: Vec<Resource> = docs::list_pages()
            .into_iter()
            .map(|p| {
                let mut r = Resource::new(p.uri, p.title);
                if !p.description.is_empty() {
                    r = r.with_description(p.description);
                }
                r
            })
            .collect();
        let gw = self.state.gateway.read().await;
        for r in &gw.routes {
            resources.push(Resource::new(
                format!("featherbit://routes/{}", r.name),
                format!("route {}", r.name),
            ));
        }
        for p in &gw.policies {
            resources.push(Resource::new(
                format!("featherbit://policies/{}", p.name),
                format!("policy {}", p.name),
            ));
        }
        for s in &gw.supernodes {
            resources.push(Resource::new(
                format!("featherbit://supernodes/{}", s.name),
                format!("supernode {}", s.name),
            ));
        }
        Ok(ListResourcesResult {
            resources,
            ttl_ms: Some(0),
            cache_scope: Some(CacheScope::Private),
            ..Default::default()
        })
    }

    async fn list_resource_templates(
        &self,
        _request: Option<PaginatedRequestParams>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<ListResourceTemplatesResult, McpError> {
        principal(&ctx)?;
        let resource_templates = vec![
            ResourceTemplate::new(
                "featherbit://docs/plugins/{type}",
                "Node type documentation",
            ),
            ResourceTemplate::new("featherbit://docs/concepts/{name}", "Concept guide"),
            ResourceTemplate::new(
                "featherbit://docs/reference/{name}",
                "Reference page (context-vars, conditions, templates)",
            ),
            ResourceTemplate::new("featherbit://routes/{name}", "Route definition (YAML)"),
            ResourceTemplate::new("featherbit://policies/{name}", "Policy definition (YAML)"),
            ResourceTemplate::new(
                "featherbit://supernodes/{name}",
                "Supernode definition (YAML)",
            ),
            ResourceTemplate::new(
                "featherbit://traces/{id}",
                "Debug trace (JSON, with snapshots)",
            ),
        ];
        Ok(ListResourceTemplatesResult {
            resource_templates,
            ttl_ms: Some(0),
            cache_scope: Some(CacheScope::Private),
            ..Default::default()
        })
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        ctx: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResponse, McpError> {
        principal(&ctx)?;
        let uri = request.uri.clone();
        let not_found = || {
            McpError::resource_not_found(
                "resource_not_found",
                Some(serde_json::json!({"uri": uri})),
            )
        };

        let (text, mime) = if let Some(md) = docs::read_uri(&request.uri) {
            (md, "text/markdown")
        } else if let Some(name) = request.uri.strip_prefix("featherbit://routes/") {
            let gw = self.state.gateway.read().await;
            let r = gw
                .routes
                .iter()
                .find(|r| r.name == name)
                .ok_or_else(not_found)?;
            (
                serde_yaml::to_string(r)
                    .map_err(|e| McpError::internal_error(e.to_string(), None))?,
                "application/yaml",
            )
        } else if let Some(name) = request.uri.strip_prefix("featherbit://policies/") {
            let gw = self.state.gateway.read().await;
            let p = gw
                .policies
                .iter()
                .find(|p| p.name == name)
                .ok_or_else(not_found)?;
            (
                serde_yaml::to_string(p)
                    .map_err(|e| McpError::internal_error(e.to_string(), None))?,
                "application/yaml",
            )
        } else if let Some(name) = request.uri.strip_prefix("featherbit://supernodes/") {
            let gw = self.state.gateway.read().await;
            let s = gw
                .supernodes
                .iter()
                .find(|s| s.name == name)
                .ok_or_else(not_found)?;
            (
                serde_yaml::to_string(s)
                    .map_err(|e| McpError::internal_error(e.to_string(), None))?,
                "application/yaml",
            )
        } else if let Some(id) = request.uri.strip_prefix("featherbit://traces/") {
            let v = tools::call(
                &self.state,
                "get_trace",
                tools_obj(serde_json::json!({"id": id, "include_snapshots": true})),
            )
            .await
            .map_err(|e| match e.code {
                "not_found" => not_found(),
                _ => {
                    let body = e.to_json();
                    McpError::internal_error(e.message, Some(body))
                }
            })?;
            (v.to_string(), "application/json")
        } else {
            return Err(not_found());
        };
        Ok(ReadResourceResult::new(vec![
            ResourceContents::text(text, request.uri).with_mime_type(mime)
        ])
        .into())
    }

    async fn list_prompts(
        &self,
        _request: Option<PaginatedRequestParams>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<ListPromptsResult, McpError> {
        principal(&ctx)?;
        let prompts = prompts::prompt_defs()
            .iter()
            .map(|d| {
                Prompt::new(
                    d.name,
                    Some(d.description),
                    Some(
                        d.args
                            .iter()
                            .map(|a| {
                                PromptArgument::new(a.name)
                                    .with_description(a.description)
                                    .with_required(a.required)
                            })
                            .collect(),
                    ),
                )
            })
            .collect();
        Ok(ListPromptsResult {
            prompts,
            ttl_ms: Some(0),
            cache_scope: Some(CacheScope::Private),
            ..Default::default()
        })
    }

    async fn get_prompt(
        &self,
        request: GetPromptRequestParams,
        ctx: RequestContext<RoleServer>,
    ) -> Result<GetPromptResponse, McpError> {
        principal(&ctx)?;
        // Spec-compliant clients send string values; accept anything and stringify.
        let args: std::collections::HashMap<String, String> = request
            .arguments
            .unwrap_or_default()
            .into_iter()
            .map(|(k, v)| {
                (
                    k,
                    match v {
                        serde_json::Value::String(s) => s,
                        other => other.to_string(),
                    },
                )
            })
            .collect();
        let rendered = prompts::render(&self.state, &request.name, &args)
            .await
            .map_err(|e| match e.code {
                "unknown_prompt" | "invalid_input" => McpError::invalid_params(e.message, None),
                _ => {
                    let body = e.to_json();
                    McpError::internal_error(e.message, Some(body))
                }
            })?;
        Ok(
            GetPromptResult::new(vec![PromptMessage::new_text(Role::User, rendered.text)])
                .with_description(rendered.description)
                .into(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AdminConfig;
    use crate::mcp::tools::test_support::{obj, state, ECHO_GATEWAY};
    use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
    use rmcp::transport::StreamableHttpClientTransport;
    use rmcp::ServiceExt;

    const READ: &str = "read-token-0123456789";
    const WRITE: &str = "write-token-0123456789";

    fn admin(enabled: bool) -> AdminConfig {
        serde_yaml::from_str(&format!(
            "username: u\npassword: p\nui_enabled: false\nmcp:\n  enabled: {enabled}\n  tokens:\n    - token: {READ}\n      scope: read\n      name: reader\n    - token: {WRITE}\n      scope: write\n"
        ))
        .unwrap()
    }

    /// Serves the admin router on a loopback port; returns the MCP URL.
    async fn serve(enabled: bool, debug: bool) -> (String, Arc<SharedState>) {
        let sys = if debug {
            "debug:\n  enabled: true\n  sandbox: true\n"
        } else {
            "{}"
        };
        let st = state(sys, ECHO_GATEWAY);
        let app = crate::admin::build_router(&admin(enabled), st.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (format!("http://{addr}/mcp"), st)
    }

    async fn client(
        url: &str,
        token: &str,
    ) -> rmcp::service::RunningService<rmcp::RoleClient, ClientInfo> {
        let cfg = StreamableHttpClientTransportConfig::with_uri(url.to_string()).auth_header(token);
        let transport = StreamableHttpClientTransport::from_config(cfg);
        ClientInfo::new(
            ClientCapabilities::default(),
            Implementation::new("test", "0"),
        )
        .serve(transport)
        .await
        .expect("initialize")
    }

    fn text_of(r: &CallToolResult) -> serde_json::Value {
        let s = r
            .content
            .iter()
            .find_map(|c| c.as_text().map(|t| t.text.clone()))
            .expect("text content");
        serde_json::from_str(&s).unwrap()
    }

    #[tokio::test]
    async fn initialize_lists_and_filters_tools_by_scope() {
        let (url, _) = serve(true, false).await;
        let reader = client(&url, READ).await;
        let info = reader.peer_info().unwrap();
        assert_eq!(info.server_info.as_ref().unwrap().name, "featherbit");
        assert!(info
            .instructions
            .as_deref()
            .unwrap()
            .contains("MUST be wired"));
        let names: Vec<String> = reader
            .list_all_tools()
            .await
            .unwrap()
            .into_iter()
            .map(|t| t.name.to_string())
            .collect();
        assert!(names.contains(&"get_policy".to_string()));
        assert!(!names.iter().any(|n| n.starts_with("put_")), "{names:?}");
        reader.cancel().await.unwrap();

        let writer = client(&url, WRITE).await;
        let names: Vec<String> = writer
            .list_all_tools()
            .await
            .unwrap()
            .into_iter()
            .map(|t| t.name.to_string())
            .collect();
        assert!(names.contains(&"put_policy".to_string()));
        writer.cancel().await.unwrap();
    }

    #[tokio::test]
    async fn read_parity_and_forbidden_write() {
        let (url, st) = serve(true, false).await;
        let reader = client(&url, READ).await;
        let r = reader
            .call_tool(
                CallToolRequestParams::new("get_policy")
                    .with_arguments(obj(serde_json::json!({"name": "echo-policy"}))),
            )
            .await
            .unwrap();
        assert_ne!(r.is_error, Some(true));
        let v = text_of(&r);
        let expected = serde_json::to_value(st.gateway.read().await.policies[0].clone()).unwrap();
        assert_eq!(v["policy"], expected);

        let r = reader
            .call_tool(
                CallToolRequestParams::new("put_policy")
                    .with_arguments(obj(serde_json::json!({"name": "x", "definition": {}}))),
            )
            .await
            .unwrap();
        assert_eq!(r.is_error, Some(true));
        assert_eq!(text_of(&r)["code"], "forbidden");

        let err = reader
            .call_tool(CallToolRequestParams::new("no_such_tool"))
            .await;
        assert!(err.is_err(), "unknown tool is a protocol error");
        reader.cancel().await.unwrap();
    }

    #[tokio::test]
    async fn write_dry_run_then_apply_is_visible_and_routable() {
        let (url, st) = serve(true, false).await;
        let writer = client(&url, WRITE).await;
        let def = serde_json::json!({
            "nodes": [
                {"id": "l", "type": "listener"},
                {"id": "e", "type": "echo", "config": {"body": "hi"}},
                {"id": "c", "type": "client"}
            ],
            "edges": [{"from": "l.out", "to": "e.in"}, {"from": "e.out", "to": "c.in"}]
        });
        let r = writer
            .call_tool(CallToolRequestParams::new("put_policy").with_arguments(obj(
                serde_json::json!({"name": "p2", "definition": def, "dry_run": true}),
            )))
            .await
            .unwrap();
        assert_eq!(text_of(&r)["applied"], false);
        assert!(st
            .gateway
            .read()
            .await
            .policies
            .iter()
            .all(|p| p.name != "p2"));

        let r = writer
            .call_tool(
                CallToolRequestParams::new("put_policy")
                    .with_arguments(obj(serde_json::json!({"name": "p2", "definition": def}))),
            )
            .await
            .unwrap();
        assert_eq!(text_of(&r)["applied"], true);
        writer
            .call_tool(CallToolRequestParams::new("put_route").with_arguments(obj(
                serde_json::json!({"name": "r2", "definition": {"match": {"path": "/two"}, "policy": "p2"}}),
            )))
            .await
            .unwrap();
        assert_eq!(
            st.routes.read().await.len(),
            2,
            "hot-applied to the route table"
        );

        // key-auth declares a `denied` outcome port; leaving it unwired must
        // fail compilation rather than silently dropping rejected requests.
        let bad = serde_json::json!({
            "nodes": [
                {"id": "l", "type": "listener"},
                {"id": "k", "type": "key-auth", "config": {"keys": ["k1"]}},
                {"id": "c", "type": "client"}
            ],
            "edges": [{"from": "l.out", "to": "k.in"}, {"from": "k.out", "to": "c.in"}]
        });
        let r = writer
            .call_tool(
                CallToolRequestParams::new("put_policy")
                    .with_arguments(obj(serde_json::json!({"name": "bad", "definition": bad}))),
            )
            .await
            .unwrap();
        assert_eq!(r.is_error, Some(true));
        let v = text_of(&r);
        assert_eq!(v["code"], "invalid_config");
        assert!(v["errors"][0].as_str().unwrap().contains("bad"));
        writer.cancel().await.unwrap();
    }

    #[tokio::test]
    async fn resources_and_prompts() {
        let (url, _) = serve(true, true).await;
        let c = client(&url, READ).await;
        // 2026-07-28 peers (Claude Code) require SEP-2549 cache hints on every list.
        let tl = c.list_tools(None).await.unwrap();
        assert_eq!(
            (tl.ttl_ms, tl.cache_scope),
            (Some(0), Some(CacheScope::Private))
        );
        let tpl = c.list_resource_templates(None).await.unwrap();
        assert_eq!(
            (tpl.ttl_ms, tpl.cache_scope),
            (Some(0), Some(CacheScope::Private))
        );
        let res = c.list_resources(None).await.unwrap();
        assert_eq!(
            (res.ttl_ms, res.cache_scope),
            (Some(0), Some(CacheScope::Private))
        );
        assert!(res
            .resources
            .iter()
            .any(|r| r.uri == "featherbit://docs/plugins/limit-count"));
        assert!(res
            .resources
            .iter()
            .any(|r| r.uri == "featherbit://policies/echo-policy"));
        let page = c
            .read_resource(ReadResourceRequestParams::new(
                "featherbit://docs/plugins/limit-count",
            ))
            .await
            .unwrap();
        let text = match &page.contents[0] {
            ResourceContents::TextResourceContents { text, .. } => text.clone(),
            _ => panic!("text"),
        };
        assert!(text.starts_with("# limit-count"));
        let pol = c
            .read_resource(ReadResourceRequestParams::new(
                "featherbit://policies/echo-policy",
            ))
            .await
            .unwrap();
        let text = match &pol.contents[0] {
            ResourceContents::TextResourceContents { text, .. } => text.clone(),
            _ => panic!("text"),
        };
        assert!(text.contains("name: echo-policy"));
        assert!(c
            .read_resource(ReadResourceRequestParams::new("featherbit://policies/nope"))
            .await
            .is_err());

        let prompts = c.list_prompts(None).await.unwrap();
        assert_eq!(
            (prompts.ttl_ms, prompts.cache_scope),
            (Some(0), Some(CacheScope::Private))
        );
        assert!(prompts.prompts.iter().any(|p| p.name == "why_this_port"));
        let run = c
            .call_tool(
                CallToolRequestParams::new("run_sandbox").with_arguments(obj(
                    serde_json::json!({"policy": "echo-policy", "context": {"path": "/hello"}}),
                )),
            )
            .await
            .unwrap();
        let id = text_of(&run)["stored_trace_id"]
            .as_str()
            .unwrap()
            .to_string();
        let p = c
            .get_prompt(
                GetPromptRequestParams::new("explain_trace")
                    .with_arguments(obj(serde_json::json!({"trace_id": id}))),
            )
            .await
            .unwrap();
        let msg = match &p.messages[0].content {
            ContentBlock::Text(t) => t.text.clone(),
            _ => panic!("text"),
        };
        assert!(msg.contains("# What is happening in this request?"));
        assert!(c
            .get_prompt(GetPromptRequestParams::new("nope"))
            .await
            .is_err());
        c.cancel().await.unwrap();
    }

    #[tokio::test]
    async fn debug_disabled_surfaces_as_tool_error() {
        let (url, _) = serve(true, false).await;
        let c = client(&url, READ).await;
        let r = c
            .call_tool(CallToolRequestParams::new("list_traces"))
            .await
            .unwrap();
        assert_eq!(r.is_error, Some(true));
        assert_eq!(text_of(&r)["code"], "debug_disabled");
        c.cancel().await.unwrap();
    }

    #[tokio::test]
    async fn raw_http_auth_and_disabled_behaviors() {
        let (url, _) = serve(true, false).await;
        let http = reqwest::Client::new();
        let init = serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {"protocolVersion": "2025-03-26", "capabilities": {}, "clientInfo": {"name": "t", "version": "0"}}});
        let resp = http
            .post(&url)
            .header("accept", "application/json, text/event-stream")
            .json(&init)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 401);
        assert_eq!(
            resp.headers().get("www-authenticate").unwrap(),
            "Bearer realm=\"featherbit-mcp\""
        );
        let resp = http
            .post(&url)
            .header("accept", "application/json, text/event-stream")
            .header("origin", "http://evil.example")
            .bearer_auth(READ)
            .json(&init)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 403);
        // Basic Auth does not open the MCP door.
        let resp = http
            .post(&url)
            .header("accept", "application/json, text/event-stream")
            .basic_auth("u", Some("p"))
            .json(&init)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 401);
        // MCP tokens do not open the Admin API.
        let api = url.replace("/mcp", "/api/policies");
        let resp = http.get(&api).bearer_auth(WRITE).send().await.unwrap();
        assert_eq!(resp.status(), 401);

        let (url, _) = serve(false, false).await;
        let resp = http
            .post(&url)
            .bearer_auth(READ)
            .json(&init)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 404);
        assert_eq!(
            resp.json::<serde_json::Value>().await.unwrap(),
            serde_json::json!({"error": "not_found"})
        );
    }
}
