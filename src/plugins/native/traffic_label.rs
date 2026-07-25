//! Traffic labeling plugin (`traffic-label`) — a faithful subset of Apache
//! APISIX's `traffic-label` plugin: match requests with condition expressions
//! and tag them, picking among weighted actions.
//!
//! The APISIX plugin supports one action kind, `set_headers`, plus a `weight`
//! for weighted selection between an action group's entries; featherbit adds
//! `set_labels`, which writes `label.<key>` entries into `context.message` so
//! downstream nodes (logging, scripts, upstream selection) can read the label
//! without touching the wire request.
//!
//! This node never fails at execution time and is always pass-through: wire
//! `success` onward; the `error` port is never taken.

use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::context::Context;
use crate::plugins::{Plugin, PluginOutput, PluginResult};
use crate::vars::{interpolate, Expr};

/// Applies the first matching rule's action: header values and label values
/// are `$var` templates interpolated per request.
pub struct TrafficLabelPlugin {
    rules: Vec<Rule>,
}

struct Rule {
    /// One APISIX triple-array expression (rules AND-ed); `None` matches all.
    matcher: Option<Expr>,
    actions: Vec<ActionEntry>,
    total_weight: u64,
    /// Round-robin cursor for weighted selection between `actions`.
    cursor: AtomicU64,
}

struct ActionEntry {
    weight: u64,
    /// Lowercased header name → value template, set on the request.
    set_headers: Vec<(String, String)>,
    /// Label key → value template, written to `message["label.<key>"]`.
    set_labels: Vec<(String, String)>,
}

/// Parses a `{name: scalar-template}` object into ordered pairs; numbers and
/// bools are stringified, nested shapes are rejected.
fn parse_kv_object(v: &serde_json::Value, field: &str) -> Result<Vec<(String, String)>, String> {
    let obj = v
        .as_object()
        .ok_or_else(|| format!("{field} must be an object of name: value"))?;
    obj.iter()
        .map(|(k, v)| {
            let value = match v {
                serde_json::Value::String(s) => s.clone(),
                serde_json::Value::Number(n) => n.to_string(),
                serde_json::Value::Bool(b) => b.to_string(),
                _ => return Err(format!("{field}['{k}'] must be a scalar value")),
            };
            Ok((k.clone(), value))
        })
        .collect()
}

impl TrafficLabelPlugin {
    /// Builds the plugin from node config. All expressions are validated
    /// here at config load.
    ///
    /// Accepted keys:
    /// - `rules` (array, **required**, non-empty): evaluated in order; the
    ///   first rule whose `match` passes applies one of its actions.
    ///   - `match` (array, optional): APISIX triple-array condition, rules
    ///     AND-ed. Omit to match every request.
    ///   - `actions` (array of objects, **required**, non-empty): one entry
    ///     is chosen by weighted round-robin. Each entry:
    ///     - `set_headers` (object `{name: value}`): request headers to set
    ///       (replacing existing values); values support `$var` interpolation.
    ///     - `set_labels` (object `{key: value}`, featherbit extension):
    ///       written to `context.message` as `label.<key>`; values support
    ///       `$var` interpolation.
    ///     - `weight` (integer >= 1, default `1`): selection weight.
    ///
    /// ```yaml
    /// type: traffic-label
    /// config:
    ///   rules:
    ///     - match:
    ///         - ["arg_channel", "==", "beta"]
    ///       actions:
    ///         - set_headers:
    ///             x-server-id: beta
    ///           set_labels:
    ///             tier: "beta"
    ///           weight: 3
    ///         - set_headers:
    ///             x-server-id: stable
    ///           weight: 1
    /// ```
    pub fn from_config(config: &HashMap<String, serde_json::Value>) -> Result<Self, String> {
        let raw_rules = config
            .get("rules")
            .and_then(|v| v.as_array())
            .filter(|r| !r.is_empty())
            .ok_or("traffic-label requires a non-empty 'rules' array")?;

        let mut rules = Vec::with_capacity(raw_rules.len());
        for (idx, raw) in raw_rules.iter().enumerate() {
            let obj = raw
                .as_object()
                .ok_or_else(|| format!("rules[{idx}] must be an object"))?;

            let matcher = match obj.get("match") {
                None => None,
                Some(v) => Some(Expr::parse(v).map_err(|e| format!("rules[{idx}].match: {e}"))?),
            };

            let raw_actions = obj
                .get("actions")
                .and_then(|v| v.as_array())
                .filter(|a| !a.is_empty())
                .ok_or_else(|| format!("rules[{idx}] requires a non-empty 'actions' array"))?;

            let mut actions = Vec::with_capacity(raw_actions.len());
            for (aidx, raw_action) in raw_actions.iter().enumerate() {
                let aobj = raw_action
                    .as_object()
                    .ok_or_else(|| format!("rules[{idx}].actions[{aidx}] must be an object"))?;

                let mut entry = ActionEntry {
                    weight: 1,
                    set_headers: Vec::new(),
                    set_labels: Vec::new(),
                };
                for (name, value) in aobj {
                    match name.as_str() {
                        "weight" => {
                            entry.weight = value.as_u64().filter(|w| *w >= 1).ok_or_else(|| {
                                format!(
                                    "rules[{idx}].actions[{aidx}].weight must be an integer >= 1"
                                )
                            })?;
                        }
                        "set_headers" => {
                            entry.set_headers = parse_kv_object(
                                value,
                                &format!("rules[{idx}].actions[{aidx}].set_headers"),
                            )?
                            .into_iter()
                            .map(|(k, v)| (k.to_lowercase(), v))
                            .collect();
                        }
                        "set_labels" => {
                            entry.set_labels = parse_kv_object(
                                value,
                                &format!("rules[{idx}].actions[{aidx}].set_labels"),
                            )?;
                        }
                        other => {
                            return Err(format!(
                                "rules[{idx}].actions[{aidx}]: not supported action: {other}"
                            ));
                        }
                    }
                }
                actions.push(entry);
            }

            let total_weight = actions.iter().map(|a| a.weight).sum();
            rules.push(Rule {
                matcher,
                actions,
                total_weight,
                cursor: AtomicU64::new(0),
            });
        }

        Ok(Self { rules })
    }
}

impl Rule {
    /// Picks an action by weighted round-robin: a shared cursor modulo the
    /// total weight walks the cumulative weight buckets, so an action with
    /// weight 3 is chosen 3 out of every `total_weight` calls.
    fn pick_action(&self) -> &ActionEntry {
        let slot = self.cursor.fetch_add(1, Ordering::Relaxed) % self.total_weight;
        let mut acc = 0;
        for action in &self.actions {
            acc += action.weight;
            if slot < acc {
                return action;
            }
        }
        // Unreachable: slot < total_weight == sum(weights).
        self.actions.last().expect("actions is non-empty")
    }
}

#[async_trait]
impl Plugin for TrafficLabelPlugin {
    fn plugin_type(&self) -> &str {
        "traffic-label"
    }

    async fn execute(
        &self,
        mut ctx: Context,
        _named_inputs: &HashMap<String, serde_json::Value>,
    ) -> PluginResult {
        for rule in &self.rules {
            let matched = rule.matcher.as_ref().is_none_or(|e| e.eval(&ctx));
            if !matched {
                continue;
            }

            let action = rule.pick_action();
            if action.set_headers.is_empty() && action.set_labels.is_empty() {
                // Weight-only entry: nothing to apply — fall through to the
                // next rule (mirrors the APISIX plugin's behavior).
                continue;
            }

            let headers: Vec<(String, String)> = action
                .set_headers
                .iter()
                .map(|(name, tmpl)| (name.clone(), interpolate(&ctx, tmpl)))
                .collect();
            let labels: Vec<(String, String)> = action
                .set_labels
                .iter()
                .map(|(key, tmpl)| (key.clone(), interpolate(&ctx, tmpl)))
                .collect();

            for (name, value) in headers {
                ctx.request.headers.insert(name, vec![value]);
            }
            for (key, value) in labels {
                ctx.message
                    .insert(format!("label.{key}"), serde_json::Value::String(value));
            }
            break;
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
    use bytes::Bytes;

    fn test_ctx(channel: Option<&str>) -> Context {
        let mut query = HashMap::new();
        if let Some(c) = channel {
            query.insert("channel".to_string(), vec![c.to_string()]);
        }
        Context {
            request: GatewayRequest {
                method: "GET".to_string(),
                path: "/api".to_string(),
                host: "example.com".to_string(),
                scheme: "http".to_string(),
                headers: HashMap::new(),
                query_params: query,
                body: Bytes::new(),
                remote_addr: "10.1.2.3:44321".to_string(),
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

    fn plugin(config: serde_json::Value) -> Result<TrafficLabelPlugin, String> {
        let map: HashMap<String, serde_json::Value> = serde_json::from_value(config).unwrap();
        TrafficLabelPlugin::from_config(&map)
    }

    #[tokio::test]
    async fn test_match_sets_headers_with_interpolation() {
        let p = plugin(serde_json::json!({
            "rules": [{
                "match": [["arg_channel", "==", "beta"]],
                "actions": [{
                    "set_headers": { "X-Server-Id": "beta", "x-origin": "$remote_addr" }
                }]
            }]
        }))
        .unwrap();

        let out = p
            .execute(test_ctx(Some("beta")), &HashMap::new())
            .await
            .unwrap();
        let ctx = out.context;
        assert_eq!(
            ctx.request.headers.get("x-server-id"),
            Some(&vec!["beta".to_string()])
        );
        assert_eq!(
            ctx.request.headers.get("x-origin"),
            Some(&vec!["10.1.2.3".to_string()])
        );

        // Non-matching request passes through untouched.
        let out = p
            .execute(test_ctx(Some("stable")), &HashMap::new())
            .await
            .unwrap();
        assert!(out.context.request.headers.is_empty());
    }

    #[tokio::test]
    async fn test_set_labels_writes_message_keys() {
        let p = plugin(serde_json::json!({
            "rules": [{
                "actions": [{ "set_labels": { "tier": "beta", "path": "$uri" } }]
            }]
        }))
        .unwrap();
        let out = p.execute(test_ctx(None), &HashMap::new()).await.unwrap();
        assert_eq!(
            out.context.message.get("label.tier"),
            Some(&serde_json::json!("beta"))
        );
        assert_eq!(
            out.context.message.get("label.path"),
            Some(&serde_json::json!("/api"))
        );
    }

    #[tokio::test]
    async fn test_weighted_round_robin_between_actions() {
        let p = plugin(serde_json::json!({
            "rules": [{
                "actions": [
                    { "set_headers": { "x-variant": "a" }, "weight": 3 },
                    { "set_headers": { "x-variant": "b" }, "weight": 1 }
                ]
            }]
        }))
        .unwrap();

        let mut counts: HashMap<String, u32> = HashMap::new();
        for _ in 0..8 {
            let out = p.execute(test_ctx(None), &HashMap::new()).await.unwrap();
            let v = out.context.request.headers.get("x-variant").unwrap()[0].clone();
            *counts.entry(v).or_insert(0) += 1;
        }
        assert_eq!(counts.get("a"), Some(&6));
        assert_eq!(counts.get("b"), Some(&2));
    }

    #[tokio::test]
    async fn test_weight_only_action_falls_through_to_next_rule() {
        let p = plugin(serde_json::json!({
            "rules": [
                { "actions": [{ "weight": 1 }] },
                { "actions": [{ "set_headers": { "x-fallback": "yes" } }] }
            ]
        }))
        .unwrap();
        let out = p.execute(test_ctx(None), &HashMap::new()).await.unwrap();
        assert_eq!(
            out.context.request.headers.get("x-fallback"),
            Some(&vec!["yes".to_string()])
        );
    }

    #[tokio::test]
    async fn test_first_matching_rule_wins() {
        let p = plugin(serde_json::json!({
            "rules": [
                { "actions": [{ "set_labels": { "rule": "first" } }] },
                { "actions": [{ "set_labels": { "rule": "second" } }] }
            ]
        }))
        .unwrap();
        let out = p.execute(test_ctx(None), &HashMap::new()).await.unwrap();
        assert_eq!(
            out.context.message.get("label.rule"),
            Some(&serde_json::json!("first"))
        );
    }

    #[test]
    fn test_config_errors() {
        // No rules.
        assert!(plugin(serde_json::json!({})).is_err());
        assert!(plugin(serde_json::json!({ "rules": [] })).is_err());
        // Missing actions.
        assert!(plugin(serde_json::json!({ "rules": [{}] })).is_err());
        // Unsupported action key.
        assert!(plugin(serde_json::json!({
            "rules": [{ "actions": [{ "redirect": { "uri": "/x" } }] }]
        }))
        .is_err());
        // Invalid match expression surfaces at load.
        assert!(plugin(serde_json::json!({
            "rules": [{
                "match": [["uri", "bogus", "/x"]],
                "actions": [{ "set_headers": { "x": "y" } }]
            }]
        }))
        .is_err());
        // Invalid weight.
        assert!(plugin(serde_json::json!({
            "rules": [{ "actions": [{ "set_headers": { "x": "y" }, "weight": 0 }] }]
        }))
        .is_err());
    }
}
