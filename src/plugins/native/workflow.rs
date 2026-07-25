//! Workflow plugin (`workflow`) — a faithful subset of Apache APISIX's
//! `workflow` plugin: declarative traffic rules evaluated in order, where the
//! first matching `case` triggers its action.
//!
//! Supported actions (of APISIX's registrable set): `return` (reject with a
//! status code) and `limit-count` (fixed-window rate limiting via the shared
//! counter store).
//!
//! **Early-exit wiring**: a request rejected by a `return` action or an
//! exceeded `limit-count` has the rejection already written onto
//! `Context.response` and exits through the node's **error** port (codes
//! `WORKFLOW_REJECTED` / `RATE_LIMITED`). Wire `error` to a pass-through path
//! (straight to `client.in`, or an `error-handler` that preserves the
//! prepared response). Requests that match no rule — or that pass the
//! `limit-count` check — continue through the **success** port.

use async_trait::async_trait;
use bytes::Bytes;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crate::context::{Context, GatewayError};
use crate::plugins::resources::PluginResources;
use crate::plugins::{Plugin, PluginExecutionError, PluginOutput, PluginResult};
use crate::ratelimit::CounterStore;
use crate::vars::{interpolate, Expr};

/// Distinguishes counter namespaces between workflow node instances so two
/// nodes with identical rules don't share windows (APISIX isolates with a
/// per-conf `_vid`).
static INSTANCE_SEQ: AtomicU64 = AtomicU64::new(0);

/// Evaluates ordered rules; the first rule whose `case` matches applies its
/// action. Rules without a `case` always match. No matching rule means
/// passthrough.
pub struct WorkflowPlugin {
    rules: Vec<Rule>,
}

struct Rule {
    /// One APISIX triple-array expression (rules AND-ed); `None` matches all.
    case: Option<Expr>,
    action: Action,
}

enum Action {
    /// Reject with a fixed status and APISIX's exact body.
    Return { code: u16 },
    /// Fixed-window rate limit through the shared counter store.
    LimitCount(LimitCount),
}

struct LimitCount {
    count: u64,
    time_window: Duration,
    /// `$var` template resolved per request into the counter key.
    key_template: String,
    rejected_code: u16,
    rejected_msg: Option<String>,
    /// Namespaces this action's counters: `workflow:<instance>:<rule idx>`.
    counter_prefix: String,
    store: Arc<dyn CounterStore>,
}

impl WorkflowPlugin {
    /// Builds the plugin from node config. All expressions and action
    /// parameters are validated here at config load.
    ///
    /// Accepted keys:
    /// - `rules` (array, **required**, non-empty): evaluated in order.
    ///   - `case` (array, optional): APISIX triple-array condition, rules
    ///     AND-ed (e.g. `[["uri", "==", "/v1"], ["arg_debug", "==", "1"]]`).
    ///     Omit to match every request.
    ///   - `actions` (array, **required**): `[[name, params]]`. Only the
    ///     first action is applied (APISIX supports exactly one). Supported:
    ///     - `"return"` — params `{code}` (integer 100-599, **required**).
    ///       Rejects with that status and body
    ///       `{"error_msg":"rejected by workflow"}`.
    ///     - `"limit-count"` — params:
    ///       - `count` (integer > 0, **required**): allowed requests per window.
    ///       - `time_window` (number > 0, seconds, **required**).
    ///       - `key` (string, default `"$remote_addr"`): `$var` template
    ///         resolved per request into the counter key.
    ///       - `rejected_code` (integer 200-599, default `503`).
    ///       - `rejected_msg` (string, optional): when set, the rejection body
    ///         is `{"error_msg": "<msg>"}`; empty body otherwise.
    ///       - `policy` (string, default `"local"`): counter backend name.
    ///
    /// ```yaml
    /// type: workflow
    /// config:
    ///   rules:
    ///     - case:
    ///         - ["uri", "~~", "^/admin"]
    ///       actions:
    ///         - ["return", { "code": 403 }]
    ///     - actions:
    ///         - ["limit-count", { "count": 100, "time_window": 60, "key": "$remote_addr" }]
    /// ```
    pub fn from_config(
        config: &HashMap<String, serde_json::Value>,
        resources: &Arc<PluginResources>,
    ) -> Result<Self, String> {
        let raw_rules = config
            .get("rules")
            .and_then(|v| v.as_array())
            .ok_or("workflow requires a 'rules' array")?;
        if raw_rules.is_empty() {
            return Err("workflow 'rules' must not be empty".to_string());
        }

        let instance = INSTANCE_SEQ.fetch_add(1, Ordering::Relaxed);
        let mut rules = Vec::with_capacity(raw_rules.len());

        for (idx, raw) in raw_rules.iter().enumerate() {
            let obj = raw
                .as_object()
                .ok_or_else(|| format!("rules[{idx}] must be an object"))?;

            let case = match obj.get("case") {
                None => None,
                Some(v) => Some(Expr::parse(v).map_err(|e| format!("rules[{idx}].case: {e}"))?),
            };

            let actions = obj
                .get("actions")
                .and_then(|v| v.as_array())
                .filter(|a| !a.is_empty())
                .ok_or_else(|| format!("rules[{idx}] requires a non-empty 'actions' array"))?;

            // Only one action is supported (matching APISIX).
            let action_arr = actions[0]
                .as_array()
                .filter(|a| !a.is_empty())
                .ok_or_else(|| format!("rules[{idx}].actions[0] must be a non-empty array"))?;
            let name = action_arr[0]
                .as_str()
                .ok_or_else(|| format!("rules[{idx}]: action name must be a string"))?;
            let empty = serde_json::Map::new();
            let params = action_arr
                .get(1)
                .map(|v| {
                    v.as_object().ok_or_else(|| {
                        format!("rules[{idx}]: '{name}' action params must be an object")
                    })
                })
                .transpose()?
                .unwrap_or(&empty);

            let action = match name {
                "return" => {
                    let code = params
                        .get("code")
                        .and_then(|v| v.as_u64())
                        .filter(|c| (100..=599).contains(c))
                        .ok_or_else(|| {
                            format!("rules[{idx}]: 'return' action requires 'code' (100-599)")
                        })? as u16;
                    Action::Return { code }
                }
                "limit-count" => {
                    let count = params
                        .get("count")
                        .and_then(|v| v.as_u64())
                        .filter(|c| *c > 0)
                        .ok_or_else(|| {
                            format!("rules[{idx}]: 'limit-count' requires 'count' (integer > 0)")
                        })?;
                    let time_window = params
                        .get("time_window")
                        .and_then(|v| v.as_f64())
                        .filter(|w| *w > 0.0 && w.is_finite())
                        .ok_or_else(|| {
                            format!(
                                "rules[{idx}]: 'limit-count' requires 'time_window' (seconds > 0)"
                            )
                        })?;
                    let key_template = params
                        .get("key")
                        .map(|v| {
                            v.as_str()
                                .map(String::from)
                                .ok_or_else(|| format!("rules[{idx}]: 'key' must be a string"))
                        })
                        .transpose()?
                        .unwrap_or_else(|| "$remote_addr".to_string());
                    let rejected_code =
                        params
                            .get("rejected_code")
                            .map(|v| {
                                v.as_u64().filter(|c| (200..=599).contains(c)).ok_or_else(|| {
                                format!("rules[{idx}]: 'rejected_code' must be an integer 200-599")
                            })
                            })
                            .transpose()?
                            .unwrap_or(503) as u16;
                    let rejected_msg = params
                        .get("rejected_msg")
                        .map(|v| {
                            v.as_str().map(String::from).ok_or_else(|| {
                                format!("rules[{idx}]: 'rejected_msg' must be a string")
                            })
                        })
                        .transpose()?;
                    let policy = params
                        .get("policy")
                        .and_then(|v| v.as_str())
                        .unwrap_or("local");
                    let store = resources.counters.get(policy)?;

                    Action::LimitCount(LimitCount {
                        count,
                        time_window: Duration::from_secs_f64(time_window),
                        key_template,
                        rejected_code,
                        rejected_msg,
                        counter_prefix: format!("workflow:{instance}:{idx}"),
                        store,
                    })
                }
                other => {
                    return Err(format!(
                        "rules[{idx}]: unsupported action: {other} — supported: return, limit-count"
                    ));
                }
            };

            rules.push(Rule { case, action });
        }

        Ok(Self { rules })
    }

    /// Writes a JSON rejection onto the response and returns the error result
    /// so the graph engine routes through the error port.
    fn reject(
        mut ctx: Context,
        status: u16,
        body: Bytes,
        code: &str,
        message: &str,
    ) -> PluginResult {
        ctx.response.status_code = status;
        ctx.response.body = body;
        ctx.response.headers.insert(
            "content-type".to_string(),
            vec!["application/json".to_string()],
        );
        Err(PluginExecutionError {
            context: ctx,
            error: GatewayError {
                node_id: String::new(),
                code: code.to_string(),
                message: message.to_string(),
                metadata: HashMap::new(),
            },
        })
    }
}

/// Sets the standard `X-RateLimit-*` response headers (APISIX's
/// `show_limit_quota_header` behavior, always on).
fn set_quota_headers(ctx: &mut Context, limit: u64, remaining: u64, reset: Duration) {
    ctx.response
        .headers
        .insert("x-ratelimit-limit".to_string(), vec![limit.to_string()]);
    ctx.response.headers.insert(
        "x-ratelimit-remaining".to_string(),
        vec![remaining.to_string()],
    );
    ctx.response.headers.insert(
        "x-ratelimit-reset".to_string(),
        vec![reset.as_secs().to_string()],
    );
}

#[async_trait]
impl Plugin for WorkflowPlugin {
    fn plugin_type(&self) -> &str {
        "workflow"
    }

    async fn execute(
        &self,
        mut ctx: Context,
        _named_inputs: &HashMap<String, serde_json::Value>,
    ) -> PluginResult {
        for rule in &self.rules {
            let matched = rule.case.as_ref().is_none_or(|e| e.eval(&ctx));
            if !matched {
                continue;
            }

            // First matching case wins.
            match &rule.action {
                Action::Return { code } => {
                    return Self::reject(
                        ctx,
                        *code,
                        Bytes::from_static(br#"{"error_msg":"rejected by workflow"}"#),
                        "WORKFLOW_REJECTED",
                        "rejected by workflow",
                    );
                }
                Action::LimitCount(lc) => {
                    let key = format!(
                        "{}:{}",
                        lc.counter_prefix,
                        interpolate(&ctx, &lc.key_template)
                    );
                    let window = match lc
                        .store
                        .incr_fixed_window(&key, lc.count, lc.time_window)
                        .await
                    {
                        Ok(w) => w,
                        Err(e) => {
                            // Counter backend failure: reject rather than fail open.
                            return Self::reject(
                                ctx,
                                500,
                                Bytes::from_static(br#"{"error_msg":"rate limit backend error"}"#),
                                "RATE_LIMIT_ERROR",
                                &e.to_string(),
                            );
                        }
                    };

                    set_quota_headers(&mut ctx, window.limit, window.remaining, window.reset);

                    if window.allowed {
                        return Ok(PluginOutput {
                            context: ctx,
                            named_outputs: HashMap::new(),
                        });
                    }
                    let body = match &lc.rejected_msg {
                        Some(msg) => {
                            Bytes::from(serde_json::json!({ "error_msg": msg }).to_string())
                        }
                        None => Bytes::new(),
                    };
                    return Self::reject(
                        ctx,
                        lc.rejected_code,
                        body,
                        "RATE_LIMITED",
                        "rejected by workflow limit-count",
                    );
                }
            }
        }

        // No rule matched: passthrough.
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

    fn test_ctx(path: &str) -> Context {
        Context {
            request: GatewayRequest {
                method: "GET".to_string(),
                path: path.to_string(),
                host: "example.com".to_string(),
                scheme: "http".to_string(),
                headers: HashMap::new(),
                query_params: HashMap::new(),
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

    fn plugin(config: serde_json::Value) -> Result<WorkflowPlugin, String> {
        let map: HashMap<String, serde_json::Value> = serde_json::from_value(config).unwrap();
        WorkflowPlugin::from_config(&map, &PluginResources::empty())
    }

    #[tokio::test]
    async fn test_return_action_rejects_matching_request() {
        let p = plugin(serde_json::json!({
            "rules": [{
                "case": [["uri", "~~", "^/admin"]],
                "actions": [["return", { "code": 403 }]]
            }]
        }))
        .unwrap();

        let err = p
            .execute(test_ctx("/admin/users"), &HashMap::new())
            .await
            .unwrap_err();
        assert_eq!(err.error.code, "WORKFLOW_REJECTED");
        assert_eq!(err.context.response.status_code, 403);
        assert_eq!(
            err.context.response.body,
            Bytes::from_static(br#"{"error_msg":"rejected by workflow"}"#)
        );

        // Non-matching request passes through untouched.
        let out = p
            .execute(test_ctx("/public"), &HashMap::new())
            .await
            .unwrap();
        assert_eq!(out.context.response.status_code, 0);
    }

    #[tokio::test]
    async fn test_rule_without_case_matches_all() {
        let p = plugin(serde_json::json!({
            "rules": [{ "actions": [["return", { "code": 418 }]] }]
        }))
        .unwrap();
        let err = p
            .execute(test_ctx("/anything"), &HashMap::new())
            .await
            .unwrap_err();
        assert_eq!(err.context.response.status_code, 418);
    }

    #[tokio::test]
    async fn test_first_matching_rule_wins() {
        let p = plugin(serde_json::json!({
            "rules": [
                { "case": [["uri", "==", "/both"]], "actions": [["return", { "code": 401 }]] },
                { "actions": [["return", { "code": 403 }]] }
            ]
        }))
        .unwrap();
        let err = p
            .execute(test_ctx("/both"), &HashMap::new())
            .await
            .unwrap_err();
        assert_eq!(err.context.response.status_code, 401);
        let err = p
            .execute(test_ctx("/other"), &HashMap::new())
            .await
            .unwrap_err();
        assert_eq!(err.context.response.status_code, 403);
    }

    #[tokio::test]
    async fn test_limit_count_allows_then_rejects() {
        let p = plugin(serde_json::json!({
            "rules": [{
                "actions": [["limit-count", {
                    "count": 2,
                    "time_window": 60,
                    "rejected_msg": "over quota"
                }]]
            }]
        }))
        .unwrap();

        for i in 0..2 {
            let out = p
                .execute(test_ctx("/x"), &HashMap::new())
                .await
                .unwrap_or_else(|_| panic!("request {i} should pass"));
            assert_eq!(
                out.context.response.headers.get("x-ratelimit-limit"),
                Some(&vec!["2".to_string()])
            );
        }

        let err = p
            .execute(test_ctx("/x"), &HashMap::new())
            .await
            .unwrap_err();
        assert_eq!(err.error.code, "RATE_LIMITED");
        assert_eq!(err.context.response.status_code, 503);
        assert_eq!(
            err.context.response.body,
            Bytes::from(r#"{"error_msg":"over quota"}"#)
        );
        assert_eq!(
            err.context.response.headers.get("x-ratelimit-remaining"),
            Some(&vec!["0".to_string()])
        );
    }

    #[tokio::test]
    async fn test_limit_count_key_isolates_clients() {
        let p = plugin(serde_json::json!({
            "rules": [{
                "actions": [["limit-count", { "count": 1, "time_window": 60 }]]
            }]
        }))
        .unwrap();

        assert!(p.execute(test_ctx("/x"), &HashMap::new()).await.is_ok());
        assert!(p.execute(test_ctx("/x"), &HashMap::new()).await.is_err());

        // A different remote_addr gets its own window (default key $remote_addr).
        let mut other = test_ctx("/x");
        other.request.remote_addr = "192.168.9.9:1".to_string();
        assert!(p.execute(other, &HashMap::new()).await.is_ok());
    }

    #[test]
    fn test_config_errors() {
        // No rules.
        assert!(plugin(serde_json::json!({})).is_err());
        assert!(plugin(serde_json::json!({ "rules": [] })).is_err());
        // Unsupported action.
        assert!(plugin(serde_json::json!({
            "rules": [{ "actions": [["redirect", { "uri": "/x" }]] }]
        }))
        .is_err());
        // return without code.
        assert!(plugin(serde_json::json!({
            "rules": [{ "actions": [["return", {}]] }]
        }))
        .is_err());
        // limit-count missing count/time_window.
        assert!(plugin(serde_json::json!({
            "rules": [{ "actions": [["limit-count", { "count": 5 }]] }]
        }))
        .is_err());
        // Invalid case expression surfaces at load.
        assert!(plugin(serde_json::json!({
            "rules": [{
                "case": [["uri", "bogus", "/x"]],
                "actions": [["return", { "code": 403 }]]
            }]
        }))
        .is_err());
        // Unknown counter policy.
        assert!(plugin(serde_json::json!({
            "rules": [{
                "actions": [["limit-count", { "count": 1, "time_window": 1, "policy": "redis" }]]
            }]
        }))
        .is_err());
    }
}
