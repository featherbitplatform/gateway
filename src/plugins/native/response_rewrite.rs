//! The `response-rewrite` node — rewrites the status code, body, and headers
//! of the response before it reaches the client. Port of APISIX's
//! `response-rewrite` plugin (response-phase: place after `upstream`, before
//! `client`).
//!
//! Since featherbit responses are fully buffered in `Context.response`,
//! APISIX's `header_filter` + `body_filter` phases collapse into a single
//! `execute()`.
//!
//! Deviations from APISIX:
//! - filter `options` only supports `"i"` (case-insensitive); APISIX's PCRE
//!   flags `j`/`o` are JIT/compile-cache hints with no meaning here and are
//!   rejected at config load.
//! - when filters skip because of an unsupported/undecodable
//!   `content-encoding`, the response headers are left untouched (APISIX has
//!   already cleared `content-length`/`content-encoding` at that point, which
//!   garbles the response — we deliberately do not mirror that quirk).

use async_trait::async_trait;
use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use bytes::Bytes;
use regex::Regex;
use std::collections::HashMap;

use crate::context::Context;
use crate::plugins::util::content_codec::{self, ContentEncoding};
use crate::plugins::{Plugin, PluginOutput, PluginResult};
use crate::vars;

/// Rewrites `Context.response`: optionally forces a status code, replaces the
/// body (with optional base64-decoded config content), applies regex filters
/// to the body, and adds/sets/removes headers with `$var` interpolation in
/// header values. An optional `vars` expression gates the whole node — when
/// present and false the node is a pure passthrough. This plugin never fails
/// at execution time.
pub struct ResponseRewritePlugin {
    status_code: Option<u16>,
    /// Replacement body, already base64-decoded when `body_base64` was set.
    body: Option<Bytes>,
    filters: Vec<BodyFilter>,
    add_headers: Vec<(String, String)>,
    set_headers: Vec<(String, String)>,
    remove_headers: Vec<String>,
    vars: Option<vars::Expr>,
}

/// One compiled body filter: a regex substitution applied once or globally.
struct BodyFilter {
    regex: Regex,
    replace: String,
    global: bool,
}

/// Headers the server layer derives from the final body; stale copies from
/// the upstream must be dropped whenever the body is replaced. Mirrors
/// APISIX's `core.response.clear_header_as_body_modified` (which also drops
/// the cache validators `last-modified` and `etag`).
const BODY_DERIVED_HEADERS: [&str; 4] = [
    "content-length",
    "content-encoding",
    "last-modified",
    "etag",
];

fn clear_body_derived_headers(ctx: &mut Context) {
    for name in BODY_DERIVED_HEADERS {
        crate::plugins::util::headers::remove_ci(&mut ctx.response.headers, name);
    }
}

/// Stringifies a scalar header value (string or number, matching the APISIX
/// schema); returns `None` for other shapes.
fn header_value_to_string(v: &serde_json::Value) -> Option<String> {
    match v {
        serde_json::Value::String(s) => Some(s.clone()),
        serde_json::Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

/// Splits an `add` entry of the form `"Name: value"` into `(name, value)`.
///
/// Mirrors APISIX's `^([^:\s]+)\s*:\s*([^:]+)$`: the name has no colon or
/// whitespace, and the value part must be non-empty and colon-free.
fn parse_add_entry(entry: &str) -> Result<(String, String), String> {
    let (name, value) = entry
        .split_once(':')
        .ok_or_else(|| format!("headers.add entry '{}' must be 'Name: value'", entry))?;
    let name = name.trim();
    let value = value.trim();
    if name.is_empty() || name.contains(char::is_whitespace) {
        return Err(format!(
            "headers.add entry '{}' has an invalid header name",
            entry
        ));
    }
    if value.is_empty() || value.contains(':') {
        return Err(format!(
            "headers.add entry '{}' has an invalid header value (must be non-empty, no ':')",
            entry
        ));
    }
    Ok((name.to_lowercase(), value.to_string()))
}

/// Parses the `headers` config key. Two accepted shapes:
/// - structured: `{add: ["Name: value", ...], set: {name: value}, remove: [name]}`
/// - deprecated flat map: `{name: value}` — treated as `set` (as APISIX does).
#[allow(clippy::type_complexity)]
fn parse_headers(
    raw: &serde_json::Value,
) -> Result<(Vec<(String, String)>, Vec<(String, String)>, Vec<String>), String> {
    let obj = raw
        .as_object()
        .ok_or("headers must be an object".to_string())?;

    let is_structured = obj.get("add").is_some_and(|v| v.is_array())
        || obj.get("set").is_some_and(|v| v.is_object())
        || obj.get("remove").is_some_and(|v| v.is_array());

    let mut add = Vec::new();
    let mut set = Vec::new();
    let mut remove = Vec::new();

    if is_structured {
        if let Some(entries) = obj.get("add") {
            for entry in entries.as_array().ok_or("headers.add must be an array")? {
                let s = entry
                    .as_str()
                    .ok_or("headers.add entries must be strings ('Name: value')")?;
                add.push(parse_add_entry(s)?);
            }
        }
        if let Some(map) = obj.get("set") {
            for (name, value) in map.as_object().ok_or("headers.set must be a map")? {
                let v = header_value_to_string(value)
                    .ok_or_else(|| format!("headers.set['{}'] must be a string or number", name))?;
                set.push((name.to_lowercase(), v));
            }
        }
        if let Some(names) = obj.get("remove") {
            for name in names.as_array().ok_or("headers.remove must be an array")? {
                let s = name
                    .as_str()
                    .ok_or("headers.remove entries must be strings")?;
                remove.push(s.to_lowercase());
            }
        }
    } else {
        // Deprecated flat map: every key is a `set`.
        for (name, value) in obj {
            let v = header_value_to_string(value)
                .ok_or_else(|| format!("headers['{}'] must be a string or number", name))?;
            set.push((name.to_lowercase(), v));
        }
    }

    Ok((add, set, remove))
}

/// Parses one `filters` entry into a compiled [`BodyFilter`].
fn parse_filter(v: &serde_json::Value) -> Result<BodyFilter, String> {
    let obj = v
        .as_object()
        .ok_or("filters entries must be objects".to_string())?;

    let pattern = obj
        .get("regex")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .ok_or("filters entries require a non-empty 'regex' string")?;

    let replace = obj
        .get("replace")
        .and_then(|v| v.as_str())
        .ok_or("filters entries require a 'replace' string")?
        .to_string();

    let global = match obj.get("scope").and_then(|v| v.as_str()) {
        None | Some("once") => false,
        Some("global") => true,
        Some(other) => {
            return Err(format!(
                "filters scope must be 'once' or 'global', got '{}'",
                other
            ))
        }
    };

    let options = obj.get("options").and_then(|v| v.as_str()).unwrap_or("");
    let case_insensitive = match options {
        "" => false,
        "i" => true,
        other => {
            return Err(format!(
                "filters options only supports 'i' (case-insensitive), got '{}'",
                other
            ))
        }
    };

    let full_pattern = if case_insensitive {
        format!("(?i){}", pattern)
    } else {
        pattern.to_string()
    };
    let regex = Regex::new(&full_pattern)
        .map_err(|e| format!("filters regex \"{}\" validation failed: {}", pattern, e))?;

    Ok(BodyFilter {
        regex,
        replace,
        global,
    })
}

impl ResponseRewritePlugin {
    /// Builds the plugin from node config.
    ///
    /// Accepted keys (all optional):
    /// - `status_code` (integer, 200-598): new response status code.
    /// - `body` (string): new response body. Wins over `filters` — configuring
    ///   both is rejected (as in APISIX).
    /// - `body_base64` (bool, default `false`): when true, `body` is decoded
    ///   from base64 at config load; invalid or empty base64 content fails
    ///   here.
    /// - `headers` — either the structured shape or the deprecated flat map:
    ///   - `add` (array of `"Name: value"` strings): appended alongside
    ///     existing values. The value must be non-empty and colon-free.
    ///   - `set` (map `{name: value}`): replaces existing values. Values may
    ///     be strings or numbers.
    ///   - `remove` (array of names): deleted.
    ///   - flat map `{name: value}` (deprecated): treated as `set`.
    ///
    ///   `add`/`set` values support `$var` / `${var}` interpolation at
    ///   execution time (e.g. `$remote_addr`, `$status`, `$http_x_id`).
    /// - `filters` (array): regex substitutions applied to the response body.
    ///   Each entry: `regex` (required, non-empty), `replace` (required),
    ///   `scope` (`once` | `global`, default `once`), `options` (only `"i"`
    ///   for case-insensitive; anything else is rejected). Regexes are
    ///   compiled here, so invalid patterns fail at config load.
    /// - `vars` (array, APISIX triple-array expression): gate — when present
    ///   and it evaluates to false, the node passes the context through
    ///   unchanged.
    ///
    /// ```yaml
    /// type: response-rewrite
    /// config:
    ///   status_code: 200
    ///   headers:
    ///     set:
    ///       x-server-id: "3"
    ///     add:
    ///       - "x-trace: $http_x_request_id"
    ///     remove: [x-powered-by]
    ///   filters:
    ///     - regex: "X-Amzn-"
    ///       scope: global
    ///       replace: ""
    ///   vars:
    ///     - ["status", "==", "200"]
    /// ```
    pub fn from_config(config: &HashMap<String, serde_json::Value>) -> Result<Self, String> {
        // A common mix-up: `proxy-rewrite` takes `add_headers`/`remove_headers`,
        // but `response-rewrite` takes a single `headers` object. Silently
        // ignoring the misplaced key produces a node that appears to do nothing,
        // so reject it with the correct shape instead.
        for wrong in ["add_headers", "set_headers", "remove_headers"] {
            if config.contains_key(wrong) {
                return Err(format!(
                    "response-rewrite has no '{wrong}' key (that is proxy-rewrite's schema); \
                     use headers with add/set/remove, e.g. \
                     headers: {{ set: {{ x-foo: bar }}, remove: [x-powered-by] }}"
                ));
            }
        }

        let status_code = match config.get("status_code") {
            None => None,
            Some(v) => {
                let code = v
                    .as_u64()
                    .ok_or("status_code must be an integer".to_string())?;
                if !(200..=598).contains(&code) {
                    return Err(format!("status_code must be within 200-598, got {}", code));
                }
                Some(code as u16)
            }
        };

        let body_base64 = config
            .get("body_base64")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        let body = match config.get("body") {
            None => {
                if body_base64 {
                    return Err("body_base64 requires 'body' to be set".to_string());
                }
                None
            }
            Some(v) => {
                let s = v.as_str().ok_or("body must be a string".to_string())?;
                if body_base64 {
                    if s.is_empty() {
                        return Err("invalid base64 content".to_string());
                    }
                    let decoded = BASE64
                        .decode(s.trim())
                        .map_err(|_| "invalid base64 content".to_string())?;
                    Some(Bytes::from(decoded))
                } else {
                    Some(Bytes::from(s.to_string()))
                }
            }
        };

        let filters = match config.get("filters") {
            None => Vec::new(),
            Some(v) => {
                let arr = v.as_array().ok_or("filters must be an array".to_string())?;
                if arr.is_empty() {
                    return Err("filters must contain at least one entry".to_string());
                }
                arr.iter()
                    .map(parse_filter)
                    .collect::<Result<Vec<_>, _>>()?
            }
        };

        if body.is_some() && !filters.is_empty() {
            return Err("'body' and 'filters' are mutually exclusive".to_string());
        }

        let (add_headers, set_headers, remove_headers) = match config.get("headers") {
            None => (Vec::new(), Vec::new(), Vec::new()),
            Some(raw) => parse_headers(raw)?,
        };

        let vars = match config.get("vars") {
            None => None,
            Some(v) => Some(
                vars::Expr::parse(v)
                    .map_err(|e| format!("failed to validate the 'vars' expression: {}", e))?,
            ),
        };

        Ok(Self {
            status_code,
            body,
            filters,
            add_headers,
            set_headers,
            remove_headers,
            vars,
        })
    }

    /// Runs the configured regex filters against the response body, decoding
    /// a content-encoded body first. On any obstacle (unsupported encoding,
    /// corrupt compressed data, non-UTF-8 body) it logs a warning and leaves
    /// the response untouched, matching APISIX's "filters may not work as
    /// expected" behavior.
    fn apply_filters(&self, ctx: &mut Context) {
        let encoding_header = ctx
            .response
            .headers
            .get("content-encoding")
            .and_then(|v| v.first())
            .cloned()
            .unwrap_or_default();

        let decoded = match ContentEncoding::parse(&encoding_header) {
            Err(e) => {
                tracing::warn!(
                    "response-rewrite: filters skipped due to unsupported \
                     compression encoding: {}",
                    e
                );
                return;
            }
            Ok(None) => ctx.response.body.clone(),
            Ok(Some(encoding)) => match content_codec::decode(&encoding, &ctx.response.body) {
                Ok(decoded) => decoded,
                Err(e) => {
                    tracing::warn!("response-rewrite: filters skipped: {}", e);
                    return;
                }
            },
        };

        let mut text = match std::str::from_utf8(&decoded) {
            Ok(s) => s.to_string(),
            Err(_) => {
                tracing::warn!("response-rewrite: filters skipped: body is not valid UTF-8");
                return;
            }
        };

        for filter in &self.filters {
            text = if filter.global {
                filter.regex.replace_all(&text, filter.replace.as_str())
            } else {
                filter.regex.replace(&text, filter.replace.as_str())
            }
            .into_owned();
        }

        // The body is left decoded (as in APISIX), so all body-derived
        // headers — including the stale content-encoding — must go.
        ctx.response.body = Bytes::from(text);
        clear_body_derived_headers(ctx);
    }
}

#[async_trait]
impl Plugin for ResponseRewritePlugin {
    fn plugin_type(&self) -> &str {
        "response-rewrite"
    }

    async fn execute(
        &self,
        mut ctx: Context,
        _named_inputs: &HashMap<String, serde_json::Value>,
    ) -> PluginResult {
        // `vars` gate: when configured and false, the node is a no-op.
        if let Some(expr) = &self.vars {
            if !expr.eval(&ctx) {
                return Ok(PluginOutput {
                    context: ctx,
                    named_outputs: HashMap::new(),
                });
            }
        }

        if let Some(code) = self.status_code {
            ctx.response.status_code = code;
        }

        if let Some(body) = &self.body {
            ctx.response.body = body.clone();
            clear_body_derived_headers(&mut ctx);
        } else if !self.filters.is_empty() {
            self.apply_filters(&mut ctx);
        }

        // Header ops in APISIX order: add, set, remove. Values are
        // interpolated against the context ($status, $http_<name>, ...).
        for (name, value) in &self.add_headers {
            let value = vars::interpolate(&ctx, value);
            ctx.response
                .headers
                .entry(name.clone())
                .or_default()
                .push(value);
        }
        for (name, value) in &self.set_headers {
            let value = vars::interpolate(&ctx, value);
            ctx.response.headers.insert(name.clone(), vec![value]);
        }
        for name in &self.remove_headers {
            crate::plugins::util::headers::remove_ci(&mut ctx.response.headers, name);
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

    fn test_context(status: u16, body: &[u8]) -> Context {
        let mut response_headers = HashMap::new();
        response_headers.insert("content-length".to_string(), vec![body.len().to_string()]);
        Context {
            request: GatewayRequest {
                method: "GET".to_string(),
                path: "/test".to_string(),
                host: "localhost".to_string(),
                scheme: "http".to_string(),
                headers: HashMap::new(),
                query_params: HashMap::new(),
                body: Bytes::new(),
                remote_addr: "127.0.0.1:12345".to_string(),
                protocol: Protocol::Http1,
            },
            response: GatewayResponse {
                status_code: status,
                headers: response_headers,
                body: Bytes::copy_from_slice(body),
            },
            message: HashMap::new(),
            errors: Vec::new(),
        }
    }

    fn plugin(config: serde_json::Value) -> ResponseRewritePlugin {
        let map: HashMap<String, serde_json::Value> =
            serde_json::from_value(config).expect("test config must be an object");
        ResponseRewritePlugin::from_config(&map).expect("config should be valid")
    }

    #[tokio::test]
    async fn test_response_rewrite_status_and_body() {
        let p = plugin(serde_json::json!({
            "status_code": 404,
            "body": "not found\n"
        }));
        let ctx = test_context(200, b"original");
        let out = p.execute(ctx, &HashMap::new()).await.unwrap();
        assert_eq!(out.context.response.status_code, 404);
        assert_eq!(out.context.response.body.as_ref(), b"not found\n");
        // Body-mutation convention: stale content-length is gone.
        assert!(!out.context.response.headers.contains_key("content-length"));
    }

    #[tokio::test]
    async fn test_response_rewrite_body_base64() {
        let p = plugin(serde_json::json!({
            "body": "aGVsbG8gd29ybGQ=",
            "body_base64": true
        }));
        let ctx = test_context(200, b"x");
        let out = p.execute(ctx, &HashMap::new()).await.unwrap();
        assert_eq!(out.context.response.body.as_ref(), b"hello world");
    }

    #[test]
    fn test_response_rewrite_invalid_base64_rejected() {
        let mut config = HashMap::new();
        config.insert("body".to_string(), serde_json::json!("not!!valid@@base64"));
        config.insert("body_base64".to_string(), serde_json::json!(true));
        assert!(ResponseRewritePlugin::from_config(&config).is_err());

        // body_base64 without a body is also invalid.
        let mut config = HashMap::new();
        config.insert("body_base64".to_string(), serde_json::json!(true));
        assert!(ResponseRewritePlugin::from_config(&config).is_err());
    }

    #[test]
    fn test_response_rewrite_config_validation() {
        // status_code out of range
        let mut config = HashMap::new();
        config.insert("status_code".to_string(), serde_json::json!(199));
        assert!(ResponseRewritePlugin::from_config(&config).is_err());
        let mut config = HashMap::new();
        config.insert("status_code".to_string(), serde_json::json!(599));
        assert!(ResponseRewritePlugin::from_config(&config).is_err());

        // body and filters are mutually exclusive
        let mut config = HashMap::new();
        config.insert("body".to_string(), serde_json::json!("x"));
        config.insert(
            "filters".to_string(),
            serde_json::json!([{ "regex": "a", "replace": "b" }]),
        );
        assert!(ResponseRewritePlugin::from_config(&config).is_err());

        // invalid regex fails at load
        let mut config = HashMap::new();
        config.insert(
            "filters".to_string(),
            serde_json::json!([{ "regex": "(", "replace": "" }]),
        );
        assert!(ResponseRewritePlugin::from_config(&config).is_err());

        // unsupported regex options rejected (only "i" is supported)
        let mut config = HashMap::new();
        config.insert(
            "filters".to_string(),
            serde_json::json!([{ "regex": "a", "replace": "b", "options": "jo" }]),
        );
        assert!(ResponseRewritePlugin::from_config(&config).is_err());

        // malformed add entry (colon in value) rejected
        let mut config = HashMap::new();
        config.insert(
            "headers".to_string(),
            serde_json::json!({ "add": ["x-key: a:b"] }),
        );
        assert!(ResponseRewritePlugin::from_config(&config).is_err());
    }

    /// proxy-rewrite's header keys must not be silently ignored here — that
    /// produces a node that looks like it does nothing. Fail with guidance.
    #[test]
    fn test_proxy_rewrite_header_keys_rejected_with_guidance() {
        for wrong in ["add_headers", "set_headers", "remove_headers"] {
            let mut config = HashMap::new();
            config.insert(wrong.to_string(), serde_json::json!({ "x-foo": "bar" }));
            let err = ResponseRewritePlugin::from_config(&config)
                .err()
                .unwrap_or_else(|| panic!("{wrong} should be rejected"));
            assert!(err.contains(wrong), "error should name the bad key: {err}");
            assert!(
                err.contains("headers"),
                "error should point to the right shape: {err}"
            );
        }
        // The correct shape still builds.
        let mut ok = HashMap::new();
        ok.insert(
            "headers".to_string(),
            serde_json::json!({ "set": { "x-foo": "bar" } }),
        );
        assert!(ResponseRewritePlugin::from_config(&ok).is_ok());
    }

    #[tokio::test]
    async fn test_response_rewrite_headers_add_set_remove() {
        let p = plugin(serde_json::json!({
            "headers": {
                "add": ["X-Trace: abc"],
                "set": { "X-Server": "featherbit", "x-version": 3 },
                "remove": ["X-Powered-By"]
            }
        }));
        let mut ctx = test_context(200, b"body");
        ctx.response
            .headers
            .insert("x-powered-by".to_string(), vec!["php".to_string()]);
        ctx.response
            .headers
            .insert("x-trace".to_string(), vec!["existing".to_string()]);
        ctx.response
            .headers
            .insert("x-server".to_string(), vec!["nginx".to_string()]);

        let out = p.execute(ctx, &HashMap::new()).await.unwrap();
        let headers = &out.context.response.headers;
        // add appends alongside the existing value
        assert_eq!(
            headers.get("x-trace"),
            Some(&vec!["existing".to_string(), "abc".to_string()])
        );
        // set replaces
        assert_eq!(
            headers.get("x-server"),
            Some(&vec!["featherbit".to_string()])
        );
        assert_eq!(headers.get("x-version"), Some(&vec!["3".to_string()]));
        // remove deletes
        assert!(!headers.contains_key("x-powered-by"));
        // body untouched → content-length kept
        assert!(headers.contains_key("content-length"));
    }

    #[tokio::test]
    async fn test_response_rewrite_deprecated_flat_headers_map() {
        let p = plugin(serde_json::json!({
            "headers": { "X-Flat": "yes" }
        }));
        let ctx = test_context(200, b"body");
        let out = p.execute(ctx, &HashMap::new()).await.unwrap();
        assert_eq!(
            out.context.response.headers.get("x-flat"),
            Some(&vec!["yes".to_string()])
        );
    }

    #[tokio::test]
    async fn test_response_rewrite_header_var_interpolation() {
        let p = plugin(serde_json::json!({
            "headers": {
                "set": { "x-origin": "$remote_addr", "x-status": "$status" }
            }
        }));
        let ctx = test_context(201, b"body");
        let out = p.execute(ctx, &HashMap::new()).await.unwrap();
        assert_eq!(
            out.context.response.headers.get("x-origin"),
            Some(&vec!["127.0.0.1".to_string()])
        );
        assert_eq!(
            out.context.response.headers.get("x-status"),
            Some(&vec!["201".to_string()])
        );
    }

    #[tokio::test]
    async fn test_response_rewrite_filters_once_and_global() {
        let p = plugin(serde_json::json!({
            "filters": [{ "regex": "foo", "replace": "bar" }]
        }));
        let ctx = test_context(200, b"foo foo foo");
        let out = p.execute(ctx, &HashMap::new()).await.unwrap();
        assert_eq!(out.context.response.body.as_ref(), b"bar foo foo");
        assert!(!out.context.response.headers.contains_key("content-length"));

        let p = plugin(serde_json::json!({
            "filters": [{ "regex": "FOO", "replace": "bar", "scope": "global", "options": "i" }]
        }));
        let ctx = test_context(200, b"foo Foo fOO");
        let out = p.execute(ctx, &HashMap::new()).await.unwrap();
        assert_eq!(out.context.response.body.as_ref(), b"bar bar bar");
    }

    #[tokio::test]
    async fn test_response_rewrite_filters_decode_gzip_body() {
        let plain = Bytes::from_static(b"hello encoded world");
        let compressed = content_codec::encode(&ContentEncoding::Gzip, &plain, 6).unwrap();

        let p = plugin(serde_json::json!({
            "filters": [{ "regex": "encoded", "replace": "decoded" }]
        }));
        let mut ctx = test_context(200, &compressed);
        ctx.response
            .headers
            .insert("content-encoding".to_string(), vec!["gzip".to_string()]);
        ctx.response
            .headers
            .insert("etag".to_string(), vec!["\"abc\"".to_string()]);

        let out = p.execute(ctx, &HashMap::new()).await.unwrap();
        // Body is decoded, filtered, and left decoded (as in APISIX).
        assert_eq!(out.context.response.body.as_ref(), b"hello decoded world");
        let headers = &out.context.response.headers;
        assert!(!headers.contains_key("content-encoding"));
        assert!(!headers.contains_key("content-length"));
        assert!(!headers.contains_key("etag"));
    }

    #[tokio::test]
    async fn test_response_rewrite_filters_skip_unsupported_encoding() {
        let p = plugin(serde_json::json!({
            "filters": [{ "regex": "foo", "replace": "bar" }]
        }));
        let mut ctx = test_context(200, b"foo body");
        ctx.response
            .headers
            .insert("content-encoding".to_string(), vec!["zstd".to_string()]);

        let out = p.execute(ctx, &HashMap::new()).await.unwrap();
        // Unsupported encoding: body and headers untouched.
        assert_eq!(out.context.response.body.as_ref(), b"foo body");
        assert_eq!(
            out.context.response.headers.get("content-encoding"),
            Some(&vec!["zstd".to_string()])
        );
        assert!(out.context.response.headers.contains_key("content-length"));
    }

    #[tokio::test]
    async fn test_response_rewrite_filters_skip_corrupt_encoded_body() {
        let p = plugin(serde_json::json!({
            "filters": [{ "regex": "foo", "replace": "bar" }]
        }));
        let mut ctx = test_context(200, b"\x00not gzip\xff");
        ctx.response
            .headers
            .insert("content-encoding".to_string(), vec!["gzip".to_string()]);

        let out = p.execute(ctx, &HashMap::new()).await.unwrap();
        assert_eq!(out.context.response.body.as_ref(), b"\x00not gzip\xff");
        assert!(out
            .context
            .response
            .headers
            .contains_key("content-encoding"));
    }

    #[tokio::test]
    async fn test_response_rewrite_vars_gate() {
        let config = serde_json::json!({
            "status_code": 500,
            "body": "rewritten",
            "headers": { "set": { "x-hit": "1" } },
            "vars": [["status", "==", "200"]]
        });

        // Gate matches → rewrite applies.
        let p = plugin(config.clone());
        let out = p
            .execute(test_context(200, b"orig"), &HashMap::new())
            .await
            .unwrap();
        assert_eq!(out.context.response.status_code, 500);
        assert_eq!(out.context.response.body.as_ref(), b"rewritten");

        // Gate does not match → complete passthrough.
        let p = plugin(config);
        let out = p
            .execute(test_context(404, b"orig"), &HashMap::new())
            .await
            .unwrap();
        assert_eq!(out.context.response.status_code, 404);
        assert_eq!(out.context.response.body.as_ref(), b"orig");
        assert!(!out.context.response.headers.contains_key("x-hit"));
        assert!(out.context.response.headers.contains_key("content-length"));
    }
}
