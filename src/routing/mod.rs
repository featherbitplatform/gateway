//! Route matching: decides which configured route (and thus which policy
//! graph) handles an incoming request, based on path, method, header, and
//! host rules from `gateway.yaml`.

use crate::config::MatchRule;

/// Matches an incoming request against a route's match rule.
///
/// All criteria present in the rule must match (logical AND); absent
/// criteria match anything. Semantics per criterion:
/// - **path** — exact match, trailing `/*` prefix wildcard, or `*` path
///   segment wildcards (see `match_path`);
/// - **methods** — case-insensitive; an empty list matches any method;
/// - **headers** — every required key must be present with an exactly equal
///   value (header names compared case-insensitively, values exactly);
/// - **host** / **hosts** — the request `Host` (port stripped) must match
///   any configured pattern: exact hostname (case-insensitive) or a
///   single-label wildcard such as `*.example.com` (see `host_pattern_matches`).
///   No `host` and no `hosts` matches every host.
pub fn matches_route(
    rule: &MatchRule,
    path: &str,
    method: &str,
    headers: &[(String, String)],
    host: &str,
) -> bool {
    // Path matching
    if let Some(ref pattern) = rule.path {
        if !match_path(pattern, path) {
            return false;
        }
    }

    // Method matching
    if !rule.methods.is_empty() {
        let method_upper = method.to_uppercase();
        if !rule
            .methods
            .iter()
            .any(|m| m.to_uppercase() == method_upper)
        {
            return false;
        }
    }

    // Header matching
    for (required_key, required_value) in &rule.headers {
        let key_lower = required_key.to_lowercase();
        let found = headers
            .iter()
            .any(|(k, v)| k.to_lowercase() == key_lower && v == required_value);
        if !found {
            return false;
        }
    }

    // Host matching: any configured pattern may match.
    let mut patterns = rule.host.iter().chain(rule.hosts.iter()).peekable();
    if patterns.peek().is_some() {
        let host = strip_host_port(host);
        if !patterns.any(|p| host_pattern_matches(p, host)) {
            return false;
        }
    }

    true
}

/// Drops a trailing `:port` from a `Host` header value, leaving IPv6
/// literals (`[::1]:8080` → `[::1]`) intact.
fn strip_host_port(host: &str) -> &str {
    if host.starts_with('[') {
        match host.find(']') {
            Some(end) => &host[..=end],
            None => host,
        }
    } else {
        match host.rfind(':') {
            Some(idx) => &host[..idx],
            None => host,
        }
    }
}

/// Matches a (port-less) host against a route host pattern, ASCII
/// case-insensitively. `*.example.com` matches exactly one leading label:
/// `api.example.com` but not `example.com` or `a.b.example.com`. Same
/// semantics as the SNI matcher in `crate::stream::sni`, without allocating.
fn host_pattern_matches(pattern: &str, host: &str) -> bool {
    match pattern.strip_prefix("*.") {
        Some(base) => {
            if host.len() <= base.len() + 1 {
                return false;
            }
            let split = host.len() - base.len();
            let (prefix, suffix) = host.split_at(split);
            suffix.eq_ignore_ascii_case(base)
                && prefix.ends_with('.')
                && !prefix[..prefix.len() - 1].contains('.')
        }
        None => pattern.eq_ignore_ascii_case(host),
    }
}

/// Checks a `host`/`hosts` pattern at route-table build time, so a pattern
/// that could never match is rejected with a clear error instead of
/// silently dropping traffic.
pub fn validate_host_pattern(pattern: &str) -> Result<(), String> {
    if pattern.is_empty() {
        return Err("host pattern must not be empty".into());
    }
    if pattern.chars().any(|c| c.is_whitespace() || c == '/') {
        return Err(format!(
            "host pattern '{pattern}' must be a bare hostname (no path, scheme, or whitespace)"
        ));
    }
    if !pattern.starts_with('[') {
        if let Some((_, port)) = pattern.rsplit_once(':') {
            if !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()) {
                return Err(format!(
                    "host pattern '{pattern}' must not include a port; the request port is ignored when matching"
                ));
            }
        }
    }
    let rest = match pattern.strip_prefix("*.") {
        Some("") => {
            return Err(format!(
                "host pattern '{pattern}' needs a domain after the wildcard, e.g. '*.example.com'"
            ))
        }
        Some(rest) => rest,
        None => pattern,
    };
    if rest.contains('*') {
        return Err(format!(
            "host pattern '{pattern}' may only use a single leading '*.' wildcard, e.g. '*.example.com'"
        ));
    }
    Ok(())
}

/// Validates every host pattern of a route match rule.
pub fn validate_match_rule(rule: &MatchRule) -> Result<(), String> {
    for pattern in rule.host.iter().chain(rule.hosts.iter()) {
        validate_host_pattern(pattern)?;
    }
    Ok(())
}

/// Matches a path pattern against an actual path.
/// Supports:
///   - Exact match: `/api/v1/users`
///   - Prefix with wildcard: `/api/v1/*`
///   - Glob segments: `/api/*/users`
///
/// A trailing `/*` matches the bare prefix itself (`/api/v1`) and anything
/// below it (`/api/v1/users/123`), but not sibling prefixes (`/api/v10`).
/// A `*` segment matches exactly one path segment, so segment patterns
/// require the same number of segments as the path.
fn match_path(pattern: &str, path: &str) -> bool {
    if pattern == path {
        return true;
    }

    // Trailing wildcard: /api/v1/* matches /api/v1/anything
    if let Some(prefix) = pattern.strip_suffix("/*") {
        return path == prefix || path.starts_with(&format!("{}/", prefix));
    }

    // Segment wildcards: /api/*/users
    let pattern_parts: Vec<&str> = pattern.split('/').collect();
    let path_parts: Vec<&str> = path.split('/').collect();

    if pattern_parts.len() != path_parts.len() {
        return false;
    }

    pattern_parts
        .iter()
        .zip(path_parts.iter())
        .all(|(p, s)| *p == "*" || p == s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn rule(path: Option<&str>, methods: &[&str]) -> MatchRule {
        MatchRule {
            path: path.map(String::from),
            methods: methods.iter().map(|s| s.to_string()).collect(),
            headers: HashMap::new(),
            host: None,
            hosts: Vec::new(),
        }
    }

    #[test]
    fn test_exact_path() {
        let r = rule(Some("/api/v1/users"), &[]);
        assert!(matches_route(&r, "/api/v1/users", "GET", &[], ""));
        assert!(!matches_route(&r, "/api/v1/other", "GET", &[], ""));
    }

    #[test]
    fn test_wildcard_path() {
        let r = rule(Some("/api/v1/*"), &[]);
        assert!(matches_route(&r, "/api/v1/users", "GET", &[], ""));
        assert!(matches_route(&r, "/api/v1/users/123", "GET", &[], ""));
        assert!(matches_route(&r, "/api/v1", "GET", &[], ""));
        assert!(!matches_route(&r, "/api/v2/users", "GET", &[], ""));
    }

    #[test]
    fn test_segment_wildcard() {
        let r = rule(Some("/api/*/users"), &[]);
        assert!(matches_route(&r, "/api/v1/users", "GET", &[], ""));
        assert!(matches_route(&r, "/api/v2/users", "GET", &[], ""));
        assert!(!matches_route(&r, "/api/v1/posts", "GET", &[], ""));
    }

    #[test]
    fn test_method_filter() {
        let r = rule(Some("/api/*"), &["GET", "POST"]);
        assert!(matches_route(&r, "/api/users", "GET", &[], ""));
        assert!(matches_route(&r, "/api/users", "POST", &[], ""));
        assert!(!matches_route(&r, "/api/users", "DELETE", &[], ""));
    }

    #[test]
    fn test_header_filter() {
        let mut r = rule(Some("/api/*"), &[]);
        r.headers
            .insert("x-api-version".to_string(), "1".to_string());

        let headers = vec![("x-api-version".to_string(), "1".to_string())];
        assert!(matches_route(&r, "/api/users", "GET", &headers, ""));

        let headers = vec![("x-api-version".to_string(), "2".to_string())];
        assert!(!matches_route(&r, "/api/users", "GET", &headers, ""));
    }

    #[test]
    fn test_host_filter() {
        let mut r = rule(Some("/api/*"), &[]);
        r.host = Some("example.com".to_string());

        assert!(matches_route(&r, "/api/users", "GET", &[], "example.com"));
        assert!(!matches_route(&r, "/api/users", "GET", &[], "other.com"));
    }

    #[test]
    fn test_host_filter_ignores_request_port() {
        let mut r = rule(Some("/api/*"), &[]);
        r.host = Some("example.com".to_string());

        assert!(matches_route(
            &r,
            "/api/users",
            "GET",
            &[],
            "example.com:8080"
        ));
        assert!(matches_route(
            &r,
            "/api/users",
            "GET",
            &[],
            "EXAMPLE.com:443"
        ));
        assert!(!matches_route(
            &r,
            "/api/users",
            "GET",
            &[],
            "other.com:8080"
        ));
    }

    #[test]
    fn test_host_filter_ipv6_literal_with_port() {
        let mut r = rule(None, &[]);
        r.host = Some("[::1]".to_string());

        assert!(matches_route(&r, "/", "GET", &[], "[::1]:8080"));
        assert!(matches_route(&r, "/", "GET", &[], "[::1]"));
        assert!(!matches_route(&r, "/", "GET", &[], "[::2]:8080"));
    }

    #[test]
    fn test_host_wildcard_matches_exactly_one_label() {
        let mut r = rule(None, &[]);
        r.host = Some("*.example.com".to_string());

        assert!(matches_route(&r, "/", "GET", &[], "api.example.com"));
        assert!(matches_route(&r, "/", "GET", &[], "API.Example.COM:8443"));
        assert!(!matches_route(&r, "/", "GET", &[], "example.com"));
        assert!(!matches_route(&r, "/", "GET", &[], "a.b.example.com"));
        assert!(!matches_route(&r, "/", "GET", &[], "notexample.com"));
    }

    #[test]
    fn test_hosts_list_matches_any_entry() {
        let mut r = rule(None, &[]);
        r.hosts = vec!["a.example.com".to_string(), "*.example.org".to_string()];

        assert!(matches_route(&r, "/", "GET", &[], "a.example.com"));
        assert!(matches_route(&r, "/", "GET", &[], "x.example.org"));
        assert!(!matches_route(&r, "/", "GET", &[], "b.example.com"));
        assert!(!matches_route(&r, "/", "GET", &[], ""));
    }

    #[test]
    fn test_host_and_hosts_are_merged() {
        let mut r = rule(None, &[]);
        r.host = Some("legacy.example.com".to_string());
        r.hosts = vec!["new.example.com".to_string()];

        assert!(matches_route(&r, "/", "GET", &[], "legacy.example.com"));
        assert!(matches_route(&r, "/", "GET", &[], "new.example.com"));
        assert!(!matches_route(&r, "/", "GET", &[], "other.example.com"));
    }

    #[test]
    fn test_no_host_rule_matches_every_host() {
        let r = rule(None, &[]);
        assert!(matches_route(&r, "/", "GET", &[], ""));
        assert!(matches_route(
            &r,
            "/",
            "GET",
            &[],
            "anything.example.com:1234"
        ));
    }

    #[test]
    fn test_validate_host_pattern() {
        for ok in [
            "example.com",
            "*.example.com",
            "[::1]",
            "localhost",
            "a.b.c.d",
        ] {
            assert!(validate_host_pattern(ok).is_ok(), "{ok}");
        }
        for bad in [
            "",
            "*",
            "*.",
            "*example.com",
            "api.*.example.com",
            "*.*.example.com",
            "example.com:8080",
            "example.com/path",
            "bad host",
        ] {
            assert!(validate_host_pattern(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn test_validate_match_rule_reports_offending_host() {
        let mut r = rule(None, &[]);
        r.hosts = vec!["ok.example.com".to_string(), "*".to_string()];
        let err = validate_match_rule(&r).unwrap_err();
        assert!(err.contains("'*'"), "{err}");
        r.hosts.pop();
        assert!(validate_match_rule(&r).is_ok());
    }
}
