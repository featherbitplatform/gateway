//! Precompiled prompts: the "what is happening here?" / "why did this node
//! exit on `false`?" questions, rendered with live data so the agent's model
//! starts from the facts. The same renderer backs MCP `prompts/get` and the
//! Admin API's `GET /api/mcp/prompts/{name}` (the UI's "copy as agent
//! prompt"), so the two are byte-identical.

use std::collections::HashMap;

use serde_json::Value;

use crate::mcp::tools::{self, JsonObject, ToolError};
use crate::state::SharedState;

pub struct PromptArg {
    pub name: &'static str,
    pub description: &'static str,
    pub required: bool,
}

pub struct PromptDef {
    pub name: &'static str,
    pub description: &'static str,
    pub args: &'static [PromptArg],
}

const fn arg(name: &'static str, description: &'static str, required: bool) -> PromptArg {
    PromptArg {
        name,
        description,
        required,
    }
}

static PROMPTS: [PromptDef; 8] = [
    PromptDef {
        name: "explain_trace",
        description: "What is happening in this request? Walk through a trace node by node.",
        args: &[arg(
            "trace_id",
            "Trace id from list_traces or the Debug panel",
            true,
        )],
    },
    PromptDef {
        name: "why_this_port",
        description: "Why did a node exit on this port (false/denied/error/limited…)?",
        args: &[
            arg("trace_id", "Trace id", true),
            arg("node_id", "Node id of the step to explain", true),
        ],
    },
    PromptDef {
        name: "why_this_response",
        description: "Why did the client receive this status code?",
        args: &[arg("trace_id", "Trace id", true)],
    },
    PromptDef {
        name: "review_policy",
        description:
            "Review a stored policy for dead nodes, ordering problems and missing error handling.",
        args: &[arg("policy_name", "Policy name", true)],
    },
    PromptDef {
        name: "design_policy",
        description:
            "Design a new policy for a goal, validate it, and apply it (or hand back YAML).",
        args: &[
            arg("goal", "What the policy must do, in plain words", true),
            arg("name", "Policy name to create", false),
        ],
    },
    PromptDef {
        name: "design_supernode",
        description: "Design a reusable supernode for a goal.",
        args: &[
            arg("goal", "What the supernode must do", true),
            arg("name", "Supernode name to create", false),
        ],
    },
    PromptDef {
        name: "design_route",
        description: "Design a route (match rule + policy reference) for a goal.",
        args: &[arg(
            "goal",
            "Which requests should match and which policy should handle them",
            true,
        )],
    },
    PromptDef {
        name: "diagnose_route",
        description: "Which route would match this request, and what would its policy do?",
        args: &[
            arg("method", "HTTP method", true),
            arg("path", "Request path", true),
            arg("headers", "Optional headers as 'Name: value' lines", false),
        ],
    },
];

pub fn prompt_defs() -> &'static [PromptDef] {
    &PROMPTS
}

pub fn prompt_def(name: &str) -> Option<&'static PromptDef> {
    PROMPTS.iter().find(|p| p.name == name)
}

/// A rendered prompt: one user message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderedPrompt {
    pub description: String,
    pub text: String,
}

fn required<'a>(args: &'a HashMap<String, String>, name: &str) -> Result<&'a str, ToolError> {
    args.get(name)
        .map(String::as_str)
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| ToolError::invalid_input(format!("prompt argument '{name}' is required")))
}

fn obj(v: Value) -> JsonObject {
    v.as_object().cloned().unwrap_or_default()
}

fn block(title: &str, v: &Value) -> String {
    format!(
        "## {title}\n\n```json\n{}\n```\n\n",
        serde_json::to_string_pretty(v).unwrap_or_default()
    )
}

const MCP_HINT: &str = "You are connected to the gateway's `featherbit` MCP server: prefer its tools (get_trace_step, get_node_type, validate_policy, run_sandbox) for anything not inlined below.\n\n";

const WIRING_RULE: &str = "Rules of this gateway: a policy is a node graph. Every node's `success`/`out` port and every declared outcome port (e.g. `denied`, `redirect`, `limited`, `true`/`false`) MUST be wired to another node's `in` port or the policy fails to compile; only `error` ports may be left unwired (they fall back to the policy's `error_handler`). Every policy has exactly one `listener` (entry) and one `client` (exit). Plugin config keys are documented per node type — call get_node_type(type) before configuring a node.\n\n";

pub async fn render(
    state: &SharedState,
    name: &str,
    args: &HashMap<String, String>,
) -> Result<RenderedPrompt, ToolError> {
    let def = prompt_def(name).ok_or_else(|| {
        let mut e = ToolError::unknown_tool(name);
        e.code = "unknown_prompt";
        e.message = format!("no prompt named '{name}'");
        e
    })?;
    let text = match name {
        "explain_trace" => {
            let id = required(args, "trace_id")?;
            let trace = tools::call(state, "get_trace", obj(serde_json::json!({"id": id}))).await?;
            format!(
                "{MCP_HINT}# What is happening in this request?\n\nBelow is a debug trace of one request through policy `{}`. Walk through it node by node: for each step say what the node did (use its `changes`), which port it exited on and why. Finish with: which node produced the final response (status {}), and whether anything looks wrong.\n\n{}",
                trace["policy"].as_str().unwrap_or("?"),
                trace["status"],
                block("Trace", &trace)
            )
        }
        "why_this_port" => {
            let id = required(args, "trace_id")?;
            let node = required(args, "node_id")?;
            let step = tools::call(
                state,
                "get_trace_step",
                obj(serde_json::json!({"id": id, "node_id": node})),
            )
            .await?;
            let node_type = step["step"]["node_type"].as_str().unwrap_or("").to_string();
            let port = step["step"]["port"]
                .as_str()
                .unwrap_or("success")
                .to_string();
            let docs = crate::mcp::docs::plugin_page(&node_type).unwrap_or_default();
            format!(
                "{MCP_HINT}# Why did node `{node}` (`{node_type}`) exit on port `{port}`?\n\nExplain, pointing at the exact config keys and context values (headers, query, message, errors) that decided it. If it is an error, name the error code and the fix.\n\n{}{}## Documentation for `{node_type}`\n\n{docs}\n",
                block("Step (before / after / changes / node_config)", &step),
                if step["step"]["outcome"]["kind"] == "error" { "The step's `outcome` is an error — start from its `code` and `message`.\n\n" } else { "" }
            )
        }
        "why_this_response" => {
            let id = required(args, "trace_id")?;
            let trace = tools::call(state, "get_trace", obj(serde_json::json!({"id": id}))).await?;
            let status = trace["status"].clone();
            let setter = trace["steps"]
                .as_array()
                .and_then(|steps| {
                    steps.iter().rev().find(|s| {
                        s["changes"].as_array().is_some_and(|c| {
                            c.iter().any(|ch| ch["path"] == "response.status_code")
                        })
                    })
                })
                .and_then(|s| s["node_id"].as_str())
                .unwrap_or("(no node changed the status — it is the upstream's or the default)");
            format!(
                "{MCP_HINT}# Why did the client receive status {status}?\n\nThe last node to change `response.status_code` was `{setter}`. Explain why it did, using the trace below, and say what would have to change for the request to succeed.\n\n{}",
                block("Trace", &trace)
            )
        }
        "review_policy" => {
            let pname = required(args, "policy_name")?;
            let policy =
                tools::call(state, "get_policy", obj(serde_json::json!({"name": pname}))).await?;
            let types: Vec<String> = policy["policy"]["nodes"]
                .as_array()
                .map(|n| {
                    n.iter()
                        .filter_map(|x| x["type"].as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default();
            let catalog = tools::call(state, "list_node_types", JsonObject::new()).await?;
            let used: Vec<&Value> = catalog["node_types"]
                .as_array()
                .map(|all| {
                    all.iter()
                        .filter(|e| types.iter().any(|t| e["type"] == *t))
                        .collect()
                })
                .unwrap_or_default();
            format!(
                "{MCP_HINT}{WIRING_RULE}# Review policy `{pname}`\n\nReview for: unreachable nodes; ordering problems (authentication or rate limiting after `upstream`; request rewrites after the proxy; response rewrites before it); outcome/error ports routed straight to `client` where a proper rejection or error handler is expected; redundant or contradictory nodes; missing `error_handler`. Propose concrete YAML edits.\n\n{}{}",
                block("Policy", &policy),
                block("Port declarations of the node types used", &Value::Array(used.into_iter().cloned().collect()))
            )
        }
        "design_policy" | "design_supernode" | "design_route" => {
            let goal = required(args, "goal")?;
            let target_name = args.get("name").cloned();
            let catalog = tools::call(state, "list_node_types", JsonObject::new()).await?;
            let existing = match name {
                "design_route" => tools::call(state, "list_routes", JsonObject::new()).await?,
                _ => tools::call(state, "list_policies", JsonObject::new()).await?,
            };
            let (what, put_tool, validate_tool, extra) = match name {
                "design_policy" => ("policy", "put_policy", "validate_policy", String::new()),
                "design_supernode" => (
                    "supernode",
                    "put_supernode",
                    "validate_supernode",
                    format!(
                        "## Supernode rules\n\n{}\n",
                        crate::mcp::docs::concept_page("supernodes").unwrap_or_default()
                    ),
                ),
                _ => ("route", "put_route", "validate_policy", String::new()),
            };
            let named = target_name
                .map(|n| format!(" named `{n}`"))
                .unwrap_or_default();
            format!(
                "{MCP_HINT}{WIRING_RULE}# Design a {what}{named}\n\nGoal: {goal}\n\nWorkflow: (1) pick node types from the catalog below and call get_node_type for each to learn its config keys and ports; (2) write the {what} as YAML; (3) validate with {validate_tool}; (4) call {put_tool} with dry_run=true, fix every reported error, then call it for real. If your token is read-only (write tools are missing or return `forbidden`), stop after validation and return the YAML for a human to apply.\n\n{extra}{}{}",
                block("Node type catalog (type, description, ports)", &catalog),
                block("Existing definitions (avoid name clashes; reuse where sensible)", &existing)
            )
        }
        "diagnose_route" => {
            let method = required(args, "method")?;
            let path = required(args, "path")?;
            let headers = args.get("headers").cloned().unwrap_or_default();
            let routes = tools::call(state, "list_routes", JsonObject::new()).await?;
            format!(
                "{MCP_HINT}# Diagnose `{method} {path}`\n\nDetermine which route matches this request (routes are evaluated in declaration order; the first match wins; `match.path` is a prefix unless the docs say otherwise — check `featherbit://docs/concepts/policies-and-graphs` if unsure). Then call run_sandbox with `policy` set to that route's policy and a `context` of {{method: \"{method}\", path: \"{path}\", headers: …}} and explain what the policy would do to it.\n\nHeaders:\n```\n{headers}\n```\n\n{}",
                block("Routes", &routes)
            )
        }
        _ => unreachable!("prompt_def guarantees a known name"),
    };
    Ok(RenderedPrompt {
        description: def.description.to_string(),
        text,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::tools::test_support::{obj, state, ECHO_GATEWAY};

    fn args(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn defs_are_unique_and_documented() {
        let mut seen = std::collections::HashSet::new();
        for p in prompt_defs() {
            assert!(seen.insert(p.name));
            assert!(!p.description.is_empty());
            assert!(!p.args.is_empty());
        }
        assert!(prompt_def("explain_trace").is_some());
        assert!(prompt_def("nope").is_none());
    }

    #[tokio::test]
    async fn unknown_and_missing_args() {
        let s = state("{}", ECHO_GATEWAY);
        let err = render(&s, "nope", &args(&[])).await.unwrap_err();
        assert_eq!(err.code, "unknown_prompt");
        let err = render(&s, "explain_trace", &args(&[])).await.unwrap_err();
        assert_eq!(err.code, "invalid_input");
        assert!(err.message.contains("trace_id"));
    }

    #[tokio::test]
    async fn trace_prompts_render_from_a_sandbox_run() {
        let s = state("debug:\n  enabled: true\n", ECHO_GATEWAY);
        let run = tools::call(
            &s,
            "run_sandbox",
            obj(serde_json::json!({"policy": "echo-policy", "context": {"path": "/hello"}})),
        )
        .await
        .unwrap();
        let id = run["stored_trace_id"].as_str().unwrap();

        let p = render(&s, "explain_trace", &args(&[("trace_id", id)]))
            .await
            .unwrap();
        assert!(p.text.contains("# What is happening in this request?"));
        assert!(p.text.contains("echo-policy"));
        assert!(p.text.starts_with("You are connected"));

        let p = render(
            &s,
            "why_this_port",
            &args(&[("trace_id", id), ("node_id", "e")]),
        )
        .await
        .unwrap();
        assert!(
            p.text.contains("exit on port `success`"),
            "{}",
            &p.text[..200]
        );
        assert!(p.text.contains("# echo"), "docs page inlined");
        assert!(p.text.contains("node_config"));

        let p = render(&s, "why_this_response", &args(&[("trace_id", id)]))
            .await
            .unwrap();
        assert!(p.text.contains("receive status"));

        let err = render(
            &s,
            "why_this_port",
            &args(&[("trace_id", id), ("node_id", "zz")]),
        )
        .await
        .unwrap_err();
        assert_eq!(err.code, "not_found");
    }

    #[tokio::test]
    async fn authoring_prompts_render() {
        let s = state("{}", ECHO_GATEWAY);
        let p = render(
            &s,
            "review_policy",
            &args(&[("policy_name", "echo-policy")]),
        )
        .await
        .unwrap();
        assert!(p.text.contains("# Review policy `echo-policy`"));
        assert!(p.text.contains("\"type\": \"echo\""));
        let p = render(
            &s,
            "design_policy",
            &args(&[("goal", "rate limit by api key"), ("name", "rl")]),
        )
        .await
        .unwrap();
        assert!(p.text.contains("# Design a policy named `rl`"));
        assert!(p.text.contains("put_policy with dry_run=true"));
        assert!(p.text.contains("limit-count"));
        let p = render(&s, "design_supernode", &args(&[("goal", "auth guard")]))
            .await
            .unwrap();
        assert!(p.text.contains("## Supernode rules"));
        let p = render(
            &s,
            "design_route",
            &args(&[("goal", "/v2 to the v2 policy")]),
        )
        .await
        .unwrap();
        assert!(p.text.contains("\"hello\""), "existing routes inlined");
        let p = render(
            &s,
            "diagnose_route",
            &args(&[("method", "GET"), ("path", "/hello")]),
        )
        .await
        .unwrap();
        assert!(p.text.contains("Diagnose `GET /hello`"));
        assert!(p.text.contains("run_sandbox"));
    }
}
