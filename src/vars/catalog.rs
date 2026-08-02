//! Machine-readable catalog of every variable [`super::resolve`] supports —
//! the single source the Admin API (`GET /api/vars`), the UI autocomplete,
//! and the var legend consume. Guarded against drift from the resolver by
//! `test_catalog_matches_resolver`, which parses `resolve()`'s source.

use serde::Serialize;

/// Whether an entry is a fixed name or a `prefix_*` family.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum VarKind {
    Static,
    Family,
}

/// One catalog row. `family_source` names the context collection that
/// populates a family's live suggestions in the UI.
#[derive(Debug, Serialize)]
pub struct VarEntry {
    pub name: &'static str,
    pub kind: VarKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub family_source: Option<&'static str>,
    pub description: &'static str,
    pub example: &'static str,
}

const S: VarKind = VarKind::Static;
const F: VarKind = VarKind::Family;

fn e(
    name: &'static str,
    kind: VarKind,
    family_source: Option<&'static str>,
    description: &'static str,
    example: &'static str,
) -> VarEntry {
    VarEntry {
        name,
        kind,
        family_source,
        description,
        example,
    }
}

/// Every variable `resolve()` accepts, statics first, then families.
pub fn var_catalog() -> Vec<VarEntry> {
    vec![
        e("uri", S, None, "Request path (no query string)", "$uri"),
        e(
            "request_uri",
            S,
            None,
            "Path plus ?query when query params exist",
            "$request_uri",
        ),
        e(
            "method",
            S,
            None,
            "HTTP method (alias: request_method)",
            "$method",
        ),
        e(
            "request_method",
            S,
            None,
            "HTTP method (alias of method)",
            "$request_method",
        ),
        e("host", S, None, "Request Host", "$host"),
        e("scheme", S, None, "http or https", "$scheme"),
        e(
            "protocol",
            S,
            None,
            "HTTP protocol version (http1, http2, ...)",
            "$protocol",
        ),
        e(
            "remote_addr",
            S,
            None,
            "Client IP without port",
            "$remote_addr",
        ),
        e("remote_port", S, None, "Client port", "$remote_port"),
        e(
            "query_string",
            S,
            None,
            "Full query string, rebuilt and sorted",
            "$query_string",
        ),
        e("status", S, None, "Response status code", "$status"),
        e(
            "resp_body",
            S,
            None,
            "Response body (lossy UTF-8)",
            "$resp_body",
        ),
        e(
            "request_body",
            S,
            None,
            "Request body (lossy UTF-8)",
            "$request_body",
        ),
        e(
            "consumer_name",
            S,
            None,
            "Authenticated consumer name (set by auth plugins)",
            "$consumer_name",
        ),
        e(
            "consumer_group_id",
            S,
            None,
            "Authenticated consumer group",
            "$consumer_group_id",
        ),
        e(
            "arg_*",
            F,
            Some("query_params"),
            "First value of a query parameter",
            "$arg_page",
        ),
        e(
            "http_*",
            F,
            Some("request_headers"),
            "First value of a request header (underscores map to dashes)",
            "$http_user_agent",
        ),
        e(
            "cookie_*",
            F,
            Some("cookies"),
            "Value from the Cookie request header",
            "$cookie_session",
        ),
        e(
            "post_arg_*",
            F,
            Some("form_body"),
            "Form field from an application/x-www-form-urlencoded body",
            "$post_arg_username",
        ),
        e(
            "msg_*",
            F,
            Some("message"),
            "Any context.message key, stringified; dotted keys need ${msg_key.with.dots}",
            "${msg_consumer.name}",
        ),
        e(
            "sent_http_*",
            F,
            Some("response_headers"),
            "First value of a response header (underscores map to dashes)",
            "$sent_http_content_type",
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    /// The catalog must track resolve()'s source exactly, both directions.
    /// Statics are quoted names on match-arm lines containing "=>"; families
    /// are the strip_prefix("...") literals. Same source-parsing guard style
    /// as KNOWN_PLUGIN_TYPES.
    #[test]
    fn test_catalog_matches_resolver() {
        let src = include_str!("mod.rs");
        // Limit the scan to resolve()'s body: from `pub fn resolve` to the
        // next `pub fn` after it.
        let start = src.find("pub fn resolve").expect("resolve fn present");
        let rest = &src[start..];
        let end = rest[10..]
            .find("pub fn ")
            .map(|i| i + 10)
            .unwrap_or(rest.len());
        let body = &rest[..end];

        let mut from_source: BTreeSet<String> = BTreeSet::new();
        for line in body.lines() {
            let t = line.trim();
            if t.contains("=>") {
                // every quoted token on an arm line is a static var name
                let mut s = t;
                while let Some(open) = s.find('"') {
                    let after = &s[open + 1..];
                    if let Some(close) = after.find('"') {
                        let name = &after[..close];
                        if !name.is_empty()
                            && name.chars().all(|c| c.is_ascii_lowercase() || c == '_')
                        {
                            from_source.insert(name.to_string());
                        }
                        s = &after[close + 1..];
                    } else {
                        break;
                    }
                }
            }
            if let Some(idx) = t.find("strip_prefix(\"") {
                let after = &t[idx + 14..];
                if let Some(close) = after.find('"') {
                    from_source.insert(format!("{}*", &after[..close]));
                }
            }
        }
        // message_str constants referenced by consumer arms appear as quoted
        // strings on arm lines ("consumer.name"/"consumer.group") — they are
        // lookup keys, not var names; strip them.
        from_source.remove("consumer.name");
        from_source.remove("consumer.group");

        let from_catalog: BTreeSet<String> =
            var_catalog().iter().map(|v| v.name.to_string()).collect();

        assert_eq!(from_catalog, from_source, "catalog drifted from resolve()");
    }

    #[test]
    fn test_catalog_families_have_sources_and_statics_do_not() {
        for v in var_catalog() {
            match v.kind {
                VarKind::Family => {
                    assert!(
                        v.family_source.is_some(),
                        "{} missing family_source",
                        v.name
                    );
                    assert!(
                        v.name.ends_with("_*"),
                        "{} family name must end in _*",
                        v.name
                    );
                }
                VarKind::Static => assert!(v.family_source.is_none(), "{}", v.name),
            }
        }
    }
}
