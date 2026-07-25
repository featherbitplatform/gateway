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
/// - **host** — case-insensitive equality.
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

    // Host matching
    if let Some(ref required_host) = rule.host {
        if !host.eq_ignore_ascii_case(required_host) {
            return false;
        }
    }

    true
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
}
