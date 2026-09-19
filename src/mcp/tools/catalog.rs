//! Read tools over static knowledge: node types, vars, status, config export.

use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

use super::ToolError;
use crate::state::SharedState;

/// Tools that take no arguments. Only referenced by the `mcp`-only `TOOLS`
/// registry in `src/mcp/tools/mod.rs`.
#[cfg_attr(not(feature = "mcp"), allow(dead_code))]
#[derive(Debug, Default, Deserialize, JsonSchema)]
pub struct NoArgs {}

/// `{ "type": "<node type>" }`
#[derive(Debug, Deserialize, JsonSchema)]
pub struct TypeArgs {
    /// Node type name as it appears in YAML `type:` (e.g. `limit-count`).
    #[serde(rename = "type")]
    pub node_type: String,
}

pub async fn list_node_types() -> Result<Value, ToolError> {
    Ok(serde_json::json!({ "node_types": crate::admin::policies::plugin_catalog() }))
}

pub async fn get_node_type(a: TypeArgs) -> Result<Value, ToolError> {
    let entry = crate::admin::policies::plugin_catalog()
        .into_iter()
        .find(|e| e["type"] == a.node_type)
        .ok_or_else(|| ToolError::not_found("node type", &a.node_type))?;
    let mut out = entry;
    out["docs"] = match crate::mcp::docs::plugin_page(&a.node_type) {
        Some(md) => Value::String(md),
        None => Value::Null,
    };
    Ok(out)
}

pub async fn list_vars() -> Result<Value, ToolError> {
    Ok(serde_json::json!({
        "syntax": {
            "summary": "Two interchangeable ways to reference request data inside any string value of traffic-bound plugin config: legacy `$name` vars (the `vars` list below) and universal `{{namespace.path}}` templates. Both resolve per request; an unknown reference passes through literally. Conditions (`match`/`vars` triple-arrays) use the bare var name without `$`, e.g. [\"arg_channel\", \"==\", \"beta\"].",
            "legacy": {
                "form": "$name  or  ${name}",
                "examples": ["$uri", "$http_x_tenant", "$arg_page", "$cookie_session", "$msg_user", "${msg_label.tier}"],
                "note": "Families: http_<header> (dashes as underscores), arg_<query>, cookie_<name>, post_arg_<field>, msg_<message key>, sent_http_<response header>."
            },
            "templates": {
                "form": "{{ namespace.path }}",
                "namespaces": {
                    "request.method": "HTTP method",
                    "request.path": "request path, no query string",
                    "request.host": "Host header",
                    "request.scheme": "http or https",
                    "request.body": "request body (lossy UTF-8)",
                    "request.headers.<name>": "first value of a request header, name with dashes (request.headers.x-user-id)",
                    "request.query.<name>": "first value of a query parameter",
                    "request.cookies.<name>": "a cookie value",
                    "response.status": "response status code",
                    "response.body": "response body (lossy UTF-8)",
                    "response.headers.<name>": "first value of a response header",
                    "message.<key>": "any context.message key (dotted keys allowed) — where set-vars, traffic-label (label.<key>) and scripts put derived values",
                    "client.ip": "client IP without port",
                    "client.port": "client port",
                    "env.<NAME>": "process environment variable, substituted once at policy compile time (no default syntax)"
                },
                "examples": ["hello {{request.query.name}}", "{{request.headers.x-tenant}}", "user={{message.user}}"]
            },
            "where": "Every string config value of traffic-bound plugins: header values (proxy-rewrite/response-rewrite set_headers), mocking response_example and response_headers, limit-count/limit-conn key, redirect uri, fault-injection abort body, traffic-label set_headers/set_labels, forward-auth extra_headers, logger log_format… Not templated: regex/CIDR/IP lists, JSON-Schema/OpenAPI documents, Lua sources, upstream targets, TLS paths, logger endpoints, route match rules; body-transformer and error-handler keep their own {{...}} dialects.",
            "derive": "To compute a new variable (a path segment, a JSON body field, a regex capture of a header) add a `set-vars` node before the consumer; it stores results in context.message → $msg_<name> / {{message.<name>}}. Example: set-vars {vars: [{name: user, from: $uri, regex: '^/hello/([^/]+)'}]} then mocking response_example: 'hello $msg_user'.",
            "docs": ["featherbit://docs/reference/templates", "featherbit://docs/reference/context-vars", "featherbit://docs/reference/conditions", "featherbit://docs/plugins/set-vars"]
        },
        "vars": crate::vars::catalog::var_catalog()
    }))
}

pub async fn get_status(state: &SharedState) -> Result<Value, ToolError> {
    let routes = state.routes.read().await.len();
    let gw = state.gateway.read().await;
    Ok(serde_json::json!({
        "version": env!("CARGO_PKG_VERSION"),
        "routes": routes,
        "policies": gw.policies.len(),
        "supernodes": gw.supernodes.len(),
        "stores": gw.stores.len(),
        "debug": { "enabled": state.debug.enabled, "sandbox": state.debug.sandbox_enabled },
    }))
}

pub async fn export_config(state: &SharedState) -> Result<Value, ToolError> {
    let mut gw = state.gateway.read().await.clone();
    gw.consumers = gw
        .consumers
        .iter()
        .map(crate::consumers::mask_credentials)
        .collect();
    let yaml = serde_yaml::to_string(&gw).map_err(|e| ToolError::internal(e.to_string()))?;
    Ok(serde_json::json!({
        "format": "yaml",
        "content": yaml,
        "note": "consumer credential secrets are masked",
    }))
}

#[cfg(test)]
mod tests {
    use crate::mcp::tools::call;
    use crate::mcp::tools::test_support::{obj, state, ECHO_GATEWAY};

    #[tokio::test]
    async fn node_types_and_lookup() {
        let s = state("{}", "{}");
        let v = call(&s, "list_node_types", obj(serde_json::json!({})))
            .await
            .unwrap();
        let types = v["node_types"].as_array().unwrap();
        assert!(types.iter().any(|t| t["type"] == "limit-count"));
        assert!(types[0]["ports"].is_object());

        let v = call(
            &s,
            "get_node_type",
            obj(serde_json::json!({"type": "condition"})),
        )
        .await
        .unwrap();
        let outs = v["ports"]["outputs"].as_array().unwrap();
        assert!(
            outs.iter().any(|p| p["name"] == "true") && outs.iter().any(|p| p["name"] == "false")
        );

        let err = call(
            &s,
            "get_node_type",
            obj(serde_json::json!({"type": "nope"})),
        )
        .await
        .unwrap_err();
        assert_eq!(err.code, "not_found");
        let err = call(&s, "get_node_type", obj(serde_json::json!({})))
            .await
            .unwrap_err();
        assert_eq!(err.code, "invalid_input");
    }

    /// get_node_type is what an agent reads before authoring a node. A missing
    /// docs page returns `docs: null` rather than failing, so assert it directly
    /// instead of trusting the file-existence guard.
    #[tokio::test]
    async fn store_nodes_expose_their_docs_to_agents() {
        let s = state("{}", "{}");
        for t in ["store-get", "store-set", "store-incr", "store-delete"] {
            let v = call(&s, "get_node_type", obj(serde_json::json!({ "type": t })))
                .await
                .unwrap();
            assert!(
                v["docs"].as_str().is_some_and(|d| !d.is_empty()),
                "{t} must serve a docs page to agents"
            );
        }
    }

    /// An agent must see that store-get has a mandatory `miss` port, or it will
    /// author a policy that fails to compile.
    #[tokio::test]
    async fn store_get_exposes_its_miss_port() {
        let s = state("{}", "{}");
        let v = call(
            &s,
            "get_node_type",
            obj(serde_json::json!({ "type": "store-get" })),
        )
        .await
        .unwrap();
        let outs = v["ports"]["outputs"].as_array().unwrap();
        let miss = outs
            .iter()
            .find(|p| p["name"] == "miss")
            .expect("miss port");
        assert_eq!(miss["kind"], "outcome");
    }

    #[tokio::test]
    async fn list_vars_explains_both_syntaxes_and_where_they_apply() {
        let s = state("{}", ECHO_GATEWAY);
        let v = call(&s, "list_vars", obj(serde_json::json!({})))
            .await
            .unwrap();
        assert!(v["vars"].as_array().is_some_and(|a| !a.is_empty()));
        let syntax = &v["syntax"];
        assert!(syntax["summary"]
            .as_str()
            .unwrap()
            .contains("{{namespace.path}}"));
        assert_eq!(syntax["legacy"]["form"], "$name  or  ${name}");
        for ns in [
            "request.path",
            "request.headers.<name>",
            "message.<key>",
            "env.<NAME>",
        ] {
            assert!(syntax["templates"]["namespaces"][ns].is_string(), "{ns}");
        }
        assert!(syntax["derive"].as_str().unwrap().contains("set-vars"));
        assert!(syntax["docs"]
            .as_array()
            .unwrap()
            .iter()
            .any(|d| d == "featherbit://docs/reference/templates"));
    }

    #[tokio::test]
    async fn vars_status_export() {
        let gw = format!(
            "{ECHO_GATEWAY}\nconsumers:\n  - name: alice\n    credentials:\n      key-auth: {{ key: topsecret }}\n"
        );
        let s = state("debug:\n  enabled: true\n", &gw);
        let v = call(&s, "list_vars", obj(serde_json::json!({})))
            .await
            .unwrap();
        assert!(v["vars"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["name"] == "remote_addr"));
        let v = call(&s, "get_status", obj(serde_json::json!({})))
            .await
            .unwrap();
        assert_eq!(v["routes"], 1);
        assert_eq!(v["debug"]["enabled"], true);
        let v = call(&s, "export_config", obj(serde_json::json!({})))
            .await
            .unwrap();
        let content = v["content"].as_str().unwrap();
        assert!(content.contains("echo-policy"));
        assert!(content.contains("<masked>"));
        assert!(!content.contains("topsecret"));
        assert_eq!(v["note"], "consumer credential secrets are masked");
    }
}
