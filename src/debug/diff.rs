//! What a plugin changed, derived from two consecutive snapshots.
//!
//! This is computed at **read time** (when a trace is fetched), not while
//! recording. Two reasons: the traced request path stays as cheap as possible,
//! and the comparison stays a pure function with no engine coupling — trivially
//! unit-testable in isolation.
//!
//! The comparison is purpose-built rather than a generic JSON differ: it knows
//! that headers are name → values maps and that `errors` is append-only, so it
//! can emit paths a policy author recognises (`request.headers.x-userinfo`,
//! `message.user_id`) instead of structural noise.

use serde::Serialize;

use super::trace::ContextSnapshot;

/// How a field changed between two steps.
#[derive(Debug, Clone, Copy, Serialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ChangeKind {
    Added,
    Removed,
    Modified,
}

/// One field-level difference.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Change {
    /// Dotted path, e.g. `request.headers.x-userinfo` or `response.status_code`.
    pub path: String,
    pub kind: ChangeKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub before: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub after: Option<String>,
}

impl Change {
    fn added(path: String, after: String) -> Self {
        Self {
            path,
            kind: ChangeKind::Added,
            before: None,
            after: Some(after),
        }
    }
    fn removed(path: String, before: String) -> Self {
        Self {
            path,
            kind: ChangeKind::Removed,
            before: Some(before),
            after: None,
        }
    }
    fn modified(path: String, before: String, after: String) -> Self {
        Self {
            path,
            kind: ChangeKind::Modified,
            before: Some(before),
            after: Some(after),
        }
    }
}

/// Computes what changed between two consecutive snapshots.
///
/// Ordering is deterministic (request scalars, request headers, query params,
/// request body, response, message, errors — each map walked in `BTreeMap`
/// order), so the UI and tests see a stable list.
pub fn diff(prev: &ContextSnapshot, next: &ContextSnapshot) -> Vec<Change> {
    let mut out = Vec::new();

    scalar(
        &mut out,
        "request.method",
        &prev.request.method,
        &next.request.method,
    );
    scalar(
        &mut out,
        "request.path",
        &prev.request.path,
        &next.request.path,
    );
    scalar(
        &mut out,
        "request.host",
        &prev.request.host,
        &next.request.host,
    );
    scalar(
        &mut out,
        "request.scheme",
        &prev.request.scheme,
        &next.request.scheme,
    );

    multi_map(
        &mut out,
        "request.headers",
        &prev.request.headers,
        &next.request.headers,
    );
    multi_map(
        &mut out,
        "request.query_params",
        &prev.request.query_params,
        &next.request.query_params,
    );
    if prev.request.body.len != next.request.body.len {
        out.push(Change::modified(
            "request.body".to_string(),
            format!("{} bytes", prev.request.body.len),
            format!("{} bytes", next.request.body.len),
        ));
    }

    if prev.response.status_code != next.response.status_code {
        out.push(Change::modified(
            "response.status_code".to_string(),
            prev.response.status_code.to_string(),
            next.response.status_code.to_string(),
        ));
    }
    multi_map(
        &mut out,
        "response.headers",
        &prev.response.headers,
        &next.response.headers,
    );
    if prev.response.body.len != next.response.body.len {
        out.push(Change::modified(
            "response.body".to_string(),
            format!("{} bytes", prev.response.body.len),
            format!("{} bytes", next.response.body.len),
        ));
    }

    // `message` values are arbitrary JSON; render them compactly.
    for (k, v) in &next.message {
        match prev.message.get(k) {
            None => out.push(Change::added(format!("message.{k}"), compact(v))),
            Some(old) if old != v => out.push(Change::modified(
                format!("message.{k}"),
                compact(old),
                compact(v),
            )),
            Some(_) => {}
        }
    }
    for k in prev.message.keys() {
        if !next.message.contains_key(k) {
            out.push(Change::removed(
                format!("message.{k}"),
                compact(&prev.message[k]),
            ));
        }
    }

    // `errors` is append-only in the engine, so report the new entries.
    if next.errors.len() > prev.errors.len() {
        for (i, e) in next.errors.iter().enumerate().skip(prev.errors.len()) {
            out.push(Change::added(
                format!("errors[{i}]"),
                format!("{}: {}", e.code, e.message),
            ));
        }
    }

    out
}

fn scalar(out: &mut Vec<Change>, path: &str, prev: &str, next: &str) {
    if prev != next {
        out.push(Change::modified(
            path.to_string(),
            prev.to_string(),
            next.to_string(),
        ));
    }
}

/// Compares a `name -> values` map, reporting per-name adds/removes/modifies.
fn multi_map(
    out: &mut Vec<Change>,
    prefix: &str,
    prev: &std::collections::BTreeMap<String, Vec<String>>,
    next: &std::collections::BTreeMap<String, Vec<String>>,
) {
    for (k, v) in next {
        match prev.get(k) {
            None => out.push(Change::added(format!("{prefix}.{k}"), v.join(", "))),
            Some(old) if old != v => out.push(Change::modified(
                format!("{prefix}.{k}"),
                old.join(", "),
                v.join(", "),
            )),
            Some(_) => {}
        }
    }
    for (k, v) in prev {
        if !next.contains_key(k) {
            out.push(Change::removed(format!("{prefix}.{k}"), v.join(", ")));
        }
    }
}

fn compact(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::{Context, GatewayError, GatewayRequest, GatewayResponse, Protocol};
    use crate::debug::trace::{CaptureOptions, PreviousBodies};
    use bytes::Bytes;
    use std::collections::HashMap;

    fn base() -> Context {
        Context {
            request: GatewayRequest {
                method: "GET".to_string(),
                path: "/api/hello".to_string(),
                host: "h".to_string(),
                scheme: "http".to_string(),
                headers: HashMap::new(),
                query_params: HashMap::new(),
                body: Bytes::new(),
                remote_addr: "1.2.3.4:5".to_string(),
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

    fn snap(c: &Context) -> ContextSnapshot {
        ContextSnapshot::capture(c, &CaptureOptions::default(), &PreviousBodies::default())
    }

    fn find<'a>(changes: &'a [Change], path: &str) -> &'a Change {
        changes
            .iter()
            .find(|c| c.path == path)
            .unwrap_or_else(|| panic!("no change at {path}; got {changes:?}"))
    }

    #[test]
    fn test_identical_snapshots_have_no_changes() {
        let c = base();
        assert!(diff(&snap(&c), &snap(&c)).is_empty());
    }

    #[test]
    fn test_path_rewrite_detected() {
        let before = snap(&base());
        let mut after_ctx = base();
        after_ctx.request.path = "/hello".to_string();
        let changes = diff(&before, &snap(&after_ctx));
        let c = find(&changes, "request.path");
        assert_eq!(c.kind, ChangeKind::Modified);
        assert_eq!(c.before.as_deref(), Some("/api/hello"));
        assert_eq!(c.after.as_deref(), Some("/hello"));
    }

    #[test]
    fn test_header_added_modified_removed() {
        let mut start = base();
        start
            .request
            .headers
            .insert("x-keep".to_string(), vec!["1".to_string()]);
        start
            .request
            .headers
            .insert("x-drop".to_string(), vec!["old".to_string()]);
        let before = snap(&start);

        let mut end = base();
        end.request
            .headers
            .insert("x-keep".to_string(), vec!["2".to_string()]);
        end.request
            .headers
            .insert("x-new".to_string(), vec!["fresh".to_string()]);
        let changes = diff(&before, &snap(&end));

        assert_eq!(
            find(&changes, "request.headers.x-keep").kind,
            ChangeKind::Modified
        );
        assert_eq!(
            find(&changes, "request.headers.x-new").kind,
            ChangeKind::Added
        );
        assert_eq!(
            find(&changes, "request.headers.x-drop").kind,
            ChangeKind::Removed
        );
    }

    #[test]
    fn test_status_and_response_header_change() {
        let before = snap(&base());
        let mut end = base();
        end.response.status_code = 403;
        end.response.headers.insert(
            "access-control-allow-origin".to_string(),
            vec!["*".to_string()],
        );
        let changes = diff(&before, &snap(&end));
        assert_eq!(
            find(&changes, "response.status_code").after.as_deref(),
            Some("403")
        );
        assert_eq!(
            find(&changes, "response.headers.access-control-allow-origin").kind,
            ChangeKind::Added
        );
    }

    #[test]
    fn test_message_key_added() {
        let before = snap(&base());
        let mut end = base();
        end.message
            .insert("user_id".to_string(), serde_json::json!("alice"));
        let changes = diff(&before, &snap(&end));
        let c = find(&changes, "message.user_id");
        assert_eq!(c.kind, ChangeKind::Added);
        // Strings render bare, not JSON-quoted.
        assert_eq!(c.after.as_deref(), Some("alice"));
    }

    #[test]
    fn test_body_size_change_reported_without_capture() {
        let before = snap(&base());
        let mut end = base();
        end.response.body = Bytes::from_static(b"hello");
        let changes = diff(&before, &snap(&end));
        // Bodies are not captured here, yet the size delta is still visible.
        assert_eq!(
            find(&changes, "response.body").after.as_deref(),
            Some("5 bytes")
        );
    }

    #[test]
    fn test_appended_error_reported() {
        let before = snap(&base());
        let mut end = base();
        end.errors.push(GatewayError {
            node_id: "auth".to_string(),
            code: "UNAUTHORIZED".to_string(),
            message: "no token".to_string(),
            metadata: HashMap::new(),
        });
        let changes = diff(&before, &snap(&end));
        let c = find(&changes, "errors[0]");
        assert_eq!(c.kind, ChangeKind::Added);
        assert_eq!(c.after.as_deref(), Some("UNAUTHORIZED: no token"));
    }

    /// Deterministic ordering is what makes the UI stable and these tests
    /// meaningful; it comes from the snapshot's BTreeMaps.
    #[test]
    fn test_change_order_is_deterministic() {
        let before = snap(&base());
        let mut end = base();
        for name in ["x-c", "x-a", "x-b"] {
            end.request
                .headers
                .insert(name.to_string(), vec!["v".to_string()]);
        }
        let first = diff(&before, &snap(&end));
        let again = diff(&before, &snap(&end));
        assert_eq!(first, again);
        let paths: Vec<&str> = first.iter().map(|c| c.path.as_str()).collect();
        assert_eq!(
            paths,
            vec![
                "request.headers.x-a",
                "request.headers.x-b",
                "request.headers.x-c"
            ]
        );
    }
}
