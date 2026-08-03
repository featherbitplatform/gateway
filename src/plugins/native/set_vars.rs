//! `set-vars` — computes named values from the live context into `ctx.message`.
//!
//! Values are `{{...}}` templates rendered at this node's position in the
//! graph (plain `render`, never the legacy `$` pass); results are stored as
//! JSON strings under the raw name, readable downstream as
//! `{{message.<name>}}`, from Lua, and in debug traces. Entries are
//! independent: one var cannot reference another var set by the same node
//! (rendering happens against the context as it entered the node).
//! Runtime-infallible: always exits the success port.

use async_trait::async_trait;
use std::collections::{BTreeMap, HashMap};

use crate::context::Context;
use crate::plugins::{Plugin, PluginOutput, PluginResult};
use crate::vars::template::Template;

#[derive(Debug)]
pub struct SetVarsPlugin {
    /// name → template, BTreeMap for deterministic (alphabetical) insertion order.
    vars: BTreeMap<String, Template>,
}

/// Template-token charset: a name outside it could never be referenced as
/// `{{message.<name>}}` nor autocompleted, so reject it at load.
fn valid_var_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
}

impl SetVarsPlugin {
    /// Builds the plugin from node config.
    ///
    /// `vars` (required) accepts either the YAML-authored map form
    /// (`{ name: value }`) or the UI editor's array-of-records form
    /// (`[{ name, value }]`); at least one entry is required. Each value must
    /// be a string, number, or bool (numbers/bools are stringified); nested
    /// objects/arrays are rejected. Names are restricted to the
    /// `{{message.<name>}}`-safe charset (alphanumeric, `_`, `.`, `-`).
    ///
    /// ```yaml
    /// type: set-vars
    /// config:
    ///   vars:
    ///     tenant: "{{request.headers.x-tenant-id}}"
    /// ```
    pub fn from_config(config: &HashMap<String, serde_json::Value>) -> Result<Self, String> {
        let raw = config
            .get("vars")
            .ok_or_else(|| "set-vars requires a 'vars' object".to_string())?;

        // Accept both the YAML map form and the UI's array-of-records form.
        let pairs: Vec<(String, serde_json::Value)> = match raw {
            serde_json::Value::Object(map) => {
                map.iter().map(|(k, v)| (k.clone(), v.clone())).collect()
            }
            serde_json::Value::Array(seq) => seq
                .iter()
                .map(|item| {
                    let obj = item.as_object().ok_or_else(|| {
                        "set-vars 'vars' array entries must be {name, value} records".to_string()
                    })?;
                    let name = obj.get("name").and_then(|v| v.as_str()).ok_or_else(|| {
                        "set-vars 'vars' array entries need a string 'name'".to_string()
                    })?;
                    Ok((
                        name.to_string(),
                        obj.get("value").cloned().unwrap_or_default(),
                    ))
                })
                .collect::<Result<_, String>>()?,
            _ => {
                return Err("set-vars 'vars' must be an object or an array of {name, value}".into())
            }
        };

        if pairs.is_empty() {
            return Err("set-vars 'vars' must contain at least one variable".into());
        }

        let mut vars = BTreeMap::new();
        for (name, value) in pairs {
            if !valid_var_name(&name) {
                return Err(format!(
                    "set-vars: invalid variable name '{name}' (allowed: A-Z a-z 0-9 _ . -)"
                ));
            }
            let s = match &value {
                serde_json::Value::String(s) => s.clone(),
                serde_json::Value::Number(n) => n.to_string(),
                serde_json::Value::Bool(b) => b.to_string(),
                _ => {
                    return Err(format!(
                        "set-vars: variable '{name}' must be a string, number, or bool"
                    ))
                }
            };
            if vars.insert(name.clone(), Template::parse(&s).0).is_some() {
                return Err(format!("set-vars: duplicate variable name '{name}'"));
            }
        }
        Ok(Self { vars })
    }
}

#[async_trait]
impl Plugin for SetVarsPlugin {
    fn plugin_type(&self) -> &str {
        "set-vars"
    }

    async fn execute(
        &self,
        mut ctx: Context,
        _named_inputs: &HashMap<String, serde_json::Value>,
    ) -> PluginResult {
        // Two passes: render every template against the context as it
        // entered this node, then apply all results. Entries are
        // independent — one var must never observe a sibling's freshly-set
        // value from this same node (see the module doc comment); a single
        // combined loop would let a later-sorting name read an
        // earlier-sorting one that was just inserted.
        let rendered: Vec<(String, String)> = self
            .vars
            .iter()
            .map(|(name, tpl)| (name.clone(), tpl.render(&ctx).into_owned()))
            .collect();

        for (name, value) in rendered {
            ctx.message.insert(name, serde_json::Value::String(value));
        }

        Ok(PluginOutput {
            context: ctx,
            named_outputs: HashMap::new(),
        })
    }
}

#[cfg(test)]
mod tests {
    use crate::context::Context;
    use crate::context::{GatewayRequest, GatewayResponse, Protocol};
    use crate::plugins::Plugin;
    use bytes::Bytes;
    use std::collections::HashMap;

    use super::SetVarsPlugin;

    fn test_context() -> Context {
        let mut headers = HashMap::new();
        headers.insert("x-tenant-id".to_string(), vec!["acme".to_string()]);
        Context {
            request: GatewayRequest {
                method: "GET".to_string(),
                path: "/test".to_string(),
                host: "localhost".to_string(),
                scheme: "http".to_string(),
                headers,
                query_params: HashMap::new(),
                body: Bytes::new(),
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

    #[tokio::test]
    async fn test_set_vars_renders_and_inserts() {
        let mut config = HashMap::new();
        config.insert(
            "vars".into(),
            serde_json::json!({"tenant": "{{request.headers.x-tenant-id}}-prod"}),
        );
        let plugin = SetVarsPlugin::from_config(&config).unwrap();
        let out = plugin
            .execute(test_context(), &HashMap::new())
            .await
            .unwrap();
        assert_eq!(
            out.context.message["tenant"],
            serde_json::json!("acme-prod")
        );
    }

    #[tokio::test]
    async fn test_set_vars_overwrites_existing_key() {
        let mut config = HashMap::new();
        config.insert(
            "vars".into(),
            serde_json::json!({"tenant": "{{request.headers.x-tenant-id}}"}),
        );
        let plugin = SetVarsPlugin::from_config(&config).unwrap();
        let mut ctx = test_context();
        ctx.message
            .insert("tenant".to_string(), serde_json::json!("old"));
        let out = plugin.execute(ctx, &HashMap::new()).await.unwrap();
        assert_eq!(out.context.message["tenant"], serde_json::json!("acme"));
    }

    /// Regression: entries are independent — a var cannot observe a
    /// sibling's freshly-set value from the same node. Rendering happens
    /// against the context as it *entered* the node, so `b`'s
    /// `{{message.a}}` reference sees no pre-existing `message.a` key (the
    /// node hasn't run yet from the template's point of view) and renders
    /// to an empty string, per `Template::render`'s absent-subject behavior
    /// (`TemplateRef::Message` with a missing key contributes nothing to
    /// the rendered output; see `src/vars/template.rs`), not `a`'s value.
    #[tokio::test]
    async fn test_set_vars_entries_are_independent_not_chained() {
        let mut config = HashMap::new();
        config.insert(
            "vars".into(),
            serde_json::json!({
                "a": "hello",
                "b": "{{message.a}}"
            }),
        );
        let plugin = SetVarsPlugin::from_config(&config).unwrap();
        let out = plugin
            .execute(test_context(), &HashMap::new())
            .await
            .unwrap();
        assert_eq!(out.context.message["a"], serde_json::json!("hello"));
        assert_eq!(out.context.message["b"], serde_json::json!(""));
    }

    #[tokio::test]
    async fn test_set_vars_multiple_entries_all_set() {
        let mut config = HashMap::new();
        config.insert(
            "vars".into(),
            serde_json::json!({
                "tenant": "{{request.headers.x-tenant-id}}",
                "path": "{{request.path}}"
            }),
        );
        let plugin = SetVarsPlugin::from_config(&config).unwrap();
        let out = plugin
            .execute(test_context(), &HashMap::new())
            .await
            .unwrap();
        assert_eq!(out.context.message["tenant"], serde_json::json!("acme"));
        assert_eq!(out.context.message["path"], serde_json::json!("/test"));
    }

    #[tokio::test]
    async fn test_set_vars_unknown_ref_passes_through() {
        let mut config = HashMap::new();
        config.insert(
            "vars".into(),
            serde_json::json!({"tenant": "{{request.headres.x}}"}),
        );
        let plugin = SetVarsPlugin::from_config(&config).unwrap();
        let out = plugin
            .execute(test_context(), &HashMap::new())
            .await
            .unwrap();
        assert_eq!(
            out.context.message["tenant"],
            serde_json::json!("{{request.headres.x}}")
        );
    }

    #[tokio::test]
    async fn test_set_vars_number_value_stringified() {
        let mut config = HashMap::new();
        config.insert("vars".into(), serde_json::json!({"retries": 3}));
        let plugin = SetVarsPlugin::from_config(&config).unwrap();
        let out = plugin
            .execute(test_context(), &HashMap::new())
            .await
            .unwrap();
        assert_eq!(out.context.message["retries"], serde_json::json!("3"));
    }

    #[tokio::test]
    async fn test_set_vars_accepts_array_of_records_form() {
        let mut config = HashMap::new();
        config.insert(
            "vars".into(),
            serde_json::json!([{"name": "tenant", "value": "x"}]),
        );
        let plugin = SetVarsPlugin::from_config(&config).unwrap();
        let out = plugin
            .execute(test_context(), &HashMap::new())
            .await
            .unwrap();
        assert_eq!(out.context.message["tenant"], serde_json::json!("x"));
    }

    #[test]
    fn test_set_vars_invalid_name_rejected() {
        let mut config = HashMap::new();
        config.insert("vars".into(), serde_json::json!({"bad name!": "x"}));
        let err = SetVarsPlugin::from_config(&config).unwrap_err();
        assert!(err.contains("bad name!"), "unexpected error: {err}");
    }

    #[test]
    fn test_set_vars_duplicate_name_in_array_form_rejected() {
        let mut config = HashMap::new();
        config.insert(
            "vars".into(),
            serde_json::json!([
                {"name": "a", "value": "1"},
                {"name": "a", "value": "2"}
            ]),
        );
        let err = SetVarsPlugin::from_config(&config).unwrap_err();
        assert!(err.contains('a'), "unexpected error: {err}");
    }

    #[test]
    fn test_set_vars_missing_or_empty_rejected() {
        let config: HashMap<String, serde_json::Value> = HashMap::new();
        assert!(SetVarsPlugin::from_config(&config).is_err());

        let mut config = HashMap::new();
        config.insert("vars".into(), serde_json::json!({}));
        assert!(SetVarsPlugin::from_config(&config).is_err());

        let mut config = HashMap::new();
        config.insert("vars".into(), serde_json::json!([]));
        assert!(SetVarsPlugin::from_config(&config).is_err());
    }

    #[test]
    fn test_set_vars_object_value_rejected() {
        let mut config = HashMap::new();
        config.insert("vars".into(), serde_json::json!({"a": {"nested": true}}));
        assert!(SetVarsPlugin::from_config(&config).is_err());

        let mut config = HashMap::new();
        config.insert("vars".into(), serde_json::json!({"a": [1, 2]}));
        assert!(SetVarsPlugin::from_config(&config).is_err());
    }

    /// Downstream-read integration: a var set here is readable by a later
    /// node's `{{message.*}}` template — the whole point of this plugin.
    #[tokio::test]
    async fn test_set_vars_downstream_read_by_proxy_rewrite() {
        use crate::plugins::native::proxy_rewrite::ProxyRewritePlugin;

        let mut set_vars_config = HashMap::new();
        set_vars_config.insert(
            "vars".into(),
            serde_json::json!({"tenant": "{{request.headers.x-tenant-id}}-prod"}),
        );
        let set_vars = SetVarsPlugin::from_config(&set_vars_config).unwrap();

        let mut rewrite_config = HashMap::new();
        rewrite_config.insert(
            "add_headers".into(),
            serde_json::json!({"x-t": "{{message.tenant}}"}),
        );
        let rewrite = ProxyRewritePlugin::from_config(&rewrite_config).unwrap();

        let after_set_vars = set_vars
            .execute(test_context(), &HashMap::new())
            .await
            .unwrap()
            .context;
        let out = rewrite
            .execute(after_set_vars, &HashMap::new())
            .await
            .unwrap();
        assert_eq!(
            out.context.request.headers.get("x-t"),
            Some(&vec!["acme-prod".to_string()])
        );
    }
}
