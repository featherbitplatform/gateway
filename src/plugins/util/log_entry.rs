//! Shared access-log entry builder for the logger plugins.
//!
//! Every logger (http-logger, tcp-logger, elasticsearch-logger, ...) turns the
//! request [`Context`] into the same JSON entry so their output is consistent.
//! Two shapes, mirroring APISIX's `log-util`:
//!
//! - **default entry** — a structured object (`request`, `response`,
//!   `client_ip`, `latency`, `consumer`, ...) when no `log_format` is set.
//! - **custom entry** — a flat object built from a configured `log_format` map
//!   of `name -> "template"`, each pre-parsed into a [`Template`] at config
//!   load and rendered per request via
//!   [`Template::render_with_legacy`] (supports `{{namespace.path}}`
//!   references plus legacy `$var` interpolation).
//!
//! Latency is derived from the reserved `__request_start_ms` message key the
//! data-plane listener stamps at request start; it is omitted when absent
//! (e.g. in unit tests that build a Context directly).

use std::collections::HashMap;

use serde_json::{json, Map, Value};

use crate::context::Context;
use crate::vars::template::Template;

/// One entry in a parsed [`LogFormat`]: a string value is pre-parsed into a
/// [`Template`] at config-load time (one parse, per-request render); a
/// number/boolean scalar is kept as-is and rendered verbatim.
#[derive(Debug, Clone)]
pub enum LogFormatValue {
    /// Supports `{{namespace.path}}` references and legacy `$var`
    /// interpolation (see [`Template::render_with_legacy`]).
    Template(Template),
    /// A non-string scalar (number/bool), used verbatim.
    Literal(Value),
}

/// A parsed `log_format`: entry name -> pre-parsed value.
pub type LogFormat = HashMap<String, LogFormatValue>;

/// Builds a log entry for `ctx`.
///
/// When `log_format` is `Some`, produces a flat object whose string entries
/// are rendered from their pre-parsed templates. When `None`, produces the
/// default structured entry. `include_req_body` / `include_resp_body` add the
/// (UTF-8 lossy) bodies.
pub fn build_entry(
    ctx: &Context,
    log_format: Option<&LogFormat>,
    include_req_body: bool,
    include_resp_body: bool,
) -> Value {
    match log_format {
        Some(fmt) => build_custom(ctx, fmt),
        None => build_default(ctx, include_req_body, include_resp_body),
    }
}

/// Parses a `log_format` config value (an object of `name -> string`) into a
/// [`LogFormat`], or `None` when absent. Returns an error if it is present but
/// not an object of scalar values. String values are pre-parsed into
/// [`Template`]s here (warnings discarded — the compile-time walk, a later
/// task, reports them); number/boolean values pass through unchanged.
pub fn parse_log_format(config: &HashMap<String, Value>) -> Result<Option<LogFormat>, String> {
    match config.get("log_format") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Object(m)) => {
            let mut out = HashMap::with_capacity(m.len());
            for (k, v) in m {
                let entry = match v {
                    Value::String(s) => LogFormatValue::Template(Template::parse(s).0),
                    Value::Number(_) | Value::Bool(_) => LogFormatValue::Literal(v.clone()),
                    _ => return Err(format!("log_format['{}'] must be a scalar", k)),
                };
                out.insert(k.clone(), entry);
            }
            Ok(Some(out))
        }
        Some(_) => Err("log_format must be an object of name -> template".to_string()),
    }
}

fn build_custom(ctx: &Context, fmt: &LogFormat) -> Value {
    let mut out = Map::new();
    for (name, entry) in fmt {
        let rendered = match entry {
            LogFormatValue::Template(tpl) => Value::String(tpl.render_with_legacy(ctx)),
            LogFormatValue::Literal(v) => v.clone(),
        };
        out.insert(name.clone(), rendered);
    }
    Value::Object(out)
}

fn build_default(ctx: &Context, include_req_body: bool, include_resp_body: bool) -> Value {
    let mut request = json!({
        "method": ctx.request.method,
        "uri": request_uri(ctx),
        "host": ctx.request.host,
        "scheme": ctx.request.scheme,
        "headers": headers_to_json(&ctx.request.headers),
        "size": ctx.request.body.len(),
    });
    if include_req_body {
        request["body"] = json!(String::from_utf8_lossy(&ctx.request.body));
    }

    let mut response = json!({
        "status": ctx.response.status_code,
        "headers": headers_to_json(&ctx.response.headers),
        "size": ctx.response.body.len(),
    });
    if include_resp_body {
        response["body"] = json!(String::from_utf8_lossy(&ctx.response.body));
    }

    let mut entry = Map::new();
    entry.insert("request".to_string(), request);
    entry.insert("response".to_string(), response);
    entry.insert(
        "client_ip".to_string(),
        json!(client_ip(&ctx.request.remote_addr)),
    );
    if let Some(latency) = latency_ms(ctx) {
        entry.insert("latency".to_string(), json!(latency));
    }
    if let Some(start) = ctx
        .message
        .get("__request_start_ms")
        .and_then(|v| v.as_u64())
    {
        entry.insert("start_time".to_string(), json!(start));
    }
    if let Some(consumer) = ctx.message.get("consumer.name") {
        entry.insert("consumer".to_string(), consumer.clone());
    }
    if !ctx.errors.is_empty() {
        entry.insert(
            "errors".to_string(),
            json!(ctx
                .errors
                .iter()
                .map(|e| json!({ "node_id": e.node_id, "code": e.code, "message": e.message }))
                .collect::<Vec<_>>()),
        );
    }
    Value::Object(entry)
}

/// Milliseconds elapsed since the listener stamped `__request_start_ms`.
fn latency_ms(ctx: &Context) -> Option<u64> {
    let start = ctx.message.get("__request_start_ms")?.as_u64()?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_millis() as u64;
    Some(now.saturating_sub(start))
}

/// Header map → JSON object of `name -> [values]`.
fn headers_to_json(headers: &HashMap<String, Vec<String>>) -> Value {
    let mut m = Map::new();
    for (k, v) in headers {
        m.insert(k.clone(), json!(v));
    }
    Value::Object(m)
}

/// Path plus sorted query string.
fn request_uri(ctx: &Context) -> String {
    let mut pairs: Vec<String> = Vec::new();
    for (k, values) in &ctx.request.query_params {
        for v in values {
            pairs.push(format!("{}={}", k, v));
        }
    }
    pairs.sort();
    if pairs.is_empty() {
        ctx.request.path.clone()
    } else {
        format!("{}?{}", ctx.request.path, pairs.join("&"))
    }
}

/// Client IP without the port.
fn client_ip(remote_addr: &str) -> String {
    match remote_addr.rsplit_once(':') {
        Some((ip, _)) if !ip.contains(':') => ip.to_string(),
        _ => remote_addr.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::{GatewayRequest, GatewayResponse, Protocol};
    use bytes::Bytes;

    fn ctx() -> Context {
        let mut headers = HashMap::new();
        headers.insert("user-agent".to_string(), vec!["curl/8".to_string()]);
        let mut query = HashMap::new();
        query.insert("q".to_string(), vec!["x".to_string()]);
        let mut message = HashMap::new();
        message.insert("consumer.name".to_string(), json!("alice"));
        Context {
            request: GatewayRequest {
                method: "GET".to_string(),
                path: "/api/items".to_string(),
                host: "example.com".to_string(),
                scheme: "https".to_string(),
                headers,
                query_params: query,
                body: Bytes::from_static(b"req"),
                remote_addr: "10.0.0.5:44321".to_string(),
                protocol: Protocol::Http1,
            },
            response: GatewayResponse {
                status_code: 200,
                headers: HashMap::new(),
                body: Bytes::from_static(b"hello"),
            },
            message,
            errors: Vec::new(),
        }
    }

    #[test]
    fn test_default_entry() {
        let e = build_entry(&ctx(), None, false, false);
        assert_eq!(e["request"]["method"], "GET");
        assert_eq!(e["request"]["uri"], "/api/items?q=x");
        assert_eq!(e["request"]["size"], 3);
        assert_eq!(e["response"]["status"], 200);
        assert_eq!(e["response"]["size"], 5);
        assert_eq!(e["client_ip"], "10.0.0.5");
        assert_eq!(e["consumer"], "alice");
        assert!(e.get("body").is_none());
    }

    #[test]
    fn test_default_entry_with_bodies() {
        let e = build_entry(&ctx(), None, true, true);
        assert_eq!(e["request"]["body"], "req");
        assert_eq!(e["response"]["body"], "hello");
    }

    #[test]
    fn test_custom_log_format() {
        let mut config = HashMap::new();
        config.insert(
            "log_format".to_string(),
            json!({
                "who": "$consumer_name@$remote_addr",
                "path": "$uri",
                "code": "$status",
                "const": 7,
            }),
        );
        let fmt = parse_log_format(&config).unwrap().unwrap();
        let e = build_entry(&ctx(), Some(&fmt), false, false);
        assert_eq!(e["who"], "alice@10.0.0.5");
        assert_eq!(e["path"], "/api/items");
        assert_eq!(e["code"], "200");
        assert_eq!(e["const"], 7);
    }

    #[test]
    fn test_custom_log_format_superset_template_and_legacy_dollar() {
        // `log_format` values must render both the new `{{...}}` template
        // syntax and the legacy `$var` syntax in the same value.
        let mut config = HashMap::new();
        config.insert(
            "log_format".to_string(),
            json!({ "combo": "{{request.method}} $uri" }),
        );
        let fmt = parse_log_format(&config).unwrap().unwrap();
        let e = build_entry(&ctx(), Some(&fmt), false, false);
        assert_eq!(e["combo"], "GET /api/items");
    }

    #[test]
    fn test_parse_log_format() {
        let mut config = HashMap::new();
        assert!(parse_log_format(&config).unwrap().is_none());
        config.insert("log_format".to_string(), json!({ "a": "$uri" }));
        assert!(parse_log_format(&config).unwrap().is_some());
        config.insert("log_format".to_string(), json!("not an object"));
        assert!(parse_log_format(&config).is_err());
        config.insert("log_format".to_string(), json!({ "a": { "nested": 1 } }));
        assert!(parse_log_format(&config).is_err());
    }
}
