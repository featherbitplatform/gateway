//! Request/response variable resolution and condition expressions.
//!
//! The featherbit analogue of APISIX's `ctx.var` + `lua-resty-expr`: plugins
//! resolve named variables against the [`Context`] (`uri`, `arg_<name>`,
//! `http_<header>`, ...), interpolate `$var` / `${var}` templates, and
//! evaluate condition expressions written in APISIX's triple-array form:
//!
//! ```yaml
//! vars:
//!   - ["arg_name", "==", "jack"]
//!   - ["http_user_agent", "~~", "Mozilla.+"]
//! ```
//!
//! Rules in a top-level list are ANDed. A rule is `[var, op, value]` or the
//! negated `[var, "!", op, value]`; nested logic uses `["AND", rule...]` /
//! `["OR", rule...]`. Supported operators: `==`, `~=`, `>`, `>=`, `<`, `<=`,
//! `~~` (regex), `~*` (case-insensitive regex), `in` (value array), `has`
//! (var array contains value), `ipmatch` (value is a list of IPs/CIDRs).

use std::borrow::Cow;
use std::net::IpAddr;

use ipnet::IpNet;
use regex::Regex;

use crate::context::Context;

/// Resolves a variable name against the context.
///
/// Supported names (mirroring APISIX's `ctx.var` where featherbit has an
/// equivalent):
/// - `uri` — request path (no query string)
/// - `request_uri` — path plus `?query` when query params exist
/// - `method`, `host`, `scheme`, `protocol`
/// - `remote_addr` — client IP without port; `remote_port` — the port
/// - `query_string` — rebuilt from `query_params`
/// - `status` — response status code
/// - `resp_body` — response body (lossy UTF-8)
/// - `arg_<name>` — first query parameter value
/// - `http_<name>` — first request header value (underscores map to dashes)
/// - `cookie_<name>` — value from the `Cookie` request header
/// - `post_arg_<name>` — form field, only for
///   `application/x-www-form-urlencoded` request bodies
/// - `consumer_name`, `consumer_group_id` — from `message["consumer.name"]` /
///   `message["consumer.group"]` (set by auth plugins)
/// - `msg_<key>` — any `context.message` key (stringified)
///
/// Returns `None` for unknown names and for known prefixes whose subject is
/// absent (missing header, missing query param, ...).
pub fn resolve<'a>(ctx: &'a Context, name: &str) -> Option<Cow<'a, str>> {
    match name {
        "uri" => Some(Cow::Borrowed(ctx.request.path.as_str())),
        "request_uri" => {
            let qs = query_string(ctx);
            if qs.is_empty() {
                Some(Cow::Borrowed(ctx.request.path.as_str()))
            } else {
                Some(Cow::Owned(format!("{}?{}", ctx.request.path, qs)))
            }
        }
        "method" | "request_method" => Some(Cow::Borrowed(ctx.request.method.as_str())),
        "host" => Some(Cow::Borrowed(ctx.request.host.as_str())),
        "scheme" => Some(Cow::Borrowed(ctx.request.scheme.as_str())),
        "protocol" => Some(Cow::Owned(
            format!("{:?}", ctx.request.protocol).to_lowercase(),
        )),
        "remote_addr" => Some(Cow::Borrowed(split_remote_addr(&ctx.request.remote_addr).0)),
        "remote_port" => {
            let port = split_remote_addr(&ctx.request.remote_addr).1?;
            Some(Cow::Borrowed(port))
        }
        "query_string" => {
            let qs = query_string(ctx);
            if qs.is_empty() {
                None
            } else {
                Some(Cow::Owned(qs))
            }
        }
        "status" => Some(Cow::Owned(ctx.response.status_code.to_string())),
        "resp_body" => Some(Cow::Owned(
            String::from_utf8_lossy(&ctx.response.body).into_owned(),
        )),
        "consumer_name" => message_str(ctx, "consumer.name"),
        "consumer_group_id" => message_str(ctx, "consumer.group"),
        _ => {
            if let Some(arg) = name.strip_prefix("arg_") {
                ctx.request
                    .query_params
                    .get(arg)
                    .and_then(|v| v.first())
                    .map(|v| Cow::Borrowed(v.as_str()))
            } else if let Some(header) = name.strip_prefix("http_") {
                let header = header.replace('_', "-").to_lowercase();
                ctx.request
                    .headers
                    .get(&header)
                    .and_then(|v| v.first())
                    .map(|v| Cow::Borrowed(v.as_str()))
            } else if let Some(cookie) = name.strip_prefix("cookie_") {
                cookie_value(ctx, cookie).map(Cow::Owned)
            } else if let Some(field) = name.strip_prefix("post_arg_") {
                post_arg(ctx, field).map(Cow::Owned)
            } else if let Some(key) = name.strip_prefix("msg_") {
                message_str(ctx, key)
            } else {
                None
            }
        }
    }
}

/// Interpolates `$var` and `${var}` references in a template.
///
/// Unknown or absent variables resolve to the empty string (matching
/// APISIX's `resolve_var`). A literal `$` not followed by `[A-Za-z_{]`
/// passes through unchanged; there is no escape syntax.
pub fn interpolate(ctx: &Context, template: &str) -> String {
    let mut out = String::with_capacity(template.len());
    let bytes = template.as_bytes();
    let mut i = 0;

    while i < bytes.len() {
        if bytes[i] != b'$' {
            let start = i;
            while i < bytes.len() && bytes[i] != b'$' {
                i += 1;
            }
            out.push_str(&template[start..i]);
            continue;
        }

        // At a '$'
        if i + 1 < bytes.len() && bytes[i + 1] == b'{' {
            if let Some(end) = template[i + 2..].find('}') {
                let name = &template[i + 2..i + 2 + end];
                if let Some(v) = resolve(ctx, name) {
                    out.push_str(&v);
                }
                i += 2 + end + 1;
                continue;
            }
            out.push('$');
            i += 1;
        } else {
            let start = i + 1;
            let mut end = start;
            while end < bytes.len() && (bytes[end].is_ascii_alphanumeric() || bytes[end] == b'_') {
                end += 1;
            }
            if end == start {
                out.push('$');
                i += 1;
                continue;
            }
            if let Some(v) = resolve(ctx, &template[start..end]) {
                out.push_str(&v);
            }
            i = end;
        }
    }

    out
}

/// A compiled condition expression in APISIX's triple-array form.
///
/// Parsed once at config load ([`Expr::parse`]); regexes are compiled at
/// parse time so evaluation is allocation-light.
pub struct Expr {
    root: Node,
}

enum Node {
    And(Vec<Node>),
    Or(Vec<Node>),
    Rule { var: String, negate: bool, op: Op },
}

enum Op {
    Eq(String),
    Ne(String),
    Gt(f64),
    Ge(f64),
    Lt(f64),
    Le(f64),
    Regex(Regex),
    In(Vec<String>),
    Has(String),
    IpMatch(Vec<IpNet>),
}

impl Expr {
    /// Parses the APISIX `vars` shape: a JSON array of rules, ANDed.
    ///
    /// Each rule is `[var, op, value]`, `[var, "!", op, value]`, or a nested
    /// `["AND"|"OR", rule...]`. Fails with a descriptive message on unknown
    /// operators, malformed rules, invalid regexes, or invalid CIDRs.
    pub fn parse(v: &serde_json::Value) -> Result<Self, String> {
        let rules = v.as_array().ok_or("vars must be an array of rules")?;
        Ok(Self {
            root: Node::And(
                rules
                    .iter()
                    .map(parse_node)
                    .collect::<Result<Vec<_>, _>>()?,
            ),
        })
    }

    /// Evaluates the expression against the context. Rules referencing absent
    /// variables evaluate as if the variable were the empty string, except
    /// `ipmatch`, which is false for an absent/unparsable address.
    pub fn eval(&self, ctx: &Context) -> bool {
        eval_node(&self.root, ctx)
    }
}

fn parse_node(v: &serde_json::Value) -> Result<Node, String> {
    let arr = v.as_array().ok_or("each rule must be an array")?;
    if arr.is_empty() {
        return Err("empty rule".to_string());
    }

    // Nested logic: ["AND"|"OR", rule...]
    if let Some(first) = arr[0].as_str() {
        if first.eq_ignore_ascii_case("and") || first.eq_ignore_ascii_case("or") {
            let children = arr[1..]
                .iter()
                .map(parse_node)
                .collect::<Result<Vec<_>, _>>()?;
            return Ok(if first.eq_ignore_ascii_case("and") {
                Node::And(children)
            } else {
                Node::Or(children)
            });
        }
    }

    // [var, op, value] or [var, "!", op, value]
    let var = arr[0]
        .as_str()
        .ok_or("rule variable must be a string")?
        .to_string();
    let (negate, op_idx) = if arr.get(1).and_then(|v| v.as_str()) == Some("!") {
        (true, 2)
    } else {
        (false, 1)
    };
    let op_str = arr
        .get(op_idx)
        .and_then(|v| v.as_str())
        .ok_or_else(|| format!("rule for '{}' is missing an operator", var))?;
    let value = arr
        .get(op_idx + 1)
        .ok_or_else(|| format!("rule for '{}' is missing a value", var))?;

    let scalar = || -> Result<String, String> {
        match value {
            serde_json::Value::String(s) => Ok(s.clone()),
            serde_json::Value::Number(n) => Ok(n.to_string()),
            serde_json::Value::Bool(b) => Ok(b.to_string()),
            _ => Err(format!("rule for '{}' needs a scalar value", var)),
        }
    };
    let number = || -> Result<f64, String> {
        value
            .as_f64()
            .or_else(|| value.as_str().and_then(|s| s.parse().ok()))
            .ok_or_else(|| format!("rule for '{}' ({}) needs a numeric value", var, op_str))
    };
    let string_list = || -> Result<Vec<String>, String> {
        value
            .as_array()
            .ok_or_else(|| format!("rule for '{}' ({}) needs an array value", var, op_str))?
            .iter()
            .map(|item| {
                item.as_str()
                    .map(String::from)
                    .or_else(|| item.as_f64().map(|n| n.to_string()))
                    .ok_or_else(|| format!("rule for '{}': array items must be scalars", var))
            })
            .collect()
    };

    let op = match op_str {
        "==" => Op::Eq(scalar()?),
        "~=" | "!=" => Op::Ne(scalar()?),
        ">" => Op::Gt(number()?),
        ">=" => Op::Ge(number()?),
        "<" => Op::Lt(number()?),
        "<=" => Op::Le(number()?),
        "~~" => Op::Regex(
            Regex::new(&scalar()?).map_err(|e| format!("invalid regex for '{}': {}", var, e))?,
        ),
        "~*" => Op::Regex(
            Regex::new(&format!("(?i){}", scalar()?))
                .map_err(|e| format!("invalid regex for '{}': {}", var, e))?,
        ),
        "in" => Op::In(string_list()?),
        "has" => Op::Has(scalar()?),
        "ipmatch" => {
            let nets = string_list()?
                .iter()
                .map(|s| {
                    if let Ok(net) = s.parse::<IpNet>() {
                        Ok(net)
                    } else if let Ok(ip) = s.parse::<IpAddr>() {
                        Ok(IpNet::from(ip))
                    } else {
                        Err(format!("invalid IP/CIDR '{}' for '{}'", s, var))
                    }
                })
                .collect::<Result<Vec<_>, _>>()?;
            Op::IpMatch(nets)
        }
        other => {
            return Err(format!(
                "unknown operator '{}' — supported: ==, ~=, >, >=, <, <=, ~~, ~*, in, has, ipmatch",
                other
            ));
        }
    };

    Ok(Node::Rule { var, negate, op })
}

fn eval_node(node: &Node, ctx: &Context) -> bool {
    match node {
        Node::And(children) => children.iter().all(|c| eval_node(c, ctx)),
        Node::Or(children) => children.iter().any(|c| eval_node(c, ctx)),
        Node::Rule { var, negate, op } => {
            let value = resolve(ctx, var);
            let result = eval_op(op, value.as_deref());
            if *negate {
                !result
            } else {
                result
            }
        }
    }
}

fn eval_op(op: &Op, value: Option<&str>) -> bool {
    let v = value.unwrap_or("");
    match op {
        Op::Eq(expected) => v == expected,
        Op::Ne(expected) => v != expected,
        Op::Gt(n) => v.parse::<f64>().is_ok_and(|x| x > *n),
        Op::Ge(n) => v.parse::<f64>().is_ok_and(|x| x >= *n),
        Op::Lt(n) => v.parse::<f64>().is_ok_and(|x| x < *n),
        Op::Le(n) => v.parse::<f64>().is_ok_and(|x| x <= *n),
        Op::Regex(re) => re.is_match(v),
        Op::In(list) => list.iter().any(|item| item == v),
        Op::Has(needle) => v.split(',').map(str::trim).any(|part| part == needle),
        Op::IpMatch(nets) => value
            .and_then(|v| v.parse::<IpAddr>().ok())
            .is_some_and(|ip| nets.iter().any(|net| net.contains(&ip))),
    }
}

// ---- helpers ---------------------------------------------------------------

/// Splits `ip:port`, tolerating bare IPs and bracketed IPv6.
fn split_remote_addr(addr: &str) -> (&str, Option<&str>) {
    if let Some(stripped) = addr.strip_prefix('[') {
        // [v6]:port
        if let Some((ip, rest)) = stripped.split_once(']') {
            return (ip, rest.strip_prefix(':'));
        }
    }
    match addr.rsplit_once(':') {
        // An IPv6 without brackets contains multiple ':'; treat as bare IP.
        Some((ip, port)) if !ip.contains(':') => (ip, Some(port)),
        _ => (addr, None),
    }
}

fn query_string(ctx: &Context) -> String {
    let mut pairs: Vec<String> = Vec::new();
    for (k, values) in &ctx.request.query_params {
        for v in values {
            pairs.push(format!("{}={}", k, v));
        }
    }
    pairs.sort();
    pairs.join("&")
}

fn cookie_value(ctx: &Context, name: &str) -> Option<String> {
    let header = ctx.request.headers.get("cookie")?.first()?;
    for pair in header.split(';') {
        let (k, v) = pair.trim().split_once('=')?;
        if k == name {
            return Some(v.to_string());
        }
    }
    None
}

fn post_arg(ctx: &Context, field: &str) -> Option<String> {
    let content_type = ctx.request.headers.get("content-type")?.first()?;
    if !content_type.starts_with("application/x-www-form-urlencoded") {
        return None;
    }
    let body = std::str::from_utf8(&ctx.request.body).ok()?;
    for pair in body.split('&') {
        let (k, v) = pair.split_once('=')?;
        if k == field {
            return Some(urldecode(v));
        }
    }
    None
}

fn urldecode(s: &str) -> String {
    let mut out = Vec::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < bytes.len() => {
                if let (Some(h), Some(l)) = (hex_val(bytes[i + 1]), hex_val(bytes[i + 2])) {
                    out.push(h * 16 + l);
                    i += 3;
                    continue;
                }
                out.push(b'%');
            }
            b => out.push(b),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

fn message_str<'a>(ctx: &'a Context, key: &str) -> Option<Cow<'a, str>> {
    match ctx.message.get(key)? {
        serde_json::Value::String(s) => Some(Cow::Borrowed(s.as_str())),
        other => Some(Cow::Owned(other.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::{GatewayRequest, GatewayResponse, Protocol};
    use bytes::Bytes;
    use std::collections::HashMap;

    fn test_ctx() -> Context {
        let mut headers = HashMap::new();
        headers.insert("user-agent".to_string(), vec!["Mozilla/5.0".to_string()]);
        headers.insert(
            "cookie".to_string(),
            vec!["session=abc123; theme=dark".to_string()],
        );
        headers.insert("x-tags".to_string(), vec!["beta, internal".to_string()]);
        let mut query = HashMap::new();
        query.insert("name".to_string(), vec!["jack".to_string()]);
        query.insert("age".to_string(), vec!["30".to_string()]);
        let mut message = HashMap::new();
        message.insert("consumer.name".to_string(), serde_json::json!("alice"));

        Context {
            request: GatewayRequest {
                method: "GET".to_string(),
                path: "/api/users".to_string(),
                host: "example.com".to_string(),
                scheme: "http".to_string(),
                headers,
                query_params: query,
                body: Bytes::new(),
                remote_addr: "10.1.2.3:44321".to_string(),
                protocol: Protocol::Http1,
            },
            response: GatewayResponse {
                status_code: 502,
                headers: HashMap::new(),
                body: Bytes::from_static(b"bad gateway"),
            },
            message,
            errors: Vec::new(),
        }
    }

    #[test]
    fn test_resolve_basic_vars() {
        let ctx = test_ctx();
        let cases = [
            ("uri", Some("/api/users")),
            ("method", Some("GET")),
            ("host", Some("example.com")),
            ("scheme", Some("http")),
            ("remote_addr", Some("10.1.2.3")),
            ("remote_port", Some("44321")),
            ("status", Some("502")),
            ("resp_body", Some("bad gateway")),
            ("arg_name", Some("jack")),
            ("arg_missing", None),
            ("http_user_agent", Some("Mozilla/5.0")),
            ("http_missing", None),
            ("cookie_session", Some("abc123")),
            ("cookie_theme", Some("dark")),
            ("cookie_missing", None),
            ("consumer_name", Some("alice")),
            ("consumer_group_id", None),
            ("unknown_var", None),
        ];
        for (name, expected) in cases {
            assert_eq!(resolve(&ctx, name).as_deref(), expected, "var {name}");
        }
    }

    #[test]
    fn test_resolve_post_arg() {
        let mut ctx = test_ctx();
        ctx.request.headers.insert(
            "content-type".to_string(),
            vec!["application/x-www-form-urlencoded".to_string()],
        );
        ctx.request.body = Bytes::from_static(b"user=bob&note=hello%20world&plus=a+b");
        assert_eq!(resolve(&ctx, "post_arg_user").as_deref(), Some("bob"));
        assert_eq!(
            resolve(&ctx, "post_arg_note").as_deref(),
            Some("hello world")
        );
        assert_eq!(resolve(&ctx, "post_arg_plus").as_deref(), Some("a b"));
        assert_eq!(resolve(&ctx, "post_arg_missing"), None);

        // wrong content type -> no post args
        ctx.request.headers.insert(
            "content-type".to_string(),
            vec!["application/json".to_string()],
        );
        assert_eq!(resolve(&ctx, "post_arg_user"), None);
    }

    #[test]
    fn test_interpolate() {
        let ctx = test_ctx();
        assert_eq!(
            interpolate(&ctx, "$remote_addr -> $uri"),
            "10.1.2.3 -> /api/users"
        );
        assert_eq!(
            interpolate(&ctx, "${scheme}://${host}${uri}"),
            "http://example.com/api/users"
        );
        assert_eq!(interpolate(&ctx, "user=$arg_name!"), "user=jack!");
        assert_eq!(interpolate(&ctx, "missing=[$arg_nope]"), "missing=[]");
        assert_eq!(interpolate(&ctx, "cost: 5$"), "cost: 5$");
        assert_eq!(interpolate(&ctx, "no vars here"), "no vars here");
    }

    fn expr(json: serde_json::Value) -> Expr {
        Expr::parse(&json).expect("expression should parse")
    }

    #[test]
    fn test_expr_operators() {
        let ctx = test_ctx();
        let truthy = [
            serde_json::json!([["arg_name", "==", "jack"]]),
            serde_json::json!([["arg_name", "~=", "jill"]]),
            serde_json::json!([["arg_age", ">", 18]]),
            serde_json::json!([["arg_age", "<=", "30"]]),
            serde_json::json!([["http_user_agent", "~~", "Mozilla.+"]]),
            serde_json::json!([["http_user_agent", "~*", "mozilla.+"]]),
            serde_json::json!([["arg_name", "in", ["jack", "jill"]]]),
            serde_json::json!([["http_x_tags", "has", "beta"]]),
            serde_json::json!([["remote_addr", "ipmatch", ["10.0.0.0/8"]]]),
            serde_json::json!([["remote_addr", "ipmatch", ["10.1.2.3"]]]),
            serde_json::json!([["arg_name", "!", "==", "jill"]]),
            serde_json::json!([["arg_name", "==", "jack"], ["arg_age", ">=", 30]]),
            serde_json::json!([["OR", ["arg_name", "==", "nope"], ["arg_age", "==", "30"]]]),
        ];
        for case in &truthy {
            assert!(expr(case.clone()).eval(&ctx), "should be true: {case}");
        }

        let falsy = [
            serde_json::json!([["arg_name", "==", "jill"]]),
            serde_json::json!([["arg_age", ">", 30]]),
            serde_json::json!([["remote_addr", "ipmatch", ["192.168.0.0/16"]]]),
            serde_json::json!([["arg_name", "==", "jack"], ["arg_age", ">", 99]]),
            serde_json::json!([["AND", ["arg_name", "==", "jack"], ["arg_age", ">", 99]]]),
            serde_json::json!([["arg_missing", "==", "x"]]),
        ];
        for case in &falsy {
            assert!(!expr(case.clone()).eval(&ctx), "should be false: {case}");
        }
    }

    #[test]
    fn test_expr_parse_errors() {
        let bad = [
            serde_json::json!("not an array"),
            serde_json::json!([["arg_x", "unknown_op", "v"]]),
            serde_json::json!([["arg_x", "~~", "("]]),
            serde_json::json!([["remote_addr", "ipmatch", ["not-an-ip"]]]),
            serde_json::json!([["arg_x", "=="]]),
            serde_json::json!([[]]),
        ];
        for case in &bad {
            assert!(Expr::parse(case).is_err(), "should fail to parse: {case}");
        }
    }

    #[test]
    fn test_split_remote_addr_forms() {
        assert_eq!(split_remote_addr("1.2.3.4:80"), ("1.2.3.4", Some("80")));
        assert_eq!(split_remote_addr("1.2.3.4"), ("1.2.3.4", None));
        assert_eq!(split_remote_addr("[::1]:8080"), ("::1", Some("8080")));
        assert_eq!(split_remote_addr("::1"), ("::1", None));
    }
}
