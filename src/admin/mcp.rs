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
use axum::routing::get;
use axum::{Json, Router};

use crate::mcp::prompts;
use crate::state::SharedState;

pub fn router() -> Router<Arc<SharedState>> {
    Router::new()
        .route("/api/mcp/status", get(status))
        .route("/api/mcp/prompts", get(list_prompts))
        .route("/api/mcp/prompts/{name}", get(render_prompt))
}

async fn status(State(state): State<Arc<SharedState>>) -> impl IntoResponse {
    let mcp = state.system.admin.as_ref().and_then(|a| a.mcp.as_ref());
    let mut scopes: Vec<&str> = mcp
        .map(|m| m.tokens.iter().map(|t| t.scope.as_str()).collect())
        .unwrap_or_default();
    scopes.sort_unstable();
    scopes.dedup();
    Json(serde_json::json!({
        "compiled": cfg!(feature = "mcp"),
        "enabled": cfg!(feature = "mcp") && mcp.is_some_and(|m| m.enabled),
        "path": mcp.map(|m| m.path.clone()).unwrap_or_else(|| "/mcp".to_string()),
        "token_count": mcp.map(|m| m.tokens.len()).unwrap_or(0),
        "scopes": scopes,
    }))
}

async fn list_prompts() -> impl IntoResponse {
    let prompts: Vec<_> = prompts::prompt_defs()
        .iter()
        .map(|p| {
            serde_json::json!({
                "name": p.name,
                "description": p.description,
                "arguments": p.args.iter().map(|a| serde_json::json!({
                    "name": a.name, "description": a.description, "required": a.required
                })).collect::<Vec<_>>(),
            })
        })
        .collect();
    Json(serde_json::json!({ "prompts": prompts }))
}

async fn render_prompt(
    State(state): State<Arc<SharedState>>,
    Path(name): Path<String>,
    Query(args): Query<HashMap<String, String>>,
) -> impl IntoResponse {
    match prompts::render(&state, &name, &args).await {
        Ok(r) => {
            Json(serde_json::json!({ "name": name, "description": r.description, "text": r.text }))
                .into_response()
        }
        Err(e) => {
            let status = match e.code {
                "unknown_prompt" | "not_found" => StatusCode::NOT_FOUND,
                "invalid_input" | "debug_disabled" | "sandbox_disabled" => StatusCode::BAD_REQUEST,
                _ => StatusCode::INTERNAL_SERVER_ERROR,
            };
            let body = if e.code == "unknown_prompt" || e.code == "not_found" {
                serde_json::json!({"error": "not_found"})
            } else {
                let mut v = serde_json::json!({"error": e.code, "message": e.message});
                if let Some(h) = e.hint {
                    v["hint"] = serde_json::Value::String(h);
                }
                v
            };
            (status, Json(body)).into_response()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::tools::test_support::{state, ECHO_GATEWAY};
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    fn app(state: Arc<SharedState>) -> Router {
        router().with_state(state)
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
