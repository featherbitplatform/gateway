//! Admin API companions to the MCP server, for the web UI: connection status
//! (never token values) and the rendered prompt texts behind "Copy as agent
//! prompt". Basic-Auth like the rest of `/api`; answer whether or not MCP is
//! enabled — rendering a prompt exposes nothing a Basic Auth user cannot
//! already read.

use std::collections::HashMap;
use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use serde::Serialize;
use utoipa::ToSchema;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use super::openapi::ErrorResponse;
use crate::mcp::prompts;
use crate::state::SharedState;

pub fn router() -> OpenApiRouter<Arc<SharedState>> {
    OpenApiRouter::new()
        .routes(routes!(status))
        .routes(routes!(list_prompts))
        .routes(routes!(render_prompt))
}

/// Body of `GET /api/mcp/status`. Never carries token values.
#[derive(Serialize, ToSchema)]
struct McpStatus {
    /// Whether the binary was built with the `mcp` cargo feature.
    compiled: bool,
    /// Whether the MCP endpoint is served (compiled in and `admin.mcp.enabled`).
    enabled: bool,
    /// Mount path on the admin listener (`admin.mcp.path`, default `/mcp`).
    path: String,
    /// Number of configured bearer tokens.
    token_count: usize,
    /// Distinct scopes (`read`/`write`) granted by those tokens, sorted.
    scopes: Vec<String>,
}

/// `GET /api/mcp/status` — the MCP server's configuration as the UI needs
/// it: whether it is compiled in and enabled, its path, and its token
/// scopes. Answers whether or not MCP is enabled.
#[utoipa::path(
    get,
    path = "/api/mcp/status",
    tag = "mcp",
    operation_id = "get_mcp_status",
    summary = "Get MCP server status",
    responses((status = 200, description = "MCP server configuration (token values are never included).", body = McpStatus)),
)]
async fn status(State(state): State<Arc<SharedState>>) -> impl IntoResponse {
    let mcp = state.system.admin.as_ref().and_then(|a| a.mcp.as_ref());
    let mut scopes: Vec<String> = mcp
        .map(|m| {
            m.tokens
                .iter()
                .map(|t| t.scope.as_str().to_string())
                .collect()
        })
        .unwrap_or_default();
    scopes.sort_unstable();
    scopes.dedup();
    Json(McpStatus {
        compiled: cfg!(feature = "mcp"),
        enabled: cfg!(feature = "mcp") && mcp.is_some_and(|m| m.enabled),
        path: mcp
            .map(|m| m.path.clone())
            .unwrap_or_else(|| "/mcp".to_string()),
        token_count: mcp.map(|m| m.tokens.len()).unwrap_or(0),
        scopes,
    })
}

/// Body of `GET /api/mcp/prompts`.
#[derive(Serialize, ToSchema)]
struct McpPromptList {
    /// Every precompiled prompt.
    prompts: Vec<McpPromptInfo>,
}

/// One precompiled prompt and its arguments.
#[derive(Serialize, ToSchema)]
struct McpPromptInfo {
    /// Prompt name, for `GET /api/mcp/prompts/{name}`.
    name: &'static str,
    /// What the prompt asks the agent to do.
    description: &'static str,
    /// Arguments, passed as query parameters when rendering.
    arguments: Vec<McpPromptArgument>,
}

/// One argument of a precompiled prompt.
#[derive(Serialize, ToSchema)]
struct McpPromptArgument {
    /// Argument name (the query parameter name when rendering).
    name: &'static str,
    /// What the argument holds.
    description: &'static str,
    /// Whether rendering fails without it.
    required: bool,
}

/// `GET /api/mcp/prompts` — the precompiled agent prompts and their
/// arguments (the same set MCP `prompts/list` serves).
#[utoipa::path(
    get,
    path = "/api/mcp/prompts",
    tag = "mcp",
    summary = "List agent prompts",
    responses((status = 200, description = "Every precompiled prompt with its arguments.", body = McpPromptList)),
)]
async fn list_prompts() -> impl IntoResponse {
    let prompts = prompts::prompt_defs()
        .iter()
        .map(|p| McpPromptInfo {
            name: p.name,
            description: p.description,
            arguments: p
                .args
                .iter()
                .map(|a| McpPromptArgument {
                    name: a.name,
                    description: a.description,
                    required: a.required,
                })
                .collect(),
        })
        .collect();
    Json(McpPromptList { prompts })
}

/// Body of a successful `GET /api/mcp/prompts/{name}`.
#[derive(Serialize, ToSchema)]
struct RenderedMcpPrompt {
    /// Prompt name.
    name: String,
    /// What the prompt asks the agent to do.
    description: String,
    /// The rendered prompt text (one user message), byte-identical to MCP
    /// `prompts/get`.
    text: String,
}

/// Error body of `GET /api/mcp/prompts/{name}` for everything but `404`.
#[derive(Serialize, ToSchema)]
struct McpPromptError {
    /// Error code, e.g. `invalid_input`, `debug_disabled`, `sandbox_disabled`.
    error: &'static str,
    /// Human-readable explanation.
    message: String,
    /// What to do about it, when there is a suggestion.
    #[serde(skip_serializing_if = "Option::is_none")]
    hint: Option<String>,
}

/// `GET /api/mcp/prompts/{name}` — renders a prompt with live data (the
/// UI's "copy as agent prompt"). Prompt arguments are passed as query
/// parameters; which ones a prompt takes is listed by `GET /api/mcp/prompts`.
///
/// Errors: `404` for an unknown prompt or a referenced object (trace,
/// policy, ...) that does not exist; `400` for a missing/invalid argument or
/// when the prompt needs debug mode (or the sandbox) and it is off.
#[utoipa::path(
    get,
    path = "/api/mcp/prompts/{name}",
    tag = "mcp",
    summary = "Render an agent prompt",
    params(
        ("name" = String, Path, description = "Prompt name, from `GET /api/mcp/prompts`."),
        ("trace_id" = Option<String>, Query, description = "Trace id (`troubleshoot_trace`, `explain_trace`, `why_this_port`, `why_this_response`)."),
        ("node_id" = Option<String>, Query, description = "Node id of the step to explain (`why_this_port`)."),
        ("policy_name" = Option<String>, Query, description = "Policy to review (`review_policy`)."),
        ("goal" = Option<String>, Query, description = "What to design, in plain words (`design_policy`, `design_supernode`, `design_route`)."),
        ("name" = Option<String>, Query, description = "Name of the policy or supernode to create (`design_policy`, `design_supernode`)."),
        ("method" = Option<String>, Query, description = "HTTP method of the request to diagnose (`diagnose_route`)."),
        ("path" = Option<String>, Query, description = "Path of the request to diagnose (`diagnose_route`)."),
        ("headers" = Option<String>, Query, description = "Headers of the request to diagnose, as `Name: value` lines (`diagnose_route`)."),
    ),
    responses(
        (status = 200, description = "The rendered prompt.", body = RenderedMcpPrompt),
        (status = 400, description = "A required argument is missing or invalid (`invalid_input`), or the prompt needs debug mode or the sandbox and it is off (`debug_disabled`, `sandbox_disabled`).", body = McpPromptError),
        (status = 404, description = "No such prompt, or the trace/policy it references does not exist (`error: not_found`).", body = ErrorResponse),
        (status = 500, description = "Rendering failed for another reason.", body = McpPromptError),
    ),
)]
async fn render_prompt(
    State(state): State<Arc<SharedState>>,
    Path(name): Path<String>,
    Query(args): Query<HashMap<String, String>>,
) -> impl IntoResponse {
    match prompts::render(&state, &name, &args).await {
        Ok(r) => Json(RenderedMcpPrompt {
            name,
            description: r.description,
            text: r.text,
        })
        .into_response(),
        Err(e) => {
            let status = match e.code {
                "unknown_prompt" | "not_found" => StatusCode::NOT_FOUND,
                "invalid_input" | "debug_disabled" | "sandbox_disabled" => StatusCode::BAD_REQUEST,
                _ => StatusCode::INTERNAL_SERVER_ERROR,
            };
            if e.code == "unknown_prompt" || e.code == "not_found" {
                (
                    status,
                    Json(ErrorResponse {
                        error: "not_found".to_string(),
                    }),
                )
                    .into_response()
            } else {
                (
                    status,
                    Json(McpPromptError {
                        error: e.code,
                        message: e.message,
                        hint: e.hint,
                    }),
                )
                    .into_response()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::tools::test_support::{state, ECHO_GATEWAY};
    use axum::body::Body;
    use axum::http::Request;
    use axum::Router;
    use tower::ServiceExt;

    fn app(state: Arc<SharedState>) -> Router {
        Router::from(router()).with_state(state)
    }

    async fn get_json(app: Router, uri: &str) -> (StatusCode, serde_json::Value) {
        let resp = app
            .oneshot(Request::get(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = resp.status();
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, serde_json::from_slice(&body).unwrap())
    }

    #[tokio::test]
    async fn status_reports_config_without_tokens() {
        let s = state(
            "admin:\n  username: u\n  password: p\n  mcp:\n    enabled: true\n    path: /agent\n    tokens:\n      - {token: rrrrrrrrrrrrrrrrrrrr, scope: read}\n      - {token: wwwwwwwwwwwwwwwwwwww, scope: write}\n      - {token: qqqqqqqqqqqqqqqqqqqq, scope: read}\n",
            ECHO_GATEWAY,
        );
        let (st, v) = get_json(app(s), "/api/mcp/status").await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(v["compiled"], cfg!(feature = "mcp"));
        assert_eq!(v["enabled"], cfg!(feature = "mcp"));
        assert_eq!(v["path"], "/agent");
        assert_eq!(v["token_count"], 3);
        assert_eq!(v["scopes"], serde_json::json!(["read", "write"]));
        assert!(!v.to_string().contains("rrrrrrrr"));

        let (_, v) = get_json(app(state("{}", "{}")), "/api/mcp/status").await;
        assert_eq!(v["enabled"], false);
        assert_eq!(v["path"], "/mcp");
        assert_eq!(v["token_count"], 0);
    }

    #[tokio::test]
    async fn prompts_list_and_render() {
        let s = state("debug:\n  enabled: true\n", ECHO_GATEWAY);
        let (st, v) = get_json(app(s.clone()), "/api/mcp/prompts").await;
        assert_eq!(st, StatusCode::OK);
        assert!(v["prompts"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p["name"] == "why_this_port" && p["arguments"][1]["name"] == "node_id"));

        let (st, v) = get_json(
            app(s.clone()),
            "/api/mcp/prompts/review_policy?policy_name=echo-policy",
        )
        .await;
        assert_eq!(st, StatusCode::OK);
        assert!(v["text"]
            .as_str()
            .unwrap()
            .contains("# Review policy `echo-policy`"));
        assert_eq!(v["name"], "review_policy");

        let (st, v) = get_json(
            app(s.clone()),
            "/api/mcp/prompts/review_policy?policy_name=nope",
        )
        .await;
        assert_eq!(st, StatusCode::NOT_FOUND);
        assert_eq!(v["error"], "not_found");
        let (st, _) = get_json(app(s.clone()), "/api/mcp/prompts/nope").await;
        assert_eq!(st, StatusCode::NOT_FOUND);
        let (st, v) = get_json(app(s), "/api/mcp/prompts/explain_trace").await;
        assert_eq!(st, StatusCode::BAD_REQUEST);
        assert_eq!(v["error"], "invalid_input");

        let off = state("{}", ECHO_GATEWAY);
        let (st, v) = get_json(app(off), "/api/mcp/prompts/explain_trace?trace_id=x").await;
        assert_eq!(st, StatusCode::BAD_REQUEST);
        assert_eq!(v["error"], "debug_disabled");
    }
}
