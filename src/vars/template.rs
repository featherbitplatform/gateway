//! Universal `{{namespace.path}}` template engine.
//!
//! Complements `$var`/`${var}` interpolation ([`super::interpolate`]) with a
//! syntax that is unambiguous by construction: any `{{...}}` that doesn't
//! match a known namespace path passes through as a literal (no silent-empty,
//! no escape syntax needed). This lets the engine be applied universally —
//! including to fields holding secrets, regexes, or `$1`/`$ref` values that
//! the legacy `$` syntax would corrupt.
//!
//! Grammar: `{{` `\s*` `<namespace path>` `\s*` `}}`, no nesting. Namespaces:
//!
//! - `request.method|path|host|scheme|body`
//! - `request.headers.<name>` / `request.query.<name>` / `request.cookies.<name>`
//! - `response.status|body`, `response.headers.<name>`
//! - `message.<key>` (key may itself contain dots)
//! - `client.ip|port`
//! - `env.<NAME>` — resolved at *parse* time from the process environment
//!   (or an injected lookup fn), never at render time.
//!
//! Anything else — an unrecognized first segment, a malformed remainder
//! within a known namespace, or an unclosed `{{` — passes through literally.
//! A malformed remainder within a *known* namespace (and an unset `env.*`)
//! additionally produces a warning string from [`Template::parse`].
//!
//! [`Template::render_with_legacy`] applies the legacy `interpolate` pass to
//! the template's literal segments only; rendered `{{...}}` output is never
//! `$`-processed, so runtime data can never become a `$var` read primitive.

use std::borrow::Cow;

use crate::context::Context;
use crate::vars::{cookie_value, message_str, split_remote_addr};

/// A single resolvable reference inside a template.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TemplateRef {
    RequestMethod,
    RequestPath,
    RequestHost,
    RequestScheme,
    RequestBody,
    RequestHeader(String),
    RequestQuery(String),
    RequestCookie(String),
    ResponseStatus,
    ResponseBody,
    ResponseHeader(String),
    Message(String),
    ClientIp,
    ClientPort,
}

/// One chunk of a parsed template: either raw text or a resolvable reference.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Segment {
    Literal(String),
    Ref(TemplateRef),
}

/// A parsed template, ready to render repeatedly against different contexts.
///
/// Adjacent literal segments are merged at parse time, so a template with no
/// references at all collapses to zero-or-one `Literal` segments, enabling
/// the [`Template::render`] fast path to borrow directly from it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Template {
    segments: Vec<Segment>,
    has_refs: bool,
}

impl Template {
    /// Parses `s`, resolving `env.*` references from the process environment.
    ///
    /// Returns the compiled template plus any warnings (well-formed-but-unknown
    /// references, unset env vars) discovered along the way.
    pub fn parse(s: &str) -> (Template, Vec<String>) {
        Self::parse_with_env(s, &|name| std::env::var(name).ok())
    }

    /// Parses `s`, resolving `env.*` references via the injected `env` lookup
    /// (used by tests to avoid depending on real process environment state).
    pub fn parse_with_env(
        s: &str,
        env: &dyn Fn(&str) -> Option<String>,
    ) -> (Template, Vec<String>) {
        let mut segments: Vec<Segment> = Vec::new();
        let mut warnings: Vec<String> = Vec::new();
        let mut literal = String::new();
        let mut rest = s;

        loop {
            let Some(idx) = rest.find("{{") else {
                literal.push_str(rest);
                break;
            };
            literal.push_str(&rest[..idx]);
            let after_open = &rest[idx + 2..];
            let Some(close_idx) = after_open.find("}}") else {
                // Unclosed: literal to end of input, no warning, no hang.
                literal.push_str(&rest[idx..]);
                break;
            };
            let inner = &after_open[..close_idx];
            let trimmed = inner.trim();
            let full_match = &rest[idx..idx + 2 + close_idx + 2];

            match classify(trimmed, full_match, env) {
                Classified::Ref(r) => {
                    if !literal.is_empty() {
                        segments.push(Segment::Literal(std::mem::take(&mut literal)));
                    }
                    segments.push(Segment::Ref(r));
                }
                Classified::EnvLiteral(value) => literal.push_str(&value),
                Classified::PassThrough => literal.push_str(full_match),
                Classified::Warn(message) => {
                    literal.push_str(full_match);
                    warnings.push(message);
                }
            }

            rest = &after_open[close_idx + 2..];
        }

        if !literal.is_empty() {
            segments.push(Segment::Literal(literal));
        }
        let has_refs = segments.iter().any(|s| matches!(s, Segment::Ref(_)));
        (Template { segments, has_refs }, warnings)
    }

    /// Renders the template against `ctx`. Never touches `$var` syntax.
    ///
    /// Literal-only templates return `Cow::Borrowed` without allocating.
    pub fn render(&self, ctx: &Context) -> Cow<'_, str> {
        if !self.has_refs {
            return Cow::Borrowed(self.literal_str());
        }
        let mut out = String::new();
        for seg in &self.segments {
            match seg {
                Segment::Literal(s) => out.push_str(s),
                Segment::Ref(r) => render_ref(r, ctx, &mut out),
            }
        }
        Cow::Owned(out)
    }

    /// Applies the legacy `$var`/`${var}` pass to the template's *literal*
    /// segments only; rendered `{{...}}` output is appended inert and is
    /// never `$`-processed. Used only by fields that historically supported
    /// `$` interpolation.
    ///
    /// This composition is deliberate, not an optimization: `$`-processing
    /// the fully rendered string would let runtime data (a header, cookie,
    /// query param, ...) that happens to contain `$var`/`${var}` syntax act
    /// as a read primitive against the context (e.g. a client-supplied
    /// header value of `$http_authorization` would leak the Authorization
    /// header) and would corrupt values like `pa$sword4` or `{"$ref":1}`
    /// flowing through a ref — exactly the classes this engine exists to
    /// prevent. For a template with no refs (a single literal segment, or
    /// none), this is byte-identical to the old whole-string pass.
    pub fn render_with_legacy(&self, ctx: &Context) -> String {
        let mut out = String::new();
        for seg in &self.segments {
            match seg {
                Segment::Literal(s) => out.push_str(&crate::vars::interpolate(ctx, s)),
                Segment::Ref(r) => render_ref(r, ctx, &mut out),
            }
        }
        out
    }

    /// Borrows the single literal chunk of a literal-only template (or ""
    /// when the source was empty). Only meaningful when `!self.has_refs`.
    fn literal_str(&self) -> &str {
        match self.segments.first() {
            Some(Segment::Literal(s)) => s.as_str(),
            _ => "",
        }
    }

    /// True when the template has no `{{...}}` references at all (render is
    /// a pure borrow with no per-request work).
    pub fn is_literal(&self) -> bool {
        !self.has_refs
    }

    /// Parses `s` and returns only the warnings, discarding the template.
    /// Used by the compile-time walk that surfaces warnings without
    /// duplicating render behavior.
    pub fn source_warnings_only(s: &str) -> Vec<String> {
        Self::parse(s).1
    }
}

/// Outcome of classifying one `{{...}}` occurrence's trimmed inner text.
enum Classified {
    /// A recognized reference to resolve at render time.
    Ref(TemplateRef),
    /// A parse-time substitution (currently only `env.*`); becomes a plain
    /// literal segment with no warning.
    EnvLiteral(String),
    /// Unrecognized first segment: passed through as a literal, silently.
    PassThrough,
    /// A known namespace with a malformed/unknown remainder, or an unset
    /// `env.*` reference: passed through as a literal, with a warning.
    Warn(String),
}

fn unknown_ref_warning(full_match: &str) -> String {
    format!("unknown template reference '{full_match}' (passed through literally)")
}

fn classify(trimmed: &str, full_match: &str, env: &dyn Fn(&str) -> Option<String>) -> Classified {
    let (first, remainder) = match trimmed.split_once('.') {
        Some((first, remainder)) => (first, remainder),
        None => (trimmed, ""),
    };

    match first {
        "request" => classify_request(remainder, full_match),
        "response" => classify_response(remainder, full_match),
        "message" => classify_message(remainder, full_match),
        "client" => classify_client(remainder, full_match),
        "env" => classify_env(remainder, full_match, env),
        _ => Classified::PassThrough,
    }
}

fn classify_request(remainder: &str, full_match: &str) -> Classified {
    match remainder {
        "method" => Classified::Ref(TemplateRef::RequestMethod),
        "path" => Classified::Ref(TemplateRef::RequestPath),
        "host" => Classified::Ref(TemplateRef::RequestHost),
        "scheme" => Classified::Ref(TemplateRef::RequestScheme),
        "body" => Classified::Ref(TemplateRef::RequestBody),
        _ => {
            if let Some(name) = remainder.strip_prefix("headers.") {
                named_ref(name, full_match, |n| {
                    TemplateRef::RequestHeader(n.to_string())
                })
            } else if let Some(name) = remainder.strip_prefix("query.") {
                named_ref(name, full_match, |n| {
                    TemplateRef::RequestQuery(n.to_string())
                })
            } else if let Some(name) = remainder.strip_prefix("cookies.") {
                named_ref(name, full_match, |n| {
                    TemplateRef::RequestCookie(n.to_string())
                })
            } else {
                Classified::Warn(unknown_ref_warning(full_match))
            }
        }
    }
}

fn classify_response(remainder: &str, full_match: &str) -> Classified {
    match remainder {
        "status" => Classified::Ref(TemplateRef::ResponseStatus),
        "body" => Classified::Ref(TemplateRef::ResponseBody),
        _ => {
            if let Some(name) = remainder.strip_prefix("headers.") {
                named_ref(name, full_match, |n| {
                    TemplateRef::ResponseHeader(n.to_string())
                })
            } else {
                Classified::Warn(unknown_ref_warning(full_match))
            }
        }
    }
}

fn classify_message(remainder: &str, full_match: &str) -> Classified {
    if remainder.is_empty() {
        Classified::Warn(unknown_ref_warning(full_match))
    } else {
        Classified::Ref(TemplateRef::Message(remainder.to_string()))
    }
}

fn classify_client(remainder: &str, full_match: &str) -> Classified {
    match remainder {
        "ip" => Classified::Ref(TemplateRef::ClientIp),
        "port" => Classified::Ref(TemplateRef::ClientPort),
        _ => Classified::Warn(unknown_ref_warning(full_match)),
    }
}

fn classify_env(
    remainder: &str,
    full_match: &str,
    env: &dyn Fn(&str) -> Option<String>,
) -> Classified {
    if remainder.is_empty() {
        return Classified::Warn(unknown_ref_warning(full_match));
    }
    match env(remainder) {
        Some(value) => Classified::EnvLiteral(value),
        None => Classified::Warn(format!(
            "unset environment variable '{remainder}' referenced in template '{full_match}' (passed through literally)"
        )),
    }
}

/// Builds a `Ref` classification for a `<namespace>.<subkey>.<name>` leaf,
/// warning instead when `name` is empty (e.g. `{{request.headers.}}`).
fn named_ref(name: &str, full_match: &str, make: impl FnOnce(&str) -> TemplateRef) -> Classified {
    if name.is_empty() {
        Classified::Warn(unknown_ref_warning(full_match))
    } else {
        Classified::Ref(make(name))
    }
}

fn render_ref(r: &TemplateRef, ctx: &Context, out: &mut String) {
    match r {
        TemplateRef::RequestMethod => out.push_str(&ctx.request.method),
        TemplateRef::RequestPath => out.push_str(&ctx.request.path),
        TemplateRef::RequestHost => out.push_str(&ctx.request.host),
        TemplateRef::RequestScheme => out.push_str(&ctx.request.scheme),
        TemplateRef::RequestBody => {
            out.push_str(&String::from_utf8_lossy(&ctx.request.body));
        }
        TemplateRef::RequestHeader(name) => {
            let key = name.to_lowercase();
            if let Some(v) = ctx.request.headers.get(&key).and_then(|v| v.first()) {
                out.push_str(v);
            }
        }
        TemplateRef::RequestQuery(name) => {
            if let Some(v) = ctx.request.query_params.get(name).and_then(|v| v.first()) {
                out.push_str(v);
            }
        }
        TemplateRef::RequestCookie(name) => {
            if let Some(v) = cookie_value(ctx, name) {
                out.push_str(&v);
            }
        }
        TemplateRef::ResponseStatus => out.push_str(&ctx.response.status_code.to_string()),
        TemplateRef::ResponseBody => {
            out.push_str(&String::from_utf8_lossy(&ctx.response.body));
        }
        TemplateRef::ResponseHeader(name) => {
            let key = name.to_lowercase();
            if let Some(v) = ctx.response.headers.get(&key).and_then(|v| v.first()) {
                out.push_str(v);
            }
        }
        TemplateRef::Message(key) => {
            if let Some(v) = message_str(ctx, key) {
                out.push_str(&v);
            }
        }
        TemplateRef::ClientIp => out.push_str(split_remote_addr(&ctx.request.remote_addr).0),
        TemplateRef::ClientPort => {
            if let Some(port) = split_remote_addr(&ctx.request.remote_addr).1 {
                out.push_str(port);
            }
        }
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
        headers.insert("x-user-id".to_string(), vec!["u-42".to_string()]);
        headers.insert(
            "cookie".to_string(),
            vec!["session=abc123; theme=dark".to_string()],
        );
        let mut query = HashMap::new();
        query.insert(
            "name".to_string(),
            vec!["jack".to_string(), "jill".to_string()],
        );
        let mut message = HashMap::new();
        message.insert("consumer.key_id".to_string(), serde_json::json!("key-123"));
        message.insert("retries".to_string(), serde_json::json!(3));

        Context {
            request: GatewayRequest {
                method: "GET".to_string(),
                path: "/api/users".to_string(),
                host: "example.com".to_string(),
                scheme: "https".to_string(),
                headers,
                query_params: query,
                body: Bytes::from_static(b"req-body"),
                remote_addr: "10.1.2.3:44321".to_string(),
                protocol: Protocol::Http1,
            },
            response: GatewayResponse {
                status_code: 200,
                headers: {
                    let mut h = HashMap::new();
                    h.insert("x-cache".to_string(), vec!["HIT".to_string()]);
                    h
                },
                body: Bytes::from_static(b"resp-body"),
            },
            message,
            errors: Vec::new(),
        }
    }

    fn no_env(_: &str) -> Option<String> {
        None
    }

    // ---- Parsing / structure ----------------------------------------------

    #[test]
    fn test_literal_only_fast_path() {
        let (tpl, warnings) = Template::parse("just plain text, no braces here");
        assert!(warnings.is_empty());
        assert!(tpl.is_literal());
        let ctx = test_ctx();
        match tpl.render(&ctx) {
            Cow::Borrowed(s) => assert_eq!(s, "just plain text, no braces here"),
            Cow::Owned(_) => panic!("literal-only template must render Cow::Borrowed"),
        }
    }

    #[test]
    fn test_parse_known_refs() {
        let (tpl, warnings) = Template::parse("{{request.method}} {{ response.status }}");
        assert!(warnings.is_empty(), "warnings: {warnings:?}");
        assert!(!tpl.is_literal());
        let refs: Vec<&TemplateRef> = tpl
            .segments
            .iter()
            .filter_map(|s| match s {
                Segment::Ref(r) => Some(r),
                _ => None,
            })
            .collect();
        assert_eq!(refs.len(), 2);
        assert_eq!(refs[0], &TemplateRef::RequestMethod);
        assert_eq!(refs[1], &TemplateRef::ResponseStatus);
    }

    #[test]
    fn test_unknown_namespace_passes_silently() {
        for input in ["{{ $1 }}", "{{mustache}}", "{{body.x}}"] {
            let (tpl, warnings) = Template::parse(input);
            assert!(warnings.is_empty(), "input {input}: warnings {warnings:?}");
            assert!(tpl.is_literal(), "input {input}: should be literal");
            let ctx = test_ctx();
            assert_eq!(
                tpl.render(&ctx),
                input,
                "input {input}: should pass through verbatim"
            );
        }
    }

    #[test]
    fn test_known_namespace_bad_leaf_warns() {
        for input in ["{{request.headres.x}}", "{{client.mac}}"] {
            let (tpl, warnings) = Template::parse(input);
            assert_eq!(warnings.len(), 1, "input {input}: warnings {warnings:?}");
            assert!(tpl.is_literal());
            let ctx = test_ctx();
            assert_eq!(tpl.render(&ctx), input);
        }
    }

    #[test]
    fn test_env_substituted_at_parse() {
        let env = |name: &str| -> Option<String> {
            if name == "REGION" {
                Some("eu".to_string())
            } else {
                None
            }
        };
        let (tpl, warnings) = Template::parse_with_env("{{env.REGION}}", &env);
        assert!(warnings.is_empty());
        assert!(tpl.is_literal());
        let ctx = test_ctx();
        assert_eq!(tpl.render(&ctx), "eu");
    }

    #[test]
    fn test_env_unset_passes_and_warns() {
        let (tpl, warnings) = Template::parse_with_env("{{env.NOPE}}", &no_env);
        assert_eq!(warnings.len(), 1);
        assert!(tpl.is_literal());
        let ctx = test_ctx();
        assert_eq!(tpl.render(&ctx), "{{env.NOPE}}");
    }

    #[test]
    fn test_unclosed_braces_literal() {
        let (tpl, warnings) = Template::parse("{{request.method");
        assert!(warnings.is_empty());
        assert!(tpl.is_literal());
        let ctx = test_ctx();
        assert_eq!(tpl.render(&ctx), "{{request.method");
    }

    #[test]
    fn test_message_key_with_dots() {
        let (tpl, warnings) = Template::parse("{{message.consumer.key_id}}");
        assert!(warnings.is_empty());
        assert_eq!(
            tpl.segments,
            vec![Segment::Ref(TemplateRef::Message(
                "consumer.key_id".to_string()
            ))]
        );
    }

    // ---- Rendering ----------------------------------------------------------

    #[test]
    fn test_render_request_basics() {
        let (tpl, _) = Template::parse(
            "{{request.method}} {{request.path}} {{request.host}} {{request.scheme}}",
        );
        let ctx = test_ctx();
        assert_eq!(tpl.render(&ctx), "GET /api/users example.com https");
    }

    #[test]
    fn test_render_headers_case_insensitive_dashes() {
        let (tpl, warnings) = Template::parse("{{request.headers.X-User-ID}}");
        assert!(warnings.is_empty());
        let ctx = test_ctx();
        assert_eq!(tpl.render(&ctx), "u-42");
    }

    #[test]
    fn test_render_response_header_and_status() {
        let (tpl, _) = Template::parse("{{response.status}} {{response.headers.x-cache}}");
        let ctx = test_ctx();
        assert_eq!(tpl.render(&ctx), "200 HIT");
    }

    #[test]
    fn test_render_query_first_value() {
        let (tpl, _) = Template::parse("{{request.query.name}}");
        let ctx = test_ctx();
        assert_eq!(tpl.render(&ctx), "jack");
    }

    #[test]
    fn test_render_cookie() {
        let (tpl, _) = Template::parse("{{request.cookies.theme}}");
        let ctx = test_ctx();
        assert_eq!(tpl.render(&ctx), "dark");
    }

    #[test]
    fn test_render_message_stringified() {
        let (tpl, _) = Template::parse("{{message.consumer.key_id}} {{message.retries}}");
        let ctx = test_ctx();
        assert_eq!(tpl.render(&ctx), "key-123 3");
    }

    #[test]
    fn test_render_client_ip_port() {
        let (tpl, _) = Template::parse("{{client.ip}}:{{client.port}}");
        let ctx = test_ctx();
        assert_eq!(tpl.render(&ctx), "10.1.2.3:44321");
    }

    #[test]
    fn test_render_bodies_lossy() {
        let (tpl, _) = Template::parse("{{request.body}}|{{response.body}}");
        let ctx = test_ctx();
        assert_eq!(tpl.render(&ctx), "req-body|resp-body");
    }

    #[test]
    fn test_absent_subject_renders_empty() {
        let (tpl, _) = Template::parse("[{{request.headers.missing}}]");
        let ctx = test_ctx();
        assert_eq!(tpl.render(&ctx), "[]");
    }

    #[test]
    fn test_dollar_untouched_in_render() {
        let (tpl, _) = Template::parse("pa$sword4 {{request.method}}");
        let ctx = test_ctx();
        assert_eq!(tpl.render(&ctx), "pa$sword4 GET");
    }

    // ---- Legacy interop -------------------------------------------------------

    #[test]
    fn test_render_with_legacy_applies_dollar_after() {
        let (tpl, _) = Template::parse("{{request.method}} $uri");
        let ctx = test_ctx();
        assert_eq!(tpl.render_with_legacy(&ctx), "GET /api/users");
    }

    #[test]
    fn test_render_plain_ignores_dollar() {
        let (tpl, _) = Template::parse("$uri");
        let ctx = test_ctx();
        assert_eq!(tpl.render(&ctx), "$uri");
    }

    #[test]
    fn test_legacy_does_not_interpolate_ref_output() {
        // A ref's rendered value (`pa$sword4`, arriving as runtime data through
        // a header) must survive byte-identical, while a legacy `$uri` literal
        // in the same template still resolves — proving the `$` pass only
        // touches literal segments, not ref output.
        let mut ctx = test_ctx();
        ctx.request
            .headers
            .insert("x-password".to_string(), vec!["pa$sword4".to_string()]);
        let (tpl, _) = Template::parse("{{request.headers.x-password}} $uri");
        assert_eq!(tpl.render_with_legacy(&ctx), "pa$sword4 /api/users");
    }

    #[test]
    fn test_legacy_ref_output_cannot_read_other_vars() {
        // A client-controlled header value that looks like a `$var` reference
        // must not be able to read another part of the context back out when
        // it flows through a ref in a legacy-enabled field.
        let mut ctx = test_ctx();
        ctx.request.headers.insert(
            "authorization".to_string(),
            vec!["Bearer secret-token-xyz".to_string()],
        );
        ctx.request.headers.insert(
            "x-tenant".to_string(),
            vec!["$http_authorization".to_string()],
        );
        let (tpl, _) = Template::parse("tenant={{request.headers.x-tenant}}");
        let rendered = tpl.render_with_legacy(&ctx);
        assert_eq!(rendered, "tenant=$http_authorization");
        assert!(
            !rendered.contains("secret-token-xyz"),
            "ref output must not be re-interpolated as a $var read primitive: {rendered}"
        );
    }
}
