//! The `oas-validator` node — validates the incoming request against an
//! OpenAPI 3 (OAS 3) specification before it reaches the upstream.
//!
//! Port of APISIX's `oas-validator` plugin. The OpenAPI document is supplied
//! inline as a JSON object in the node config; at config load every
//! operation's `requestBody` JSON Schema is compiled and its required
//! parameters are indexed, so a malformed spec or an uncompilable schema fails
//! policy compilation, not a live request.
//!
//! At request time the plugin matches the request's method + path (with
//! OpenAPI path templating, e.g. `/users/{id}` matches `/users/123`) against
//! the spec. When an operation matches it validates required query/header
//! parameters and the JSON request body against the operation's schema; on any
//! violation the request is rejected through the `error` port. When **no**
//! operation matches, the request passes through untouched (matching APISIX:
//! it is not the validator's job to 404).
//!
//! ## Deviations from APISIX / scope
//!
//! - **Inline-JSON spec only.** The `spec` is an inline OpenAPI JSON object.
//!   APISIX's `spec` (a JSON *string*) and `spec_url` (remote fetch) forms, a
//!   YAML loader, and secret-reference indirection are out of scope.
//! - **Faithful subset.** Validated: presence of `required` query/header
//!   parameters, and the `application/json` `requestBody` schema. Local
//!   `$ref`s (`#/components/...`) are resolved by embedding `components` into
//!   the compiled schema root. Not covered: response validation, parameter
//!   *type/format* coercion and schema validation, `oneOf`/`anyOf` operation
//!   selection, cookie parameters, and `$ref`s to external documents.
//! - APISIX's `skip_*` toggles, `verbose_errors`, and `reject_if_not_match`
//!   are not modelled; a matched operation is always validated and violations
//!   are always rejected with `rejected_code`.

use async_trait::async_trait;
use bytes::Bytes;
use std::collections::HashMap;

use crate::context::{Context, GatewayError};
use crate::plugins::{Plugin, PluginExecutionError, PluginOutput, PluginResult};

/// One segment of a templated OpenAPI path.
enum Segment {
    /// A literal path segment that must match exactly.
    Literal(String),
    /// A `{name}` template segment that matches any single non-empty segment.
    Param,
}

/// A compiled OpenAPI operation: everything needed to match a request and
/// validate it, precomputed at config load.
struct CompiledOp {
    /// Uppercase HTTP method (`GET`, `POST`, …).
    method: String,
    /// The path template split into segments, for matching.
    segments: Vec<Segment>,
    /// Count of literal segments — used to prefer more-specific matches.
    literal_count: usize,
    /// Names of `required: true` query parameters.
    required_query: Vec<String>,
    /// Lowercased names of `required: true` header parameters.
    required_headers: Vec<String>,
    /// Compiled `application/json` requestBody schema, if any.
    body_schema: Option<jsonschema::Validator>,
    /// Whether the requestBody is marked `required: true`.
    body_required: bool,
}

/// Validates requests against a compiled OpenAPI 3 spec.
pub struct OasValidatorPlugin {
    operations: Vec<CompiledOp>,
    rejected_code: u16,
    rejected_msg: Option<String>,
}

/// Resolves a local JSON pointer `$ref` (`#/a/b/c`) against the root document.
fn resolve_ref<'a>(root: &'a serde_json::Value, reference: &str) -> Option<&'a serde_json::Value> {
    let pointer = reference.strip_prefix('#')?;
    root.pointer(pointer)
}

/// Splits an OpenAPI path template into segments.
fn parse_template(path: &str) -> (Vec<Segment>, usize) {
    let mut segments = Vec::new();
    let mut literal_count = 0;
    for part in path.split('/').filter(|s| !s.is_empty()) {
        if part.starts_with('{') && part.ends_with('}') {
            segments.push(Segment::Param);
        } else {
            literal_count += 1;
            segments.push(Segment::Literal(part.to_string()));
        }
    }
    (segments, literal_count)
}

/// Returns true if a request path matches this operation's segment template.
fn path_matches(segments: &[Segment], req_path: &str) -> bool {
    let parts: Vec<&str> = req_path.split('/').filter(|s| !s.is_empty()).collect();
    if parts.len() != segments.len() {
        return false;
    }
    for (seg, part) in segments.iter().zip(parts.iter()) {
        match seg {
            Segment::Param => {
                if part.is_empty() {
                    return false;
                }
            }
            Segment::Literal(lit) => {
                if lit != part {
                    return false;
                }
            }
        }
    }
    true
}

/// Gathers `required` parameters (query + header) for an operation, merging
/// path-item-level and operation-level parameter lists and resolving local
/// `$ref`s. Returns `(required_query, required_headers_lowercased)`.
fn collect_required_params(
    spec: &serde_json::Value,
    path_item: &serde_json::Value,
    operation: &serde_json::Value,
) -> Result<(Vec<String>, Vec<String>), String> {
    let mut required_query = Vec::new();
    let mut required_headers = Vec::new();

    let lists = [path_item.get("parameters"), operation.get("parameters")];
    for list in lists.into_iter().flatten() {
        let arr = list
            .as_array()
            .ok_or("oas-validator: 'parameters' must be an array")?;
        for raw in arr {
            // Resolve a parameter $ref if present.
            let param = if let Some(r) = raw.get("$ref").and_then(|v| v.as_str()) {
                resolve_ref(spec, r)
                    .ok_or_else(|| format!("oas-validator: unresolved parameter $ref '{}'", r))?
            } else {
                raw
            };

            let required = param
                .get("required")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            if !required {
                continue;
            }
            let name = match param.get("name").and_then(|v| v.as_str()) {
                Some(n) => n,
                None => continue,
            };
            match param.get("in").and_then(|v| v.as_str()) {
                Some("query") => required_query.push(name.to_string()),
                Some("header") => required_headers.push(name.to_lowercase()),
                // path params are inherently present once the path matches;
                // cookie params are out of scope.
                _ => {}
            }
        }
    }

    Ok((required_query, required_headers))
}

/// Compiles the `application/json` requestBody schema for an operation, if
/// present, embedding the spec's `components` so local `$ref`s resolve.
fn compile_body_schema(
    spec: &serde_json::Value,
    operation: &serde_json::Value,
    method: &str,
    template: &str,
) -> Result<(Option<jsonschema::Validator>, bool), String> {
    let request_body = match operation.get("requestBody") {
        Some(rb) => rb,
        None => return Ok((None, false)),
    };

    // requestBody itself may be a $ref.
    let request_body = if let Some(r) = request_body.get("$ref").and_then(|v| v.as_str()) {
        resolve_ref(spec, r)
            .ok_or_else(|| format!("oas-validator: unresolved requestBody $ref '{}'", r))?
    } else {
        request_body
    };

    let body_required = request_body
        .get("required")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let schema = request_body
        .get("content")
        .and_then(|c| c.get("application/json"))
        .and_then(|m| m.get("schema"));

    let schema = match schema {
        Some(s) => s,
        None => return Ok((None, body_required)),
    };

    if !schema.is_object() {
        return Err(format!(
            "oas-validator: {} {} requestBody schema must be an object",
            method, template
        ));
    }

    // Embed `components` into the schema root so `#/components/...` refs resolve.
    let mut root = schema.clone();
    if let (Some(map), Some(components)) = (root.as_object_mut(), spec.get("components")) {
        map.entry("components".to_string())
            .or_insert_with(|| components.clone());
    }

    let validator = jsonschema::validator_for(&root).map_err(|e| {
        format!(
            "oas-validator: invalid requestBody schema for {} {}: {}",
            method, template, e
        )
    })?;

    Ok((Some(validator), body_required))
}

const HTTP_METHODS: [&str; 8] = [
    "get", "put", "post", "delete", "options", "head", "patch", "trace",
];

impl OasValidatorPlugin {
    /// Builds the plugin from node config.
    ///
    /// Accepted keys:
    /// - `spec` (object, **required**): the OpenAPI 3 document as an inline
    ///   JSON object. Its `paths` are walked and every operation's
    ///   `application/json` requestBody schema is compiled and required
    ///   parameters indexed. A non-object `spec`, a malformed `paths`, or an
    ///   uncompilable schema fails here at config load.
    /// - `rejected_code` (integer 400–599, default `400`): response status for
    ///   rejected requests.
    /// - `rejected_msg` (string): fixed message returned instead of the
    ///   per-violation detail.
    ///
    /// ```yaml
    /// type: oas-validator
    /// config:
    ///   rejected_code: 400
    ///   spec:
    ///     openapi: 3.0.0
    ///     info: { title: demo, version: "1.0" }
    ///     paths:
    ///       /users/{id}:
    ///         get:
    ///           parameters:
    ///             - { name: verbose, in: query, required: true, schema: { type: string } }
    ///         post:
    ///           requestBody:
    ///             required: true
    ///             content:
    ///               application/json:
    ///                 schema:
    ///                   type: object
    ///                   required: [name]
    ///                   properties: { name: { type: string } }
    /// ```
    pub fn from_config(config: &HashMap<String, serde_json::Value>) -> Result<Self, String> {
        let spec = config
            .get("spec")
            .ok_or("oas-validator: 'spec' (inline OpenAPI JSON object) is required")?;

        if !spec.is_object() {
            return Err("oas-validator: 'spec' must be a JSON object".to_string());
        }

        let paths = spec
            .get("paths")
            .and_then(|p| p.as_object())
            .ok_or("oas-validator: spec is missing a 'paths' object")?;

        let mut operations = Vec::new();
        for (template, path_item) in paths {
            if !path_item.is_object() {
                return Err(format!(
                    "oas-validator: path item '{}' must be an object",
                    template
                ));
            }

            for method in HTTP_METHODS {
                let operation = match path_item.get(method) {
                    Some(op) => op,
                    None => continue,
                };
                if !operation.is_object() {
                    return Err(format!(
                        "oas-validator: operation '{} {}' must be an object",
                        method, template
                    ));
                }

                let (required_query, required_headers) =
                    collect_required_params(spec, path_item, operation)?;
                let (body_schema, body_required) =
                    compile_body_schema(spec, operation, method, template)?;

                // Rebuild segments per op (Segment isn't Clone; cheap).
                let (segments, literal_count) = parse_template(template);
                operations.push(CompiledOp {
                    method: method.to_uppercase(),
                    segments,
                    literal_count,
                    required_query,
                    required_headers,
                    body_schema,
                    body_required,
                });
            }
        }

        let rejected_code = match config.get("rejected_code") {
            None => 400,
            Some(v) => {
                let code = v
                    .as_u64()
                    .filter(|c| (400..=599).contains(c))
                    .ok_or("oas-validator: 'rejected_code' must be an integer in 400..=599")?;
                code as u16
            }
        };

        let rejected_msg = config
            .get("rejected_msg")
            .and_then(|v| v.as_str())
            .map(String::from);

        Ok(Self {
            operations,
            rejected_code,
            rejected_msg,
        })
    }

    /// Finds the best-matching operation for a method + path: the matching
    /// operation with the most literal (non-templated) segments wins, so an
    /// exact path is preferred over a templated one.
    fn match_operation(&self, method: &str, path: &str) -> Option<&CompiledOp> {
        self.operations
            .iter()
            .filter(|op| op.method == method && path_matches(&op.segments, path))
            .max_by_key(|op| op.literal_count)
    }

    /// Writes the rejection onto the response and returns the error that routes
    /// the context through the node's `error` port.
    fn reject(&self, mut ctx: Context, detail: String) -> PluginExecutionError {
        let message = self.rejected_msg.clone().unwrap_or(detail);
        ctx.response.status_code = self.rejected_code;
        ctx.response.body = Bytes::from(
            serde_json::json!({ "error": "oas_validation_failed", "message": message }).to_string(),
        );
        ctx.response.headers.insert(
            "content-type".to_string(),
            vec!["application/json".to_string()],
        );
        PluginExecutionError {
            context: ctx,
            error: GatewayError {
                node_id: String::new(),
                code: "OAS_VALIDATION_FAILED".to_string(),
                message,
                metadata: HashMap::new(),
            },
        }
    }
}

#[async_trait]
impl Plugin for OasValidatorPlugin {
    fn plugin_type(&self) -> &str {
        "oas-validator"
    }

    async fn execute(
        &self,
        ctx: Context,
        _named_inputs: &HashMap<String, serde_json::Value>,
    ) -> PluginResult {
        // Find the matching operation; no match → pass through (not our job to 404).
        let op = match self.match_operation(&ctx.request.method, &ctx.request.path) {
            Some(op) => op,
            None => {
                return Ok(PluginOutput {
                    context: ctx,
                    named_outputs: HashMap::new(),
                })
            }
        };

        // Required query parameters must be present.
        for q in &op.required_query {
            if !ctx.request.query_params.contains_key(q) {
                return Err(self.reject(ctx, format!("missing required query parameter '{}'", q)));
            }
        }

        // Required header parameters must be present (headers are lowercased).
        for h in &op.required_headers {
            if !ctx.request.headers.contains_key(h) {
                return Err(self.reject(ctx, format!("missing required header '{}'", h)));
            }
        }

        // Request body validation.
        let body_empty = ctx.request.body.is_empty();
        if op.body_required && body_empty {
            return Err(self.reject(ctx, "request body is required".to_string()));
        }

        if let Some(validator) = &op.body_schema {
            if !body_empty {
                let is_json = ctx
                    .request
                    .headers
                    .get("content-type")
                    .and_then(|v| v.first())
                    .map(|ct| ct.to_lowercase().starts_with("application/json"))
                    .unwrap_or(true); // absent content-type: treat as JSON
                if is_json {
                    let parsed: serde_json::Value = match serde_json::from_slice(&ctx.request.body)
                    {
                        Ok(v) => v,
                        Err(e) => {
                            return Err(self
                                .reject(ctx, format!("failed to decode the request body: {}", e)))
                        }
                    };
                    if let Err(e) = validator.validate(&parsed) {
                        return Err(self.reject(ctx, format!("body validation failed: {}", e)));
                    }
                }
            }
        }

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

    fn spec() -> serde_json::Value {
        serde_json::json!({
            "openapi": "3.0.0",
            "info": { "title": "demo", "version": "1.0" },
            "components": {
                "schemas": {
                    "User": {
                        "type": "object",
                        "required": ["name"],
                        "properties": { "name": { "type": "string", "minLength": 1 } }
                    }
                }
            },
            "paths": {
                "/users/{id}": {
                    "get": {
                        "parameters": [
                            { "name": "verbose", "in": "query", "required": true, "schema": { "type": "string" } },
                            { "name": "x-trace", "in": "header", "required": true, "schema": { "type": "string" } }
                        ]
                    },
                    "post": {
                        "requestBody": {
                            "required": true,
                            "content": {
                                "application/json": {
                                    "schema": { "$ref": "#/components/schemas/User" }
                                }
                            }
                        }
                    }
                }
            }
        })
    }

    fn plugin() -> OasValidatorPlugin {
        let mut config = HashMap::new();
        config.insert("spec".to_string(), spec());
        OasValidatorPlugin::from_config(&config).unwrap()
    }

    fn ctx(method: &str, path: &str) -> Context {
        Context {
            request: GatewayRequest {
                method: method.to_string(),
                path: path.to_string(),
                host: "localhost".to_string(),
                scheme: "http".to_string(),
                headers: HashMap::new(),
                query_params: HashMap::new(),
                body: Bytes::new(),
                remote_addr: "127.0.0.1:1".to_string(),
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

    #[tokio::test]
    async fn test_oas_valid_request_with_params_passes() {
        let p = plugin();
        // GET /users/{id} requires ?verbose and x-trace header, path templating matches.
        let mut c = ctx("GET", "/users/42");
        c.request
            .query_params
            .insert("verbose".to_string(), vec!["true".to_string()]);
        c.request
            .headers
            .insert("x-trace".to_string(), vec!["abc".to_string()]);
        assert!(p.execute(c, &HashMap::new()).await.is_ok());
    }

    #[tokio::test]
    async fn test_oas_missing_required_query_rejected() {
        let p = plugin();
        let mut c = ctx("GET", "/users/42");
        c.request
            .headers
            .insert("x-trace".to_string(), vec!["abc".to_string()]);
        // no verbose query param
        let err = p.execute(c, &HashMap::new()).await.unwrap_err();
        assert_eq!(err.error.code, "OAS_VALIDATION_FAILED");
        assert_eq!(err.context.response.status_code, 400);
        let body: serde_json::Value = serde_json::from_slice(&err.context.response.body).unwrap();
        assert_eq!(body["error"], "oas_validation_failed");
    }

    #[tokio::test]
    async fn test_oas_missing_required_header_rejected() {
        let p = plugin();
        let mut c = ctx("GET", "/users/42");
        c.request
            .query_params
            .insert("verbose".to_string(), vec!["true".to_string()]);
        let err = p.execute(c, &HashMap::new()).await.unwrap_err();
        assert_eq!(err.error.code, "OAS_VALIDATION_FAILED");
    }

    #[tokio::test]
    async fn test_oas_valid_body_passes() {
        let p = plugin();
        let mut c = ctx("POST", "/users/42");
        c.request.headers.insert(
            "content-type".to_string(),
            vec!["application/json".to_string()],
        );
        c.request.body = Bytes::from(r#"{"name":"jack"}"#);
        assert!(p.execute(c, &HashMap::new()).await.is_ok());
    }

    #[tokio::test]
    async fn test_oas_bad_body_rejected() {
        let p = plugin();
        let mut c = ctx("POST", "/users/42");
        c.request.headers.insert(
            "content-type".to_string(),
            vec!["application/json".to_string()],
        );
        // missing required "name" (via $ref schema)
        c.request.body = Bytes::from(r#"{"age":3}"#);
        let err = p.execute(c, &HashMap::new()).await.unwrap_err();
        assert_eq!(err.error.code, "OAS_VALIDATION_FAILED");
    }

    #[tokio::test]
    async fn test_oas_required_body_missing_rejected() {
        let p = plugin();
        let c = ctx("POST", "/users/42"); // empty body, requestBody.required
        let err = p.execute(c, &HashMap::new()).await.unwrap_err();
        assert_eq!(err.error.code, "OAS_VALIDATION_FAILED");
    }

    #[tokio::test]
    async fn test_oas_non_matching_path_passes_through() {
        let p = plugin();
        // No operation for this path → pass through untouched.
        let out = p
            .execute(ctx("GET", "/nope/here"), &HashMap::new())
            .await
            .unwrap();
        assert_eq!(out.context.response.status_code, 0);
        // wrong method on a known path also passes through
        assert!(p
            .execute(ctx("DELETE", "/users/42"), &HashMap::new())
            .await
            .is_ok());
    }

    #[test]
    fn test_oas_config_rejections() {
        // missing spec
        assert!(OasValidatorPlugin::from_config(&HashMap::new()).is_err());

        // spec not an object
        let mut c = HashMap::new();
        c.insert("spec".to_string(), serde_json::json!("not an object"));
        assert!(OasValidatorPlugin::from_config(&c).is_err());

        // spec without paths
        let mut c = HashMap::new();
        c.insert(
            "spec".to_string(),
            serde_json::json!({ "openapi": "3.0.0" }),
        );
        assert!(OasValidatorPlugin::from_config(&c).is_err());

        // uncompilable body schema fails fast
        let mut c = HashMap::new();
        c.insert(
            "spec".to_string(),
            serde_json::json!({
                "paths": {
                    "/x": {
                        "post": {
                            "requestBody": {
                                "content": {
                                    "application/json": {
                                        "schema": { "type": "not-a-real-type" }
                                    }
                                }
                            }
                        }
                    }
                }
            }),
        );
        assert!(OasValidatorPlugin::from_config(&c).is_err());

        // rejected_code out of range
        let mut c = HashMap::new();
        c.insert("spec".to_string(), spec());
        c.insert("rejected_code".to_string(), serde_json::json!(200));
        assert!(OasValidatorPlugin::from_config(&c).is_err());
    }

    #[test]
    fn test_oas_path_templating_matcher() {
        let (segs, lits) = parse_template("/users/{id}/posts");
        assert_eq!(lits, 2);
        assert!(path_matches(&segs, "/users/9/posts"));
        assert!(!path_matches(&segs, "/users/9"));
        assert!(!path_matches(&segs, "/users/9/posts/1"));
    }
}
