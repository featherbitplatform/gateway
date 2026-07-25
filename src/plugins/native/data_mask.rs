//! The `data-mask` node — masks sensitive fields in the request before they
//! reach loggers or the upstream: query parameters, headers, and JSON body
//! fields can be removed, replaced with a fixed value, or partially rewritten
//! with a regex substitution.
//!
//! Port of APISIX's `data-mask` plugin. Deviations from the Lua original:
//! - Body rules support `body_format: json` only; `urlencoded` is rejected at
//!   config load.
//! - Body field selection uses **dotted paths** (`user.cards.0.number`), not
//!   JSONPath — no `$..` recursive descent or wildcards. A leading `$.` is
//!   tolerated and stripped. Purely numeric segments index into arrays.
//! - APISIX runs this in the log phase (masking what loggers see); featherbit
//!   runs it wherever the node sits in the graph — place it before `logging`
//!   and/or `upstream` nodes that must not see the raw values.

use async_trait::async_trait;
use bytes::Bytes;
use regex::Regex;
use std::collections::HashMap;

use crate::context::Context;
use crate::plugins::{Plugin, PluginOutput, PluginResult};

/// Masks request query parameters, headers, and JSON body fields according
/// to a list of rules. Masking is best-effort and never fails at execution
/// time: bodies that are absent, non-JSON, or larger than `max_body_size`
/// simply skip the body rules. The plugin only touches `context.request`;
/// it never writes `context.message` or `context.errors`.
pub struct DataMaskPlugin {
    rules: Vec<MaskRule>,
    max_body_size: usize,
}

/// Where a rule applies.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Target {
    Query,
    Header,
    Body,
}

/// What a rule does to the matched field.
enum Action {
    /// Delete the field entirely.
    Remove,
    /// Overwrite the field with a fixed value.
    Replace(String),
    /// Rewrite the first regex match inside a string value (mirrors APISIX's
    /// `ngx.re.sub`, which substitutes only the first occurrence).
    Regex { regex: Regex, value: String },
}

/// One masking rule from the `request` array.
struct MaskRule {
    target: Target,
    /// Query/header name, or the raw dotted path for body rules.
    name: String,
    /// Parsed dotted path; only populated for body rules.
    path: Vec<PathSeg>,
    action: Action,
}

/// One segment of a dotted body path.
#[derive(Debug, Clone, PartialEq)]
enum PathSeg {
    Key(String),
    Index(usize),
}

/// Parses a dotted path (`user.cards.0.number`), tolerating a leading `$.`
/// or `$`. Purely numeric segments become array indices. Empty segments
/// (e.g. `a..b`) are rejected.
fn parse_dotted_path(raw: &str) -> Result<Vec<PathSeg>, String> {
    let trimmed = raw
        .strip_prefix("$.")
        .or_else(|| raw.strip_prefix('$'))
        .unwrap_or(raw);
    if trimmed.is_empty() {
        return Err(format!("body rule path '{}' is empty", raw));
    }
    trimmed
        .split('.')
        .map(|seg| {
            if seg.is_empty() {
                Err(format!("body rule path '{}' has an empty segment", raw))
            } else if let Ok(idx) = seg.parse::<usize>() {
                Ok(PathSeg::Index(idx))
            } else {
                Ok(PathSeg::Key(seg.to_string()))
            }
        })
        .collect()
}

/// Applies `action` to the value addressed by `path` inside a parsed JSON
/// document. Returns `true` if anything changed. Paths that do not resolve
/// (missing key, out-of-range index, type mismatch) are silently skipped,
/// as are regex actions on non-string values — mirroring the Lua plugin's
/// warn-and-continue behavior.
fn apply_to_json(root: &mut serde_json::Value, path: &[PathSeg], action: &Action) -> bool {
    let (last, parents) = match path.split_last() {
        Some(pair) => pair,
        None => return false,
    };

    // Walk to the parent of the addressed value.
    let mut current = root;
    for seg in parents {
        current = match seg {
            PathSeg::Key(k) => match current.get_mut(k.as_str()) {
                Some(v) => v,
                None => return false,
            },
            PathSeg::Index(i) => match current.get_mut(*i) {
                Some(v) => v,
                None => return false,
            },
        };
    }

    match (last, current) {
        (PathSeg::Key(k), serde_json::Value::Object(map)) => {
            if !map.contains_key(k.as_str()) {
                return false;
            }
            match action {
                Action::Remove => map.remove(k.as_str()).is_some(),
                Action::Replace(value) => {
                    map.insert(k.clone(), serde_json::Value::String(value.clone()));
                    true
                }
                Action::Regex { regex, value } => {
                    if let Some(serde_json::Value::String(s)) = map.get_mut(k.as_str()) {
                        regex_mask(s, regex, value)
                    } else {
                        false
                    }
                }
            }
        }
        (PathSeg::Index(i), serde_json::Value::Array(arr)) => {
            if *i >= arr.len() {
                return false;
            }
            match action {
                Action::Remove => {
                    arr.remove(*i);
                    true
                }
                Action::Replace(value) => {
                    arr[*i] = serde_json::Value::String(value.clone());
                    true
                }
                Action::Regex { regex, value } => {
                    if let serde_json::Value::String(s) = &mut arr[*i] {
                        regex_mask(s, regex, value)
                    } else {
                        false
                    }
                }
            }
        }
        _ => false,
    }
}

/// Replaces the first regex match in `s` with `replacement` (which may use
/// `$1`-style capture references). Returns `true` if a match was rewritten.
fn regex_mask(s: &mut String, regex: &Regex, replacement: &str) -> bool {
    if !regex.is_match(s) {
        return false;
    }
    *s = regex.replace(s, replacement).into_owned();
    true
}

/// Applies a rule to a multi-valued map (query params or headers). Regex
/// actions run against every value of the field; `replace` collapses the
/// field to the single configured value.
fn apply_to_multimap(map: &mut HashMap<String, Vec<String>>, name: &str, action: &Action) -> bool {
    if !map.contains_key(name) {
        return false;
    }
    match action {
        Action::Remove => map.remove(name).is_some(),
        Action::Replace(value) => {
            map.insert(name.to_string(), vec![value.clone()]);
            true
        }
        Action::Regex { regex, value } => {
            let mut masked = false;
            if let Some(values) = map.get_mut(name) {
                for v in values.iter_mut() {
                    if regex_mask(v, regex, value) {
                        masked = true;
                    }
                }
            }
            masked
        }
    }
}

impl DataMaskPlugin {
    /// Builds the plugin from node config.
    ///
    /// Accepted keys:
    /// - `request` (array of rule objects, default `[]`): each rule has
    ///   - `type` (string, required): `query`, `header`, or `body`.
    ///   - `name` (string, required): the query parameter / header name, or a
    ///     dotted path into the JSON body (`user.cards.0.number`; numeric
    ///     segments index arrays, a leading `$.` is tolerated).
    ///   - `action` (string, required): `remove`, `replace`, or `regex`.
    ///   - `value` (string): replacement value; required for `replace` and
    ///     `regex` (for `regex` it may use `$1` capture references).
    ///   - `regex` (string): pattern; required for `action: regex`, compiled
    ///     here at config load.
    ///   - `body_format` (string): required for `type: body`; only `json` is
    ///     supported (**deviation**: APISIX also accepts `urlencoded`).
    /// - `max_body_size` (integer > 0, default `1048576`): bodies larger than
    ///   this skip body rules.
    ///
    /// ```yaml
    /// type: data-mask
    /// config:
    ///   request:
    ///     - { type: header, name: authorization, action: replace, value: "***" }
    ///     - { type: query, name: token, action: remove }
    ///     - type: body
    ///       body_format: json
    ///       name: user.card_number
    ///       action: regex
    ///       regex: "^(\\d{4})\\d+(\\d{4})$"
    ///       value: "$1********$2"
    /// ```
    pub fn from_config(config: &HashMap<String, serde_json::Value>) -> Result<Self, String> {
        let mut rules = Vec::new();

        if let Some(raw) = config.get("request") {
            let items = raw
                .as_array()
                .ok_or("data-mask: 'request' must be an array of rule objects")?;
            for (idx, item) in items.iter().enumerate() {
                rules.push(parse_rule(item).map_err(|e| format!("data-mask rule {}: {}", idx, e))?);
            }
        }

        let max_body_size = match config.get("max_body_size") {
            None => 1024 * 1024,
            Some(v) => {
                let n = v
                    .as_u64()
                    .filter(|n| *n > 0)
                    .ok_or("data-mask: 'max_body_size' must be a positive integer")?;
                n as usize
            }
        };

        Ok(Self {
            rules,
            max_body_size,
        })
    }
}

/// Parses and validates one rule object, compiling regexes and dotted paths.
fn parse_rule(item: &serde_json::Value) -> Result<MaskRule, String> {
    let obj = item.as_object().ok_or("must be an object")?;

    let get_str = |key: &str| obj.get(key).and_then(|v| v.as_str());

    let target = match get_str("type") {
        Some("query") => Target::Query,
        Some("header") => Target::Header,
        Some("body") => Target::Body,
        Some(other) => return Err(format!("unknown type '{}' (query|header|body)", other)),
        None => return Err("'type' is required".to_string()),
    };

    let name = get_str("name")
        .filter(|s| !s.is_empty())
        .ok_or("'name' is required")?
        .to_string();

    if target == Target::Body {
        match get_str("body_format") {
            Some("json") => {}
            Some(other) => {
                return Err(format!(
                    "body_format '{}' is not supported — featherbit implements 'json' only",
                    other
                ));
            }
            None => return Err("'body_format' is required for body rules".to_string()),
        }
    }

    let action = match get_str("action") {
        Some("remove") => Action::Remove,
        Some("replace") => {
            let value = get_str("value").ok_or("'value' is required for action 'replace'")?;
            Action::Replace(value.to_string())
        }
        Some("regex") => {
            let pattern = get_str("regex").ok_or("'regex' is required for action 'regex'")?;
            let value = get_str("value").ok_or("'value' is required for action 'regex'")?;
            let regex =
                Regex::new(pattern).map_err(|e| format!("invalid regex '{}': {}", pattern, e))?;
            Action::Regex {
                regex,
                value: value.to_string(),
            }
        }
        Some(other) => return Err(format!("unknown action '{}' (remove|replace|regex)", other)),
        None => return Err("'action' is required".to_string()),
    };

    let path = if target == Target::Body {
        parse_dotted_path(&name)?
    } else {
        Vec::new()
    };

    Ok(MaskRule {
        target,
        name,
        path,
        action,
    })
}

#[async_trait]
impl Plugin for DataMaskPlugin {
    fn plugin_type(&self) -> &str {
        "data-mask"
    }

    async fn execute(
        &self,
        mut ctx: Context,
        _named_inputs: &HashMap<String, serde_json::Value>,
    ) -> PluginResult {
        // Body rules share one lazily-parsed JSON document.
        let mut json_body: Option<serde_json::Value> = None;
        let mut body_masked = false;

        for rule in &self.rules {
            match rule.target {
                Target::Query => {
                    apply_to_multimap(&mut ctx.request.query_params, &rule.name, &rule.action);
                }
                Target::Header => {
                    apply_to_multimap(
                        &mut ctx.request.headers,
                        &rule.name.to_lowercase(),
                        &rule.action,
                    );
                }
                Target::Body => {
                    if ctx.request.body.is_empty() || ctx.request.body.len() > self.max_body_size {
                        continue; // absent or oversized body: skip, never fail
                    }
                    if json_body.is_none() {
                        match serde_json::from_slice(&ctx.request.body) {
                            Ok(parsed) => json_body = Some(parsed),
                            Err(_) => continue, // non-JSON body: skip body rules
                        }
                    }
                    if let Some(doc) = json_body.as_mut() {
                        if apply_to_json(doc, &rule.path, &rule.action) {
                            body_masked = true;
                        }
                    }
                }
            }
        }

        if body_masked {
            if let Some(doc) = &json_body {
                // Body-mutation convention: the server layer recomputes
                // content-length from the final body.
                ctx.request.body = Bytes::from(serde_json::to_vec(doc).unwrap_or_default());
                ctx.request.headers.remove("content-length");
            }
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

    fn test_context(body: &str) -> Context {
        let mut headers = HashMap::new();
        headers.insert(
            "authorization".to_string(),
            vec!["Bearer secret-token".to_string()],
        );
        headers.insert("content-length".to_string(), vec![body.len().to_string()]);
        let mut query = HashMap::new();
        query.insert("token".to_string(), vec!["abc123".to_string()]);
        query.insert("name".to_string(), vec!["jack".to_string()]);

        Context {
            request: GatewayRequest {
                method: "POST".to_string(),
                path: "/api".to_string(),
                host: "localhost".to_string(),
                scheme: "http".to_string(),
                headers,
                query_params: query,
                body: Bytes::from(body.to_string()),
                remote_addr: "127.0.0.1:12345".to_string(),
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

    fn plugin(rules: serde_json::Value) -> DataMaskPlugin {
        let mut config = HashMap::new();
        config.insert("request".to_string(), rules);
        DataMaskPlugin::from_config(&config).unwrap()
    }

    #[tokio::test]
    async fn test_data_mask_query_remove_and_replace() {
        let p = plugin(serde_json::json!([
            { "type": "query", "name": "token", "action": "remove" },
            { "type": "query", "name": "name", "action": "replace", "value": "***" }
        ]));
        let out = p.execute(test_context(""), &HashMap::new()).await.unwrap();
        assert!(!out.context.request.query_params.contains_key("token"));
        assert_eq!(
            out.context.request.query_params.get("name"),
            Some(&vec!["***".to_string()])
        );
    }

    #[tokio::test]
    async fn test_data_mask_header_regex() {
        let p = plugin(serde_json::json!([
            { "type": "header", "name": "Authorization", "action": "regex",
              "regex": "Bearer .*", "value": "Bearer ***" }
        ]));
        let out = p.execute(test_context(""), &HashMap::new()).await.unwrap();
        assert_eq!(
            out.context.request.headers.get("authorization"),
            Some(&vec!["Bearer ***".to_string()])
        );
    }

    #[tokio::test]
    async fn test_data_mask_body_nested_and_array_paths() {
        let body = r#"{"user":{"name":"jack","cards":[{"number":"4111111111111111"},{"number":"5500000000000004"}]},"password":"hunter2"}"#;
        let p = plugin(serde_json::json!([
            { "type": "body", "body_format": "json", "name": "password", "action": "remove" },
            { "type": "body", "body_format": "json", "name": "user.cards.0.number",
              "action": "regex", "regex": r"^(\d{4})\d+(\d{4})$", "value": "$1********$2" },
            { "type": "body", "body_format": "json", "name": "user.cards.1.number",
              "action": "replace", "value": "MASKED" },
            { "type": "body", "body_format": "json", "name": "user.missing", "action": "remove" }
        ]));
        let out = p
            .execute(test_context(body), &HashMap::new())
            .await
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_slice(&out.context.request.body).unwrap();
        assert!(parsed.get("password").is_none());
        assert_eq!(parsed["user"]["cards"][0]["number"], "4111********1111");
        assert_eq!(parsed["user"]["cards"][1]["number"], "MASKED");
        // body was rewritten -> stale content-length removed
        assert!(!out.context.request.headers.contains_key("content-length"));
    }

    #[tokio::test]
    async fn test_data_mask_body_array_element_remove() {
        let body = r#"{"items":["a","b","c"]}"#;
        let p = plugin(serde_json::json!([
            { "type": "body", "body_format": "json", "name": "items.1", "action": "remove" }
        ]));
        let out = p
            .execute(test_context(body), &HashMap::new())
            .await
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_slice(&out.context.request.body).unwrap();
        assert_eq!(parsed["items"], serde_json::json!(["a", "c"]));
    }

    #[tokio::test]
    async fn test_data_mask_non_json_body_skipped() {
        let p = plugin(serde_json::json!([
            { "type": "body", "body_format": "json", "name": "password", "action": "remove" }
        ]));
        let ctx = test_context("not json at all");
        let out = p.execute(ctx, &HashMap::new()).await.unwrap();
        assert_eq!(out.context.request.body, Bytes::from("not json at all"));
        // untouched body -> content-length preserved
        assert!(out.context.request.headers.contains_key("content-length"));
    }

    #[tokio::test]
    async fn test_data_mask_oversized_body_skipped() {
        let mut config = HashMap::new();
        config.insert(
            "request".to_string(),
            serde_json::json!([
                { "type": "body", "body_format": "json", "name": "a", "action": "remove" }
            ]),
        );
        config.insert("max_body_size".to_string(), serde_json::json!(4));
        let p = DataMaskPlugin::from_config(&config).unwrap();
        let out = p
            .execute(test_context(r#"{"a":1}"#), &HashMap::new())
            .await
            .unwrap();
        assert_eq!(out.context.request.body, Bytes::from(r#"{"a":1}"#));
    }

    #[tokio::test]
    async fn test_data_mask_regex_first_match_only() {
        let body = r#"{"note":"id=123 id=456"}"#;
        let p = plugin(serde_json::json!([
            { "type": "body", "body_format": "json", "name": "note",
              "action": "regex", "regex": r"id=\d+", "value": "id=***" }
        ]));
        let out = p
            .execute(test_context(body), &HashMap::new())
            .await
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_slice(&out.context.request.body).unwrap();
        // mirrors ngx.re.sub: only the first occurrence is rewritten
        assert_eq!(parsed["note"], "id=*** id=456");
    }

    #[test]
    fn test_data_mask_config_rejections() {
        let bad = [
            // urlencoded body_format is a documented deviation
            serde_json::json!([{ "type": "body", "body_format": "urlencoded", "name": "a", "action": "remove" }]),
            // body rule without body_format
            serde_json::json!([{ "type": "body", "name": "a", "action": "remove" }]),
            // regex action without pattern
            serde_json::json!([{ "type": "query", "name": "a", "action": "regex", "value": "x" }]),
            // replace without value
            serde_json::json!([{ "type": "query", "name": "a", "action": "replace" }]),
            // invalid regex
            serde_json::json!([{ "type": "query", "name": "a", "action": "regex", "regex": "(", "value": "x" }]),
            // unknown type / action
            serde_json::json!([{ "type": "cookie", "name": "a", "action": "remove" }]),
            serde_json::json!([{ "type": "query", "name": "a", "action": "obfuscate" }]),
            // empty dotted path segment
            serde_json::json!([{ "type": "body", "body_format": "json", "name": "a..b", "action": "remove" }]),
        ];
        for rules in bad {
            let mut config = HashMap::new();
            config.insert("request".to_string(), rules.clone());
            assert!(
                DataMaskPlugin::from_config(&config).is_err(),
                "should reject: {rules}"
            );
        }
    }

    #[test]
    fn test_data_mask_dotted_path_parsing() {
        assert_eq!(
            parse_dotted_path("$.user.cards.0").unwrap(),
            vec![
                PathSeg::Key("user".to_string()),
                PathSeg::Key("cards".to_string()),
                PathSeg::Index(0)
            ]
        );
        assert_eq!(
            parse_dotted_path("a").unwrap(),
            vec![PathSeg::Key("a".to_string())]
        );
        assert!(parse_dotted_path("").is_err());
        assert!(parse_dotted_path("$.").is_err());
    }
}
