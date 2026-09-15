//! Derived variables plugin (`set-vars`, featherbit-native): pull values out
//! of the context — a path segment, a header, a query parameter, a JSON body
//! field — optionally through a JSONPath and/or a regex capture, and store
//! them in `context.message` so every downstream node can read them as
//! `$msg_<name>` / `{{message.<name>}}`.
//!
//! Always pass-through: wire `success` onward; the `error` port is never
//! taken. Malformed regexes and JSONPaths reject the policy at compile time.

use async_trait::async_trait;
use regex::Regex;
use serde_json_path::JsonPath;
use std::collections::{HashMap, HashSet};

use crate::context::Context;
use crate::plugins::{Plugin, PluginOutput, PluginResult};
use crate::vars::template::Template;

/// Evaluates every rule in order and writes each result into
/// `context.message[name]` as a string. Later rules see earlier results
/// (`$msg_<name>`), so rules can build on each other.
pub struct SetVarsPlugin {
    vars: Vec<VarRule>,
}

impl std::fmt::Debug for SetVarsPlugin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SetVarsPlugin")
            .field(
                "vars",
                &self.vars.iter().map(|v| &v.name).collect::<Vec<_>>(),
            )
            .finish()
    }
}

/// Which regex capture becomes the value.
enum Group {
    /// `0` is the whole match.
    Index(usize),
    Name(String),
}

struct VarRule {
    name: String,
    /// The source text: a `{{namespace.path}}` / `$var` template. Defaults to
    /// `$request_body` when only `json_path` is given.
    from: Template,
    /// Optional RFC 9535 JSONPath applied to `from` parsed as JSON.
    json_path: Option<JsonPath>,
    /// Optional regex applied to the (JSONPath-reduced) text.
    regex: Option<(Regex, Group)>,
    /// Used when the source is absent/non-JSON, the path matches nothing,
    /// or the regex does not match. Without it the value is `""`.
    default: Option<String>,
}

const ALLOWED_KEYS: &[&str] = &["name", "from", "json_path", "regex", "group", "default"];

fn str_field<'a>(
    obj: &'a serde_json::Map<String, serde_json::Value>,
    key: &str,
    at: &str,
) -> Result<Option<&'a str>, String> {
    match obj.get(key) {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(serde_json::Value::String(s)) => Ok(Some(s.as_str())),
        Some(_) => Err(format!("{at}.{key} must be a string")),
    }
}

impl SetVarsPlugin {
    /// Builds the plugin from node config; every regex and JSONPath is
    /// compiled here so mistakes fail policy compilation, not requests.
    ///
    /// Accepted keys:
    /// - `vars` (array, **required**, non-empty), each entry:
    ///   - `name` (string, **required**): message key; readable downstream as
    ///     `$msg_<name>` / `{{message.<name>}}`. Letters, digits, `_`, `-`, `.`.
    ///   - `from` (string): source template — `$uri`, `$http_<h>`, `$arg_<q>`,
    ///     `$cookie_<c>`, `$msg_<k>`, or `{{request.path}}`-style references.
    ///     Required unless `json_path` is set (then defaults to the request body).
    ///   - `json_path` (string): RFC 9535 JSONPath applied to `from` parsed as
    ///     JSON. One scalar node → its text; one object/array → its JSON text;
    ///     several nodes → a JSON array text; none → `default`.
    ///   - `regex` (string): applied after `json_path`; `group` (integer or
    ///     capture name, default `1`; `0` = whole match) selects the value.
    ///   - `default` (string): fallback when nothing matches.
    ///
    /// ```yaml
    /// type: set-vars
    /// config:
    ///   vars:
    ///     - { name: user, from: $uri, regex: '^/hello/([^/]+)', default: stranger }
    ///     - { name: tenant, from: $http_x_tenant }
    ///     - { name: order_id, json_path: $.order.id }
    /// ```
    pub fn from_config(config: &HashMap<String, serde_json::Value>) -> Result<Self, String> {
        let raw = config
            .get("vars")
            .ok_or("set-vars requires a 'vars' array")?
            .as_array()
            .filter(|a| !a.is_empty())
            .ok_or("set-vars requires a non-empty 'vars' array")?;

        let mut seen = HashSet::new();
        let mut vars = Vec::with_capacity(raw.len());
        for (idx, entry) in raw.iter().enumerate() {
            let at = format!("vars[{idx}]");
            let obj = entry
                .as_object()
                .ok_or_else(|| format!("{at} must be an object"))?;
            if let Some(k) = obj.keys().find(|k| !ALLOWED_KEYS.contains(&k.as_str())) {
                return Err(format!("{at}: unknown key '{k}'"));
            }

            let name = str_field(obj, "name", &at)?
                .filter(|n| !n.is_empty())
                .ok_or_else(|| format!("{at}.name is required"))?;
            if !name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
            {
                return Err(format!(
                    "{at}.name '{name}' may only contain letters, digits, '_', '-' and '.'"
                ));
            }
            if !seen.insert(name.to_string()) {
                return Err(format!("{at}: duplicate name '{name}'"));
            }

            let json_path = match str_field(obj, "json_path", &at)? {
                None => None,
                Some(p) => Some(
                    JsonPath::parse(p)
                        .map_err(|e| format!("{at}.json_path: invalid JSONPath '{p}': {e}"))?,
                ),
            };
            let from_src = match (str_field(obj, "from", &at)?, json_path.is_some()) {
                (Some(s), _) => s.to_string(),
                (None, true) => "$request_body".to_string(),
                (None, false) => {
                    return Err(format!(
                        "{at}: 'from' is required unless 'json_path' is set"
                    ))
                }
            };
            let (from, _warnings) = Template::parse(&from_src);

            let regex = match str_field(obj, "regex", &at)? {
                None => {
                    if obj.contains_key("group") {
                        return Err(format!("{at}.group needs a 'regex'"));
                    }
                    None
                }
                Some(pattern) => {
                    let re = Regex::new(pattern)
                        .map_err(|e| format!("{at}.regex: invalid regex: {e}"))?;
                    let group = match obj.get("group") {
                        None => Group::Index(1.min(re.captures_len() - 1)),
                        Some(serde_json::Value::Number(n)) => {
                            let i = n.as_u64().ok_or_else(|| {
                                format!("{at}.group must be a non-negative integer")
                            })? as usize;
                            if i >= re.captures_len() {
                                return Err(format!(
                                    "{at}.group {i} is out of range: the regex has {} capture group(s)",
                                    re.captures_len() - 1
                                ));
                            }
                            Group::Index(i)
                        }
                        // The UI form sends the group as text: "2" means index 2.
                        Some(serde_json::Value::String(g)) if g.parse::<usize>().is_ok() => {
                            let i: usize = g.parse().unwrap_or_default();
                            if i >= re.captures_len() {
                                return Err(format!(
                                    "{at}.group {i} is out of range: the regex has {} capture group(s)",
                                    re.captures_len() - 1
                                ));
                            }
                            Group::Index(i)
                        }
                        Some(serde_json::Value::String(g)) => {
                            if !re.capture_names().any(|n| n == Some(g.as_str())) {
                                return Err(format!(
                                    "{at}.group '{g}' is not a named capture group of the regex"
                                ));
                            }
                            Group::Name(g.clone())
                        }
                        Some(_) => {
                            return Err(format!(
                                "{at}.group must be an integer or a capture-group name"
                            ))
                        }
                    };
                    Some((re, group))
                }
            };

            let default = str_field(obj, "default", &at)?.map(str::to_string);

            vars.push(VarRule {
                name: name.to_string(),
                from,
                json_path,
                regex,
                default,
            });
        }
        Ok(Self { vars })
    }
}

/// Text for a JSONPath selection: one scalar → its text, one container → its
/// JSON, several nodes → a JSON array; nothing → `None`.
fn json_nodes_text(nodes: &[&serde_json::Value]) -> Option<String> {
    match nodes {
        [] => None,
        [serde_json::Value::String(s)] => Some(s.clone()),
        [one] => Some(one.to_string()),
        many => {
            Some(serde_json::Value::Array(many.iter().map(|v| (*v).clone()).collect()).to_string())
        }
    }
}

impl VarRule {
    fn evaluate(&self, ctx: &Context) -> Option<String> {
        let mut value = self.from.render_with_legacy(ctx);
        if let Some(path) = &self.json_path {
            let doc: serde_json::Value = serde_json::from_str(&value).ok()?;
            value = json_nodes_text(&path.query(&doc).all())?;
        }
        if let Some((re, group)) = &self.regex {
            let caps = re.captures(&value)?;
            let m = match group {
                Group::Index(i) => caps.get(*i),
                Group::Name(n) => caps.name(n),
            }?;
            value = m.as_str().to_string();
        }
        Some(value)
    }
}

#[async_trait]
impl Plugin for SetVarsPlugin {
    fn plugin_type(&self) -> &str {
        "set-vars"
    }

    async fn execute(&self, mut ctx: Context) -> PluginResult {
        for rule in &self.vars {
            let value = rule
                .evaluate(&ctx)
                .filter(|v| !v.is_empty())
                .or_else(|| rule.default.clone())
                .unwrap_or_default();
            ctx.message
                .insert(rule.name.clone(), serde_json::Value::String(value));
        }
        Ok(PluginOutput::success(ctx))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::{Context, GatewayRequest, GatewayResponse, Protocol};
    use bytes::Bytes;
    use std::collections::HashMap;

    fn ctx(path: &str, headers: &[(&str, &str)], query: &[(&str, &str)], body: &str) -> Context {
        Context {
            request: GatewayRequest {
                method: "GET".to_string(),
                path: path.to_string(),
                host: "example.com".to_string(),
                scheme: "http".to_string(),
                headers: headers
                    .iter()
                    .map(|(k, v)| (k.to_string(), vec![v.to_string()]))
                    .collect(),
                query_params: query
                    .iter()
                    .map(|(k, v)| (k.to_string(), vec![v.to_string()]))
                    .collect(),
                body: Bytes::from(body.to_string()),
                remote_addr: "10.1.2.3:44321".to_string(),
                protocol: Protocol::Http1,
            },
            response: GatewayResponse {
                status_code: 0,
                headers: HashMap::new(),
                body: Bytes::new(),
                stream: None,
            },
            message: HashMap::new(),
            errors: Vec::new(),
        }
    }

    fn plugin(config: serde_json::Value) -> Result<SetVarsPlugin, String> {
        let map: HashMap<String, serde_json::Value> = serde_json::from_value(config).unwrap();
        SetVarsPlugin::from_config(&map)
    }

    async fn run(config: serde_json::Value, c: Context) -> Context {
        let out = plugin(config).unwrap().execute(c).await.unwrap();
        assert!(out.port.is_none(), "always the success port");
        out.context
    }

    fn msg<'a>(c: &'a Context, key: &str) -> Option<&'a str> {
        c.message.get(key).and_then(|v| v.as_str())
    }

    #[tokio::test]
    async fn test_path_segment_via_regex_capture() {
        let out = run(
            serde_json::json!({"vars": [
                {"name": "user", "from": "$uri", "regex": "^/hello/([^/]+)"}
            ]}),
            ctx("/hello/frenk", &[], &[], ""),
        )
        .await;
        assert_eq!(msg(&out, "user"), Some("frenk"));
        // Visible to the var engine as `$msg_user`.
        assert_eq!(
            crate::vars::interpolate(&out, "hello $msg_user"),
            "hello frenk"
        );
    }

    #[tokio::test]
    async fn test_plain_copy_header_and_template_source() {
        let out = run(
            serde_json::json!({"vars": [
                {"name": "tenant", "from": "$http_x_tenant"},
                {"name": "where", "from": "{{request.method}} {{request.path}}"}
            ]}),
            ctx("/x", &[("x-tenant", "acme")], &[], ""),
        )
        .await;
        assert_eq!(msg(&out, "tenant"), Some("acme"));
        assert_eq!(msg(&out, "where"), Some("GET /x"));
    }

    #[tokio::test]
    async fn test_query_param_with_default_when_absent_or_no_match() {
        let out = run(
            serde_json::json!({"vars": [
                {"name": "plan", "from": "$arg_plan", "default": "free"},
                {"name": "user", "from": "$uri", "regex": "^/hello/([^/]+)", "default": "stranger"},
                {"name": "empty", "from": "$uri", "regex": "^/nope/(.*)"}
            ]}),
            ctx("/hello/", &[], &[], ""),
        )
        .await;
        assert_eq!(msg(&out, "plan"), Some("free"));
        assert_eq!(msg(&out, "user"), Some("stranger"));
        assert_eq!(
            msg(&out, "empty"),
            Some(""),
            "no match and no default → empty string"
        );
    }

    #[tokio::test]
    async fn test_json_path_on_request_body() {
        let body = r#"{"order":{"id":42,"ok":true,"tags":["a","b"],"ship":{"city":"Turin"}}}"#;
        let out = run(
            serde_json::json!({"vars": [
                {"name": "order_id", "json_path": "$.order.id"},
                {"name": "ok", "json_path": "$.order.ok"},
                {"name": "tags", "json_path": "$.order.tags[*]"},
                {"name": "ship", "json_path": "$.order.ship"},
                {"name": "missing", "json_path": "$.order.none", "default": "n/a"},
                {"name": "city_upper", "json_path": "$.order.ship.city", "regex": "^(T[a-z]+)"}
            ]}),
            ctx("/orders", &[], &[], body),
        )
        .await;
        assert_eq!(msg(&out, "order_id"), Some("42"));
        assert_eq!(msg(&out, "ok"), Some("true"));
        assert_eq!(
            msg(&out, "tags"),
            Some(r#"["a","b"]"#),
            "several nodes → JSON array text"
        );
        assert_eq!(
            msg(&out, "ship"),
            Some(r#"{"city":"Turin"}"#),
            "object → JSON text"
        );
        assert_eq!(msg(&out, "missing"), Some("n/a"));
        assert_eq!(msg(&out, "city_upper"), Some("Turin"));
    }

    #[tokio::test]
    async fn test_json_path_on_an_explicit_source_and_non_json_input() {
        let out = run(
            serde_json::json!({"vars": [
                {"name": "sub", "from": "$http_x_claims", "json_path": "$.sub"},
                {"name": "bad", "from": "$uri", "json_path": "$.a", "default": "fallback"}
            ]}),
            ctx("/x", &[("x-claims", r#"{"sub":"alice"}"#)], &[], "not json"),
        )
        .await;
        assert_eq!(msg(&out, "sub"), Some("alice"));
        assert_eq!(
            msg(&out, "bad"),
            Some("fallback"),
            "non-JSON source → default"
        );
    }

    #[tokio::test]
    async fn test_regex_groups_by_index_name_and_whole_match() {
        let out = run(
            serde_json::json!({"vars": [
                {"name": "minor", "from": "$http_x_version", "regex": r"^(?P<major>\d+)\.(?P<minor>\d+)", "group": "minor"},
                {"name": "major", "from": "$http_x_version", "regex": r"^(\d+)\.(\d+)", "group": 1},
                {"name": "whole", "from": "$http_x_version", "regex": r"\d+\.\d+", "group": 0},
                {"name": "minor_text", "from": "$http_x_version", "regex": r"^(\d+)\.(\d+)", "group": "2"}
            ]}),
            ctx("/x", &[("x-version", "3.14.1")], &[], ""),
        )
        .await;
        assert_eq!(msg(&out, "minor"), Some("14"));
        assert_eq!(msg(&out, "major"), Some("3"));
        assert_eq!(msg(&out, "whole"), Some("3.14"));
        assert_eq!(
            msg(&out, "minor_text"),
            Some("14"),
            "a numeric string group is an index (UI form)"
        );
    }

    #[tokio::test]
    async fn test_later_vars_can_read_earlier_ones() {
        let out = run(
            serde_json::json!({"vars": [
                {"name": "user", "from": "$uri", "regex": "^/hello/([^/]+)"},
                {"name": "greeting", "from": "hello $msg_user"}
            ]}),
            ctx("/hello/frenk", &[], &[], ""),
        )
        .await;
        assert_eq!(msg(&out, "greeting"), Some("hello frenk"));
    }

    #[test]
    fn test_config_errors() {
        let err = |c: serde_json::Value| plugin(c).unwrap_err();
        assert!(err(serde_json::json!({})).contains("vars"));
        assert!(err(serde_json::json!({"vars": []})).contains("non-empty"));
        assert!(err(serde_json::json!({"vars": [{"from": "$uri"}]})).contains("name"));
        assert!(
            err(serde_json::json!({"vars": [{"name": "a b", "from": "$uri"}]})).contains("name")
        );
        assert!(err(serde_json::json!({"vars": [{"name": "x"}]})).contains("from"));
        assert!(
            err(serde_json::json!({"vars": [{"name": "x", "from": "$uri", "regex": "("}]}))
                .contains("regex")
        );
        assert!(err(serde_json::json!({"vars": [{"name": "x", "from": "$uri", "regex": "^(a)", "group": 2}]}))
            .contains("group"));
        assert!(err(serde_json::json!({"vars": [{"name": "x", "from": "$uri", "regex": "^(a)", "group": "nope"}]}))
            .contains("group"));
        assert!(
            err(serde_json::json!({"vars": [{"name": "x", "from": "$uri", "group": 1}]}))
                .contains("group")
        );
        assert!(
            err(serde_json::json!({"vars": [{"name": "x", "json_path": "$["}]}))
                .contains("JSONPath")
        );
        assert!(
            err(serde_json::json!({"vars": [{"name": "x", "from": "$uri", "bogus": 1}]}))
                .contains("bogus")
        );
        assert!(err(serde_json::json!({"vars": [{"name": "x", "from": "$uri"}, {"name": "x", "from": "$uri"}]}))
            .contains("duplicate"));
    }
}
