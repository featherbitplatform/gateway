//! Shared access-log entry builder for the logger plugins.
//!
//! Every logger (http-logger, tcp-logger, elasticsearch-logger, ...) turns the
//! request [`Context`] into the same JSON entry so their output is consistent.
//! Two shapes, mirroring APISIX's `log-util`:
//!
//! - **default entry** — a structured object (`request`, `response`,
//!   `client_ip`, `latency`, `consumer`, ...) when no `log_format` is set.
//! - **custom entry** — a flat object built from a configured `log_format` map
//!   of `name -> "$var template"`, resolved with [`crate::vars::interpolate`].
//!
//! Latency is derived from the reserved `__request_start_ms` message key the
//! data-plane listener stamps at request start; it is omitted when absent
//! (e.g. in unit tests that build a Context directly).

use std::collections::HashMap;

use serde_json::{json, Map, Value};

use crate::context::Context;
use crate::vars;

/// Builds a log entry for `ctx`.
///
/// When `log_format` is `Some`, produces a flat object whose values are the
/// `$var`-interpolated templates. When `None`, produces the default structured
/// entry. `include_req_body` / `include_resp_body` add the (UTF-8 lossy)
/// bodies.
pub fn build_entry(
    ctx: &Context,
    log_format: Option<&HashMap<String, Value>>,
    include_req_body: bool,
    include_resp_body: bool,
) -> Value {
    match log_format {
        Some(fmt) => build_custom(ctx, fmt),
        None => build_default(ctx, include_req_body, include_resp_body),
    }
}

/// Parses a `log_format` config value (an object of `name -> string`) into a
/// map, or `None` when absent. Returns an error if it is present but not an
/// object of scalar values.
pub fn parse_log_format(
    config: &HashMap<String, Value>,
) -> Result<Option<HashMap<String, Value>>, String> {
    match config.get("log_format") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Object(m)) => {
            for (k, v) in m {
                if !v.is_string() && !v.is_number() && !v.is_boolean() {
                    return Err(format!("log_format['{}'] must be a scalar", k));
                }
            }
            Ok(Some(m.clone().into_iter().collect()))
        }
        Some(_) => Err("log_format must be an object of name -> template".to_string()),
    }
}

fn build_custom(ctx: &Context, fmt: &HashMap<String, Value>) -> Value {
    let mut out = Map::new();
    for (name, template) in fmt {
        let rendered = match template {
            Value::String(s) => Value::String(vars::interpolate(ctx, s)),
            other => other.clone(),
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
        let mut fmt = HashMap::new();
        fmt.insert("who".to_string(), json!("$consumer_name@$remote_addr"));
        fmt.insert("path".to_string(), json!("$uri"));
        fmt.insert("code".to_string(), json!("$status"));
        fmt.insert("const".to_string(), json!(7));
        let e = build_entry(&ctx(), Some(&fmt), false, false);
        assert_eq!(e["who"], "alice@10.0.0.5");
        assert_eq!(e["path"], "/api/items");
        assert_eq!(e["code"], "200");
        assert_eq!(e["const"], 7);
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
