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
//! Rules in a top-level list are ANDed. A rule is `[subject, op, value]`,
//! the negated `[subject, "!", op, value]`, or unary `[subject, "present"|"absent"|"is_null"]`.
//! Nested logic uses `["AND", rule...]`, `["OR", rule...]`, and `["NOT", rule-or-group]`
//! (NOT is a featherbit extension over APISIX's dialect). Subjects are var
//! names or JSONPath queries over JSON bodies: `$.user.name` (request body),
//! `request_body:$...`, `response_body:$...` — multi-node matches use
//! ANY-semantics. Operators: `==`, `~=`, `>`, `>=`, `<`, `<=`, `~~` (regex),
//! `~*` (case-insensitive regex), `in`, `has`, `ipmatch`, `present`,
//! `absent`, `is_null` (JSONPath only), `contains`.

use std::borrow::Cow;
use std::cell::OnceCell;
use std::net::IpAddr;

use ipnet::IpNet;
use regex::Regex;

use crate::context::Context;
use crate::vars::jsonpath::{BodyTarget, JsonSubject};

pub mod catalog;
pub mod jsonpath;
pub mod template;

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
/// - `request_body` — request body (lossy UTF-8)
/// - `arg_<name>` — first query parameter value
/// - `http_<name>` — first request header value (underscores map to dashes)
/// - `sent_http_<name>` — first response header value (underscores map to dashes)
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
        "request_body" => Some(Cow::Owned(
            String::from_utf8_lossy(&ctx.request.body).into_owned(),
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
            } else if let Some(header) = name.strip_prefix("sent_http_") {
                let header = header.replace('_', "-").to_lowercase();
                ctx.response
                    .headers
                    .get(&header)
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
#[derive(Debug)]
pub struct Expr {
    root: Node,
}

#[derive(Debug)]
enum Node {
    And(Vec<Node>),
    Or(Vec<Node>),
    Not(Box<Node>),
    Rule {
        subject: Subject,
        negate: bool,
        op: Op,
    },
}

/// What a rule's condition is evaluated against: a flat named var, or a
/// JSONPath query over a request/response body.
#[derive(Debug)]
enum Subject {
    Var(String),
    Json(JsonSubject),
}

#[derive(Debug)]
enum Op {
    Eq(serde_json::Value),
    Ne(serde_json::Value),
    Gt(f64),
    Ge(f64),
    Lt(f64),
    Le(f64),
    Regex(Regex),
    In(Vec<serde_json::Value>),
    Has(serde_json::Value),
    IpMatch(Vec<IpNet>),
    Present,
    Absent,
    IsNull,
    Contains(serde_json::Value),
}

/// Stringifies a scalar config value the way the legacy parser did
/// (String as-is, Number/Bool via to_string). None for null/array/object.
fn scalar_str(v: &serde_json::Value) -> Option<String> {
    match v {
        serde_json::Value::String(s) => Some(s.clone()),
        serde_json::Value::Number(n) => Some(n.to_string()),
        serde_json::Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
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
    /// `ipmatch`, which is false for an absent/unparsable address. JSONPath
    /// subjects match zero nodes (rule is false) when the body is empty or
    /// not valid JSON.
    pub fn eval(&self, ctx: &Context) -> bool {
        let state = EvalState::new(ctx);
        eval_node(&self.root, &state, false).expect("lenient eval is infallible")
    }

    /// Strict evaluation: errors instead of guessing when a condition cannot
    /// actually be checked. A rule is uncheckable when its variable subject
    /// is absent (except under the existence tests `present`/`absent`, which
    /// legitimately ask about absence), or when a JSONPath subject targets a
    /// body that is empty or not valid JSON (existence tests included — there
    /// is no document to ask about).
    ///
    /// Evaluation is left-to-right with short-circuiting, so an uncheckable
    /// rule only errors when it is reached before the group's outcome is
    /// decided.
    pub fn try_eval(&self, ctx: &Context) -> Result<bool, String> {
        let state = EvalState::new(ctx);
        eval_node(&self.root, &state, true)
    }
}

/// Per-evaluation state: the context plus lazily-parsed JSON bodies, so a
/// multi-rule expression parses each body at most once per eval.
struct EvalState<'a> {
    ctx: &'a Context,
    request_json: OnceCell<Option<serde_json::Value>>,
    response_json: OnceCell<Option<serde_json::Value>>,
}

impl<'a> EvalState<'a> {
    fn new(ctx: &'a Context) -> Self {
        Self {
            ctx,
            request_json: OnceCell::new(),
            response_json: OnceCell::new(),
        }
    }

    /// The parsed JSON body, or None when empty/not valid JSON.
    fn body_json(&self, target: &BodyTarget) -> Option<&serde_json::Value> {
        let (cell, bytes) = match target {
            BodyTarget::Request => (&self.request_json, &self.ctx.request.body),
            BodyTarget::Response => (&self.response_json, &self.ctx.response.body),
        };
        cell.get_or_init(|| serde_json::from_slice(bytes).ok())
            .as_ref()
    }
}

fn parse_node(v: &serde_json::Value) -> Result<Node, String> {
    let arr = v.as_array().ok_or("each rule must be an array")?;
    if arr.is_empty() {
        return Err("empty rule".to_string());
    }

    // Nested logic: ["AND"|"OR", rule...] or ["NOT", rule-or-group]
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
        if first.eq_ignore_ascii_case("not") {
            if arr.len() != 2 {
                return Err("NOT takes exactly one rule or group".to_string());
            }
            return Ok(Node::Not(Box::new(parse_node(&arr[1])?)));
        }
    }

    // [var, op, value] or [var, "!", op, value]
    let var = arr[0]
        .as_str()
        .ok_or("rule variable must be a string")?
        .to_string();
    let subject = match jsonpath::parse_json_subject(&var) {
        None => Subject::Var(var.clone()),
        Some(compiled) => Subject::Json(compiled?),
    };
    let (negate, op_idx) = if arr.get(1).and_then(|v| v.as_str()) == Some("!") {
        (true, 2)
    } else {
        (false, 1)
    };
    let op_str = arr
        .get(op_idx)
        .and_then(|v| v.as_str())
        .ok_or_else(|| format!("rule for '{}' is missing an operator", var))?;

    let is_unary = matches!(op_str, "present" | "absent" | "is_null");
    if is_unary {
        if arr.len() > op_idx + 1 {
            return Err(format!("rule for '{}': '{}' takes no value", var, op_str));
        }
        let op = match op_str {
            "present" => Op::Present,
            "absent" => Op::Absent,
            "is_null" => {
                if matches!(subject, Subject::Var(_)) {
                    return Err(format!(
                        "rule for '{}': 'is_null' requires a JSONPath subject — headers and vars cannot be null (use 'absent')",
                        var
                    ));
                }
                Op::IsNull
            }
            _ => unreachable!(),
        };
        return Ok(Node::Rule {
            subject,
            negate,
            op,
        });
    }

    let value = arr
        .get(op_idx + 1)
        .ok_or_else(|| format!("rule for '{}' is missing a value", var))?;

    let scalar = || -> Result<serde_json::Value, String> {
        scalar_str(value)
            .map(|_| value.clone())
            .ok_or_else(|| format!("rule for '{}' needs a scalar value", var))
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
    let pattern = || -> Result<String, String> {
        scalar_str(value).ok_or_else(|| format!("rule for '{}' needs a scalar value", var))
    };

    let op = match op_str {
        "==" => Op::Eq(scalar()?),
        "~=" | "!=" => Op::Ne(scalar()?),
        ">" => Op::Gt(number()?),
        ">=" => Op::Ge(number()?),
        "<" => Op::Lt(number()?),
        "<=" => Op::Le(number()?),
        "~~" => Op::Regex(
            Regex::new(&pattern()?).map_err(|e| format!("invalid regex for '{}': {}", var, e))?,
        ),
        "~*" => Op::Regex(
            Regex::new(&format!("(?i){}", pattern()?))
                .map_err(|e| format!("invalid regex for '{}': {}", var, e))?,
        ),
        "in" => {
            let items = value
                .as_array()
                .ok_or_else(|| format!("rule for '{}' (in) needs an array value", var))?;
            for item in items {
                if scalar_str(item).is_none() {
                    return Err(format!("rule for '{}': array items must be scalars", var));
                }
            }
            Op::In(items.clone())
        }
        "has" => Op::Has(scalar()?),
        "contains" => Op::Contains(scalar()?),
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
                "unknown operator '{}' — supported: ==, ~=, >, >=, <, <=, ~~, ~*, in, has, ipmatch, present, absent, is_null, contains",
                other
            ));
        }
    };

    Ok(Node::Rule {
        subject,
        negate,
        op,
    })
}

/// Evaluates a node left-to-right with short-circuiting. `strict: false`
/// never returns `Err` (absent vars evaluate as empty string / absent
/// address, an unparsable body matches zero nodes); `strict: true` turns
/// those uncheckable rules into errors instead.
fn eval_node(node: &Node, state: &EvalState, strict: bool) -> Result<bool, String> {
    match node {
        Node::And(children) => {
            for c in children {
                if !eval_node(c, state, strict)? {
                    return Ok(false);
                }
            }
            Ok(true)
        }
        Node::Or(children) => {
            for c in children {
                if eval_node(c, state, strict)? {
                    return Ok(true);
                }
            }
            Ok(false)
        }
        Node::Not(child) => Ok(!eval_node(child, state, strict)?),
        Node::Rule {
            subject,
            negate,
            op,
        } => {
            let result = match subject {
                Subject::Var(name) => {
                    let value = resolve(state.ctx, name);
                    if strict && value.is_none() && !matches!(op, Op::Present | Op::Absent) {
                        return Err(format!("variable '{}' is not resolvable", name));
                    }
                    eval_op(op, value.as_deref())
                }
                Subject::Json(js) => {
                    let nodes: Vec<&serde_json::Value> = match state.body_json(&js.target) {
                        Some(doc) => js.path.query(doc).all(),
                        None => {
                            if strict {
                                return Err(format!(
                                    "{} body is empty or not valid JSON",
                                    match js.target {
                                        BodyTarget::Request => "request",
                                        BodyTarget::Response => "response",
                                    }
                                ));
                            }
                            Vec::new()
                        }
                    };
                    eval_op_json(op, &nodes)
                }
            };
            Ok(if *negate { !result } else { result })
        }
    }
}

fn eval_op(op: &Op, value: Option<&str>) -> bool {
    let v = value.unwrap_or("");
    match op {
        Op::Eq(expected) => scalar_str(expected).as_deref() == Some(v),
        Op::Ne(expected) => scalar_str(expected).as_deref() != Some(v),
        Op::Gt(n) => v.parse::<f64>().is_ok_and(|x| x > *n),
        Op::Ge(n) => v.parse::<f64>().is_ok_and(|x| x >= *n),
        Op::Lt(n) => v.parse::<f64>().is_ok_and(|x| x < *n),
        Op::Le(n) => v.parse::<f64>().is_ok_and(|x| x <= *n),
        Op::Regex(re) => re.is_match(v),
        Op::In(list) => list
            .iter()
            .any(|item| scalar_str(item).as_deref() == Some(v)),
        Op::Has(needle) => {
            let needle = scalar_str(needle).unwrap_or_default();
            v.split(',').map(str::trim).any(|part| part == needle)
        }
        Op::IpMatch(nets) => value
            .and_then(|v| v.parse::<IpAddr>().ok())
            .is_some_and(|ip| nets.iter().any(|net| net.contains(&ip))),
        Op::Present => value.is_some(),
        Op::Absent => value.is_none(),
        Op::IsNull => false, // parse-time guarded; a flat var is never null
        Op::Contains(needle) => {
            value.is_some_and(|v| scalar_str(needle).is_some_and(|n| v.contains(&n)))
        }
    }
}

/// ANY-match: the rule holds if at least one matched node passes.
/// `Present`/`Absent`/`IsNull` are match-set operators, evaluated over the
/// whole node set rather than per-node.
fn eval_op_json(op: &Op, nodes: &[&serde_json::Value]) -> bool {
    match op {
        Op::Present => !nodes.is_empty(),
        Op::Absent => nodes.is_empty(),
        Op::IsNull => nodes.iter().any(|n| n.is_null()),
        _ => nodes.iter().any(|n| eval_op_json_node(op, n)),
    }
}

fn eval_op_json_node(op: &Op, node: &serde_json::Value) -> bool {
    match op {
        Op::Eq(expected) => json_scalar_eq(node, expected),
        Op::Ne(expected) => !json_scalar_eq(node, expected),
        Op::Gt(n) => node.as_f64().is_some_and(|x| x > *n),
        Op::Ge(n) => node.as_f64().is_some_and(|x| x >= *n),
        Op::Lt(n) => node.as_f64().is_some_and(|x| x < *n),
        Op::Le(n) => node.as_f64().is_some_and(|x| x <= *n),
        Op::Regex(re) => node.as_str().is_some_and(|s| re.is_match(s)),
        Op::In(list) => list.iter().any(|e| json_scalar_eq(node, e)),
        Op::Has(v) => node
            .as_array()
            .is_some_and(|arr| arr.iter().any(|e| json_scalar_eq(e, v))),
        Op::IpMatch(nets) => node
            .as_str()
            .and_then(|s| s.parse::<IpAddr>().ok())
            .is_some_and(|ip| nets.iter().any(|net| net.contains(&ip))),
        Op::Contains(v) => match node {
            serde_json::Value::String(s) => scalar_str(v).is_some_and(|needle| s.contains(&needle)),
            serde_json::Value::Array(arr) => arr.iter().any(|e| json_scalar_eq(e, v)),
            _ => false,
        },
        Op::Present | Op::Absent | Op::IsNull => unreachable!("handled in eval_op_json"),
    }
}

/// Native scalar equality: string↔string, number↔number, bool↔bool.
/// null, arrays, and objects never equal a scalar config value.
fn json_scalar_eq(node: &serde_json::Value, expected: &serde_json::Value) -> bool {
    use serde_json::Value as V;
    match (node, expected) {
        (V::String(a), V::String(b)) => a == b,
        (V::Number(a), V::Number(b)) => a.as_f64() == b.as_f64(),
        (V::Bool(a), V::Bool(b)) => a == b,
        _ => false,
    }
}

// ---- helpers ---------------------------------------------------------------

/// Splits `ip:port`, tolerating bare IPs and bracketed IPv6.
pub(crate) fn split_remote_addr(addr: &str) -> (&str, Option<&str>) {
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

pub(crate) fn query_string(ctx: &Context) -> String {
    let mut pairs: Vec<String> = Vec::new();
    for (k, values) in &ctx.request.query_params {
        for v in values {
            pairs.push(format!("{}={}", k, v));
        }
    }
    pairs.sort();
    pairs.join("&")
}

pub(crate) fn cookie_value(ctx: &Context, name: &str) -> Option<String> {
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

pub(crate) fn message_str<'a>(ctx: &'a Context, key: &str) -> Option<Cow<'a, str>> {
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
    fn test_expr_in_accepts_bool_items() {
        // widened on purpose: `in` items may be any scalar, including bools;
        // flat vars compare against their stringified form.
        let e = Expr::parse(&serde_json::json!([["arg_flag", "in", [true]]]))
            .expect("should parse bool in list");
        let mut ctx = test_ctx();
        ctx.request
            .query_params
            .insert("flag".to_string(), vec!["true".to_string()]);
        assert!(e.eval(&ctx), "flag=true should match in [true]");

        ctx.request
            .query_params
            .insert("flag".to_string(), vec!["false".to_string()]);
        assert!(!e.eval(&ctx), "flag=false should not match in [true]");

        // Also verify non-scalar items still fail to parse
        assert!(
            Expr::parse(&serde_json::json!([["arg_x", "in", [[1]]]])).is_err(),
            "array items in in-list should fail"
        );
    }

    #[test]
    fn test_split_remote_addr_forms() {
        assert_eq!(split_remote_addr("1.2.3.4:80"), ("1.2.3.4", Some("80")));
        assert_eq!(split_remote_addr("1.2.3.4"), ("1.2.3.4", None));
        assert_eq!(split_remote_addr("[::1]:8080"), ("::1", Some("8080")));
        assert_eq!(split_remote_addr("::1"), ("::1", None));
    }

    #[test]
    fn test_sent_http_resolves_response_header() {
        let mut ctx = test_ctx();
        ctx.response.headers.insert(
            "x-cache-status".to_string(),
            vec!["HIT".to_string(), "second".to_string()],
        );
        assert_eq!(
            resolve(&ctx, "sent_http_x_cache_status").as_deref(),
            Some("HIT"),
            "underscore->dash mapping and first-value pick must mirror http_*"
        );
        assert!(resolve(&ctx, "sent_http_missing").is_none());
    }

    #[test]
    fn test_request_body_lossy_utf8() {
        let mut ctx = test_ctx();
        ctx.request.body = bytes::Bytes::from_static(b"hello=world");
        assert_eq!(
            resolve(&ctx, "request_body").as_deref(),
            Some("hello=world")
        );

        ctx.request.body = bytes::Bytes::from_static(&[0xff, 0x61]);
        assert_eq!(resolve(&ctx, "request_body").as_deref(), Some("\u{fffd}a"));
    }

    fn json_body_ctx(body: &str) -> Context {
        let mut ctx = test_ctx();
        ctx.request.body = Bytes::from(body.to_string());
        ctx
    }

    #[test]
    fn test_expr_jsonpath_subjects() {
        let ctx = json_body_ctx(
            r#"{"user":{"name":"jack","age":30,"tags":["a","b"],"admin":true},"items":[{"price":5},{"price":0}]}"#,
        );

        let truthy = [
            serde_json::json!([["$.user.name", "==", "jack"]]),
            serde_json::json!([["request_body:$.user.name", "==", "jack"]]),
            serde_json::json!([["$.user.age", ">", 18]]),
            serde_json::json!([["$.user.admin", "==", true]]),
            serde_json::json!([["$.user.name", "~~", "^ja"]]),
            serde_json::json!([["$.user.age", "in", [30, 40]]]),
            serde_json::json!([["$.user.tags", "has", "a"]]),
            // ANY-match: one item has price > 1
            serde_json::json!([["$.items[*].price", ">", 1]]),
            // ALL via negation: NOT(any price < 0)
            serde_json::json!([["$.items[*].price", "!", "<", 0]]),
        ];
        for case in &truthy {
            assert!(
                Expr::parse(case).unwrap().eval(&ctx),
                "should be true: {case}"
            );
        }

        let falsy = [
            // number node never equals a string scalar
            serde_json::json!([["$.user.age", "==", "30"]]),
            serde_json::json!([["$.user.name", "==", "jill"]]),
            // absent path matches nothing -> comparison false
            serde_json::json!([["$.missing", "==", "x"]]),
            // object node never equals a scalar
            serde_json::json!([["$.user", "==", "jack"]]),
        ];
        for case in &falsy {
            assert!(
                !Expr::parse(case).unwrap().eval(&ctx),
                "should be false: {case}"
            );
        }
    }

    #[test]
    fn test_expr_jsonpath_response_body_and_non_json() {
        let mut ctx = test_ctx();
        ctx.response.body = Bytes::from(r#"{"ok":true}"#.to_string());
        assert!(
            Expr::parse(&serde_json::json!([["response_body:$.ok", "==", true]]))
                .unwrap()
                .eval(&ctx)
        );

        // non-JSON request body: every request-body path matches zero nodes
        let ctx = json_body_ctx("plain text");
        assert!(!Expr::parse(&serde_json::json!([["$.a", "==", "x"]]))
            .unwrap()
            .eval(&ctx));
    }

    #[test]
    fn test_expr_jsonpath_parse_errors() {
        assert!(Expr::parse(&serde_json::json!([["$.[", "==", "x"]])).is_err());
    }

    #[test]
    fn test_interpolate_sent_http_and_request_body() {
        let mut ctx = test_ctx();
        ctx.response
            .headers
            .insert("x-id".to_string(), vec!["42".to_string()]);
        ctx.request.body = bytes::Bytes::from_static(b"B");
        assert_eq!(
            interpolate(&ctx, "h=$sent_http_x_id b=$request_body"),
            "h=42 b=B"
        );
    }

    #[test]
    fn test_expr_present_absent() {
        // ctx() has header x-api-version (adapt to the module's factory);
        // build one with a known header + JSON body:
        let mut ctx = json_body_ctx(r#"{"a": null, "b": 1}"#);
        ctx.request
            .headers
            .insert("x-empty".to_string(), vec!["".to_string()]);

        let truthy = [
            serde_json::json!([["http_x_empty", "present"]]), // empty value still present
            serde_json::json!([["http_x_missing", "absent"]]),
            serde_json::json!([["http_x_missing", "!", "present"]]),
            serde_json::json!([["$.a", "present"]]), // null node counts as present
            serde_json::json!([["$.missing", "absent"]]),
            serde_json::json!([["$.a", "is_null"]]),
            serde_json::json!([["$.b", "!", "is_null"]]),
        ];
        for case in &truthy {
            assert!(
                Expr::parse(case).unwrap().eval(&ctx),
                "should be true: {case}"
            );
        }

        let falsy = [
            serde_json::json!([["http_x_empty", "absent"]]),
            serde_json::json!([["$.missing", "present"]]),
            serde_json::json!([["$.missing", "is_null"]]), // absent is NOT null
            serde_json::json!([["$.b", "is_null"]]),
        ];
        for case in &falsy {
            assert!(
                !Expr::parse(case).unwrap().eval(&ctx),
                "should be false: {case}"
            );
        }
    }

    #[test]
    fn test_expr_contains() {
        let mut ctx = json_body_ctx(r#"{"tags":["a","b"],"nums":[1,2],"name":"hello world"}"#);
        ctx.request.headers.insert(
            "authorization".to_string(),
            vec!["Bearer abc123".to_string()],
        );

        let truthy = [
            serde_json::json!([["http_authorization", "contains", "Bearer"]]),
            serde_json::json!([["$.name", "contains", "lo wo"]]),
            serde_json::json!([["$.tags", "contains", "a"]]), // array element equality
            serde_json::json!([["$.nums", "contains", 2]]),
        ];
        for case in &truthy {
            assert!(
                Expr::parse(case).unwrap().eval(&ctx),
                "should be true: {case}"
            );
        }
        let falsy = [
            serde_json::json!([["http_authorization", "contains", "Basic"]]),
            serde_json::json!([["http_x_missing", "contains", "x"]]), // absent -> false
            serde_json::json!([["$.nums", "contains", "2"]]),         // "2" != 2 in arrays
            serde_json::json!([["$.nums", "contains", 3]]),
        ];
        for case in &falsy {
            assert!(
                !Expr::parse(case).unwrap().eval(&ctx),
                "should be false: {case}"
            );
        }
    }

    #[test]
    fn test_expr_new_operator_parse_errors() {
        let cases = [
            // is_null on a flat var
            serde_json::json!([["http_x", "is_null"]]),
            // unary op given a value
            serde_json::json!([["http_x", "present", "y"]]),
            // binary op missing a value (already an error; pin it stays one)
            serde_json::json!([["http_x", "contains"]]),
        ];
        for case in &cases {
            assert!(Expr::parse(case).is_err(), "should fail to parse: {case}");
        }
    }

    #[test]
    fn test_expr_not_group() {
        let mut ctx = json_body_ctx(r#"{"user":{"id":null}}"#);
        ctx.request
            .headers
            .insert("authorization".to_string(), vec!["Bearer tok".to_string()]);

        // NOT over a rule
        assert!(!Expr::parse(&serde_json::json!([[
            "NOT",
            ["http_authorization", "present"]
        ]]))
        .unwrap()
        .eval(&ctx));

        // NOT over a group, nested logic (spec example shape)
        let e = Expr::parse(&serde_json::json!([
            ["http_authorization", "present"],
            ["http_authorization", "contains", "Bearer"],
            [
                "OR",
                ["$.user.email", "present"],
                ["NOT", ["$.user.id", "is_null"]]
            ]
        ]))
        .unwrap();
        // email absent AND id is null -> OR arm: (false OR NOT(true)) = false
        assert!(!e.eval(&ctx));

        // nested NOT(NOT(x)) == x
        assert!(Expr::parse(&serde_json::json!([[
            "NOT",
            ["NOT", ["http_authorization", "present"]]
        ]]))
        .unwrap()
        .eval(&ctx));
    }

    #[test]
    fn test_expr_try_eval_resolvable_rules() {
        let ctx = test_ctx();
        assert_eq!(
            expr(serde_json::json!([["arg_name", "==", "jack"]])).try_eval(&ctx),
            Ok(true)
        );
        assert_eq!(
            expr(serde_json::json!([["arg_name", "==", "jill"]])).try_eval(&ctx),
            Ok(false)
        );
    }

    #[test]
    fn test_expr_try_eval_absent_var_errors() {
        let ctx = test_ctx();
        let err = expr(serde_json::json!([["arg_missing", "==", "x"]]))
            .try_eval(&ctx)
            .unwrap_err();
        assert!(err.contains("arg_missing"), "{err}");

        // negation doesn't rescue an unresolvable subject
        assert!(expr(serde_json::json!([["arg_missing", "!", "==", "x"]]))
            .try_eval(&ctx)
            .is_err());
    }

    #[test]
    fn test_expr_try_eval_existence_ops_never_error() {
        let ctx = test_ctx();
        assert_eq!(
            expr(serde_json::json!([["arg_missing", "absent"]])).try_eval(&ctx),
            Ok(true)
        );
        assert_eq!(
            expr(serde_json::json!([["arg_missing", "present"]])).try_eval(&ctx),
            Ok(false)
        );
        assert_eq!(
            expr(serde_json::json!([["arg_missing", "!", "present"]])).try_eval(&ctx),
            Ok(true)
        );
    }

    #[test]
    fn test_expr_try_eval_jsonpath_body() {
        // valid JSON: absent paths are a checked false (ANY-match over zero
        // nodes), same as lenient eval
        let ctx = json_body_ctx(r#"{"user":{"name":"jack"}}"#);
        assert_eq!(
            expr(serde_json::json!([["$.user.name", "==", "jack"]])).try_eval(&ctx),
            Ok(true)
        );
        assert_eq!(
            expr(serde_json::json!([["$.missing", "==", "x"]])).try_eval(&ctx),
            Ok(false)
        );
        assert_eq!(
            expr(serde_json::json!([["$.missing", "absent"]])).try_eval(&ctx),
            Ok(true)
        );

        // empty or non-JSON body: every JSONPath rule (existence ops included)
        // is uncheckable
        let plain = json_body_ctx("plain text");
        let err = expr(serde_json::json!([["$.a", "==", "x"]]))
            .try_eval(&plain)
            .unwrap_err();
        assert!(err.contains("request body"), "{err}");
        assert!(expr(serde_json::json!([["$.a", "absent"]]))
            .try_eval(&plain)
            .is_err());
        let empty = json_body_ctx("");
        assert!(expr(serde_json::json!([["$.a", "present"]]))
            .try_eval(&empty)
            .is_err());
    }

    #[test]
    fn test_expr_try_eval_short_circuit_and_propagation() {
        let ctx = test_ctx();
        // left-to-right: a decisive false short-circuits before the
        // uncheckable rule is reached
        assert_eq!(
            expr(serde_json::json!([
                ["arg_name", "==", "jill"],
                ["arg_missing", "==", "x"]
            ]))
            .try_eval(&ctx),
            Ok(false)
        );
        // ...but an uncheckable rule reached first propagates as an error
        assert!(expr(serde_json::json!([
            ["arg_missing", "==", "x"],
            ["arg_name", "==", "jill"]
        ]))
        .try_eval(&ctx)
        .is_err());

        // OR short-circuits on true
        assert_eq!(
            expr(serde_json::json!([[
                "OR",
                ["arg_name", "==", "jack"],
                ["arg_missing", "==", "x"]
            ]]))
            .try_eval(&ctx),
            Ok(true)
        );
        // NOT propagates the error
        assert!(
            expr(serde_json::json!([["NOT", ["arg_missing", "==", "x"]]]))
                .try_eval(&ctx)
                .is_err()
        );
    }

    #[test]
    fn test_expr_lenient_eval_unchanged_by_try_eval() {
        // the exact cases that error strictly still evaluate leniently
        let ctx = test_ctx();
        assert!(!expr(serde_json::json!([["arg_missing", "==", "x"]])).eval(&ctx));
        let plain = json_body_ctx("plain text");
        assert!(!expr(serde_json::json!([["$.a", "==", "x"]])).eval(&plain));
        assert!(expr(serde_json::json!([["$.a", "absent"]])).eval(&plain));
    }

    #[test]
    fn test_expr_not_parse_errors() {
        // zero children
        assert!(Expr::parse(&serde_json::json!([["NOT"]])).is_err());
        // two children
        assert!(Expr::parse(&serde_json::json!([[
            "NOT",
            ["http_a", "present"],
            ["http_b", "present"]
        ]]))
        .is_err());
    }
}
