//! The `degraphql` node — exposes a GraphQL upstream through a plain REST
//! route: the incoming request is rewritten into a standard GraphQL POST
//! (`{"query": ..., "variables": ..., "operationName": ...}`) with variables
//! harvested from the client's query parameters and JSON body.
//!
//! Port of APISIX's `degraphql` plugin. Deviations:
//! - The `query` string is checked only structurally (non-empty, balanced
//!   braces) at config load — APISIX parses it with a real GraphQL parser and
//!   can enforce `operation_name` when the document holds several operations.
//! - APISIX keeps GET requests as GETs, packing `query`/`variables` into the
//!   URI arguments; featherbit always rewrites to a JSON **POST** body, the
//!   canonical GraphQL transport, and converts the method accordingly.
//! - Variables resolve from query parameters first, then from JSON body
//!   fields (APISIX reads only one source depending on the method).
//!
//! Place this node **before** the `upstream` node: it rewrites
//! `context.request` (method, body, headers) that the upstream forwards.

use async_trait::async_trait;
use bytes::Bytes;
use std::collections::HashMap;

use crate::context::{Context, GatewayError};
use crate::plugins::{Plugin, PluginExecutionError, PluginOutput, PluginResult};

/// Rewrites the request into a GraphQL POST for the configured `query`.
///
/// Only `GET` and `POST` requests are accepted (anything else exits through
/// the `error` port with a 405 and code `METHOD_NOT_ALLOWED`, mirroring
/// APISIX). Each configured variable name is looked up in the request's
/// query parameters (first value, as a string) and then in the JSON request
/// body (any JSON type); names found in neither are omitted from
/// `variables`. A non-empty request body that is not valid JSON fails with
/// a 400 and code `INVALID_REQUEST_BODY` when body lookups are needed.
pub struct DegraphqlPlugin {
    query: String,
    variables: Vec<String>,
    operation_name: Option<String>,
}

/// Structural sanity check for a GraphQL document: non-blank, contains a
/// selection set, and curly braces are balanced. Not a full parser — see the
/// module docs.
fn check_query(query: &str) -> Result<(), String> {
    if query.trim().is_empty() {
        return Err("degraphql: 'query' must not be blank".to_string());
    }
    let mut depth: i64 = 0;
    for c in query.chars() {
        match c {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth < 0 {
                    return Err("degraphql: 'query' has unbalanced braces".to_string());
                }
            }
            _ => {}
        }
    }
    if depth != 0 {
        return Err("degraphql: 'query' has unbalanced braces".to_string());
    }
    if !query.contains('{') {
        return Err("degraphql: 'query' has no selection set".to_string());
    }
    Ok(())
}

impl DegraphqlPlugin {
    /// Builds the plugin from node config.
    ///
    /// Accepted keys:
    /// - `query` (string, required, 1–1024 chars): the GraphQL document sent
    ///   upstream. Checked structurally at config load (balanced braces, a
    ///   selection set present).
    /// - `variables` (array of strings, optional): variable names to collect
    ///   from the request. When present it must be non-empty. When absent,
    ///   no `variables` key is sent upstream.
    /// - `operation_name` (string, optional, 1–1024 chars): sent as
    ///   `operationName` for multi-operation documents.
    ///
    /// ```yaml
    /// type: degraphql
    /// config:
    ///   query: |
    ///     query ($name: String!) {
    ///       persons(filter: { name: $name }) { id name }
    ///     }
    ///   variables: [name]
    /// ```
    pub fn from_config(config: &HashMap<String, serde_json::Value>) -> Result<Self, String> {
        let query = config
            .get("query")
            .and_then(|v| v.as_str())
            .ok_or("degraphql: 'query' is required")?;
        if query.len() > 1024 {
            return Err("degraphql: 'query' must be at most 1024 characters".to_string());
        }
        check_query(query)?;

        let variables = match config.get("variables") {
            None => Vec::new(),
            Some(raw) => {
                let items = raw
                    .as_array()
                    .ok_or("degraphql: 'variables' must be an array of strings")?;
                if items.is_empty() {
                    return Err("degraphql: 'variables' must not be empty when present".to_string());
                }
                items
                    .iter()
                    .map(|v| {
                        v.as_str()
                            .filter(|s| !s.is_empty())
                            .map(String::from)
                            .ok_or(
                                "degraphql: 'variables' items must be non-empty strings"
                                    .to_string(),
                            )
                    })
                    .collect::<Result<Vec<_>, _>>()?
            }
        };

        let operation_name = match config.get("operation_name") {
            None => None,
            Some(raw) => {
                let s = raw
                    .as_str()
                    .filter(|s| !s.is_empty() && s.len() <= 1024)
                    .ok_or("degraphql: 'operation_name' must be a string of 1–1024 characters")?;
                Some(s.to_string())
            }
        };

        Ok(Self {
            query: query.to_string(),
            variables,
            operation_name,
        })
    }

    /// Builds the `error`-port rejection with a JSON response body.
    fn fail(
        &self,
        mut ctx: Context,
        status: u16,
        code: &str,
        error: &str,
        message: String,
    ) -> PluginExecutionError {
        ctx.response.status_code = status;
        ctx.response.body =
            Bytes::from(serde_json::json!({ "error": error, "message": message }).to_string());
        ctx.response.headers.insert(
            "content-type".to_string(),
            vec!["application/json".to_string()],
        );
        PluginExecutionError {
            context: ctx,
            error: GatewayError {
                node_id: String::new(),
                code: code.to_string(),
                message,
                metadata: HashMap::new(),
            },
        }
    }
}

#[async_trait]
impl Plugin for DegraphqlPlugin {
    fn plugin_type(&self) -> &str {
        "degraphql"
    }

    async fn execute(
        &self,
        mut ctx: Context,
        _named_inputs: &HashMap<String, serde_json::Value>,
    ) -> PluginResult {
        if ctx.request.method != "GET" && ctx.request.method != "POST" {
            let method = ctx.request.method.clone();
            return Err(self.fail(
                ctx,
                405,
                "METHOD_NOT_ALLOWED",
                "method_not_allowed",
                format!("degraphql accepts GET and POST, got {}", method),
            ));
        }

        let mut new_body = serde_json::Map::new();
        new_body.insert(
            "query".to_string(),
            serde_json::Value::String(self.query.clone()),
        );
        if let Some(op) = &self.operation_name {
            new_body.insert(
                "operationName".to_string(),
                serde_json::Value::String(op.clone()),
            );
        }

        if !self.variables.is_empty() {
            // Query parameters win; JSON body fields fill the rest. The body
            // is parsed lazily — only when a variable is not in the args.
            let mut json_body: Option<serde_json::Value> = None;
            let mut vars = serde_json::Map::new();

            for name in &self.variables {
                if let Some(v) = ctx.request.query_params.get(name).and_then(|v| v.first()) {
                    vars.insert(name.clone(), serde_json::Value::String(v.clone()));
                    continue;
                }
                if !ctx.request.body.is_empty() {
                    if json_body.is_none() {
                        match serde_json::from_slice::<serde_json::Value>(&ctx.request.body) {
                            Ok(parsed) => json_body = Some(parsed),
                            Err(e) => {
                                return Err(self.fail(
                                    ctx,
                                    400,
                                    "INVALID_REQUEST_BODY",
                                    "invalid_request_body",
                                    format!("request body can't be decoded as JSON: {}", e),
                                ));
                            }
                        }
                    }
                    if let Some(v) = json_body.as_ref().and_then(|b| b.get(name)) {
                        vars.insert(name.clone(), v.clone());
                    }
                }
                // found nowhere -> omitted, like APISIX
            }

            new_body.insert("variables".to_string(), serde_json::Value::Object(vars));
        }

        // Rewrite into the canonical GraphQL POST. Body-mutation convention:
        // the new body is plain JSON, so stale framing headers must go.
        ctx.request.method = "POST".to_string();
        ctx.request.body = Bytes::from(
            serde_json::to_vec(&serde_json::Value::Object(new_body)).unwrap_or_default(),
        );
        ctx.request.headers.insert(
            "content-type".to_string(),
            vec!["application/json".to_string()],
        );
        ctx.request.headers.remove("content-length");
        ctx.request.headers.remove("content-encoding");

        Ok(PluginOutput {
            context: ctx,
            named_outputs: HashMap::new(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::{GatewayRequest, GatewayResponse, Protocol};

    const QUERY: &str = "query ($name: String!) { persons(filter: { name: $name }) { id name } }";

    fn test_context(method: &str, body: &str) -> Context {
        let mut headers = HashMap::new();
        headers.insert("content-type".to_string(), vec!["text/plain".to_string()]);
        headers.insert("content-length".to_string(), vec![body.len().to_string()]);

        Context {
            request: GatewayRequest {
                method: method.to_string(),
                path: "/persons".to_string(),
                host: "localhost".to_string(),
                scheme: "http".to_string(),
                headers,
                query_params: HashMap::new(),
                body: Bytes::from(body.to_string()),
                remote_addr: "127.0.0.1:12345".to_string(),
                protocol: Protocol::Http1,
            },
            response: GatewayResponse {
                status_code: 0,
                headers: HashMap::new(),
                body: Bytes::new(),
            },
            message: HashMap::new(),
            errors: Vec::new(),
        }
    }

    fn plugin(vars: Option<serde_json::Value>) -> DegraphqlPlugin {
        let mut config = HashMap::new();
        config.insert("query".to_string(), serde_json::json!(QUERY));
        if let Some(v) = vars {
            config.insert("variables".to_string(), v);
        }
        DegraphqlPlugin::from_config(&config).unwrap()
    }

    #[tokio::test]
    async fn test_degraphql_body_from_query_params() {
        let p = plugin(Some(serde_json::json!(["name"])));
        let mut ctx = test_context("GET", "");
        ctx.request
            .query_params
            .insert("name".to_string(), vec!["jack".to_string()]);

        let out = p.execute(ctx, &HashMap::new()).await.unwrap();
        let req = &out.context.request;
        assert_eq!(req.method, "POST");
        assert_eq!(
            req.headers.get("content-type"),
            Some(&vec!["application/json".to_string()])
        );
        assert!(!req.headers.contains_key("content-length"));
        let body: serde_json::Value = serde_json::from_slice(&req.body).unwrap();
        assert_eq!(body["query"], QUERY);
        assert_eq!(body["variables"], serde_json::json!({ "name": "jack" }));
        assert!(body.get("operationName").is_none());
    }

    #[tokio::test]
    async fn test_degraphql_body_from_json_body_preserves_types() {
        let p = plugin(Some(serde_json::json!(["name", "limit"])));
        let ctx = test_context("POST", r#"{"name":"jill","limit":10,"noise":true}"#);
        let out = p.execute(ctx, &HashMap::new()).await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&out.context.request.body).unwrap();
        // JSON body values keep their JSON types
        assert_eq!(
            body["variables"],
            serde_json::json!({ "name": "jill", "limit": 10 })
        );
    }

    #[tokio::test]
    async fn test_degraphql_query_params_win_over_body() {
        let p = plugin(Some(serde_json::json!(["name"])));
        let mut ctx = test_context("POST", r#"{"name":"from-body"}"#);
        ctx.request
            .query_params
            .insert("name".to_string(), vec!["from-args".to_string()]);
        let out = p.execute(ctx, &HashMap::new()).await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&out.context.request.body).unwrap();
        assert_eq!(body["variables"]["name"], "from-args");
    }

    #[tokio::test]
    async fn test_degraphql_missing_variable_omitted() {
        let p = plugin(Some(serde_json::json!(["name", "ghost"])));
        let ctx = test_context("POST", r#"{"name":"jack"}"#);
        let out = p.execute(ctx, &HashMap::new()).await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&out.context.request.body).unwrap();
        assert_eq!(body["variables"], serde_json::json!({ "name": "jack" }));
    }

    #[tokio::test]
    async fn test_degraphql_no_variables_config() {
        let mut config = HashMap::new();
        config.insert("query".to_string(), serde_json::json!("{ persons { id } }"));
        config.insert("operation_name".to_string(), serde_json::json!("List"));
        let p = DegraphqlPlugin::from_config(&config).unwrap();

        let out = p
            .execute(test_context("GET", ""), &HashMap::new())
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&out.context.request.body).unwrap();
        assert_eq!(body["query"], "{ persons { id } }");
        assert_eq!(body["operationName"], "List");
        assert!(body.get("variables").is_none());
    }

    #[tokio::test]
    async fn test_degraphql_rejects_other_methods() {
        let p = plugin(None);
        let err = p
            .execute(test_context("DELETE", ""), &HashMap::new())
            .await
            .unwrap_err();
        assert_eq!(err.error.code, "METHOD_NOT_ALLOWED");
        assert_eq!(err.context.response.status_code, 405);
    }

    #[tokio::test]
    async fn test_degraphql_invalid_body_when_variable_needed() {
        let p = plugin(Some(serde_json::json!(["name"])));
        let err = p
            .execute(test_context("POST", "not json"), &HashMap::new())
            .await
            .unwrap_err();
        assert_eq!(err.error.code, "INVALID_REQUEST_BODY");
        assert_eq!(err.context.response.status_code, 400);
    }

    #[test]
    fn test_degraphql_config_rejections() {
        let bad = [
            serde_json::json!({}),                                         // query required
            serde_json::json!({ "query": "" }),                            // blank
            serde_json::json!({ "query": "no selection set" }),            // no braces
            serde_json::json!({ "query": "{ unbalanced" }),                // unbalanced
            serde_json::json!({ "query": "} backwards {" }),               // closes first
            serde_json::json!({ "query": "{ x }", "variables": [] }),      // empty variables
            serde_json::json!({ "query": "{ x }", "variables": [1] }),     // non-string variable
            serde_json::json!({ "query": "{ x }", "operation_name": "" }), // blank op name
            serde_json::json!({ "query": format!("{{ {} }}", "a".repeat(2000)) }), // too long
        ];
        for case in bad {
            let config: HashMap<String, serde_json::Value> =
                serde_json::from_value(case.clone()).unwrap();
            assert!(
                DegraphqlPlugin::from_config(&config).is_err(),
                "should reject: {case}"
            );
        }
    }
}
