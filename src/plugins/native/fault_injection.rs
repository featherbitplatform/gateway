//! Fault injection plugin (`fault-injection`) — a faithful subset of Apache
//! APISIX's `fault-injection` plugin for chaos/resilience testing.
//!
//! Injects an artificial `delay` and/or an `abort` response into matching
//! requests. Both faults are gated independently by a `percentage` sample and
//! a `vars` condition (APISIX triple-array expressions, see [`crate::vars`]).
//!
//! **Early-exit wiring**: an aborted request has the configured response
//! already written onto `Context.response` and exits through the node's
//! **error** port with code `FAULT_INJECTED`; non-aborted requests continue
//! through the **success** port. Wire `error` to a pass-through path (e.g.
//! straight to `client.in`, or an `error-handler` that preserves the prepared
//! response) so the injected status/body reach the client, and wire `success`
//! to the rest of the pipeline. This deviates mechanically from APISIX (where
//! abort is a direct exit, not an error) because featherbit pipelines need a
//! distinct port for "stop here" versus "keep going".

use async_trait::async_trait;
use bytes::Bytes;
use std::collections::HashMap;
use std::time::Duration;

use crate::context::{Context, GatewayError};
use crate::plugins::{Plugin, PluginExecutionError, PluginOutput, PluginResult};
use crate::vars::{interpolate, Expr};

/// Injects delays and/or abort responses into matching requests.
///
/// Order matches APISIX: the delay (if it triggers) is applied first, then
/// the abort check runs. `body`, and string header values, support `$var`
/// interpolation against the request.
pub struct FaultInjectionPlugin {
    abort: Option<AbortRule>,
    delay: Option<DelayRule>,
}

struct AbortRule {
    http_status: u16,
    /// Body template (`$var` interpolated); empty body when unset.
    body: Option<String>,
    /// Lowercased header name → value template.
    headers: Vec<(String, String)>,
    percentage: Option<u8>,
    /// OR-ed list of expressions (APISIX shape); `None` means always match.
    vars: Option<Vec<Expr>>,
}

struct DelayRule {
    duration: Duration,
    percentage: Option<u8>,
    vars: Option<Vec<Expr>>,
}

/// Draws a pseudo-random number in `0..100`.
///
/// Uses a freshly seeded `RandomState` hasher instead of a rand crate: each
/// `RandomState::new()` carries new per-call keys, so `finish()` yields a
/// cheap, statistically casual value. Not cryptographic and not a
/// deterministic sequence — good enough for percentage sampling.
fn roll_percent() -> u8 {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};
    (RandomState::new().build_hasher().finish() % 100) as u8
}

/// APISIX `sample_hit`: no percentage means always hit; `0` never hits and
/// `100` always hits.
fn sample_hit(percentage: Option<u8>) -> bool {
    match percentage {
        None => true,
        Some(p) => roll_percent() < p,
    }
}

/// True when `vars` is unset, or when any expression in the list matches
/// (APISIX ORs across the `vars` items and ANDs within each).
fn vars_match(vars: &Option<Vec<Expr>>, ctx: &Context) -> bool {
    match vars {
        None => true,
        Some(exprs) => exprs.iter().any(|e| e.eval(ctx)),
    }
}

/// Parses the APISIX `vars` shape for this plugin: an array whose items are
/// each a full expression (OR-ed together). Two leniencies over the strict
/// shape, both resolved at config load:
/// - an item that is a `["AND"|"OR", rule...]` group is wrapped as a
///   single-rule expression;
/// - a "flat" list of bare rules (`[["arg_x", "==", "1"], ...]`) is treated
///   as one AND-ed expression.
fn parse_or_vars(v: &serde_json::Value, field: &str) -> Result<Vec<Expr>, String> {
    let items = v
        .as_array()
        .ok_or_else(|| format!("{field}.vars must be an array"))?;

    // Flat shape: first item's first element is a plain string that is not a
    // logical operator — treat the whole array as a single expression.
    let first_str = items
        .first()
        .and_then(|i| i.as_array())
        .and_then(|a| a.first())
        .and_then(|f| f.as_str());
    if let Some(s) = first_str {
        if !s.eq_ignore_ascii_case("and") && !s.eq_ignore_ascii_case("or") {
            let expr = Expr::parse(v).map_err(|e| format!("{field}.vars: {e}"))?;
            return Ok(vec![expr]);
        }
    }

    items
        .iter()
        .map(|item| {
            let starts_with_op = item
                .as_array()
                .and_then(|a| a.first())
                .and_then(|f| f.as_str())
                .is_some();
            let parsed = if starts_with_op {
                // ["AND"/"OR", ...] is one nested rule: wrap it into a rule list.
                Expr::parse(&serde_json::Value::Array(vec![item.clone()]))
            } else {
                Expr::parse(item)
            };
            parsed.map_err(|e| format!("{field}.vars: {e}"))
        })
        .collect()
}

/// Parses `percentage` as an integer in `0..=100`.
fn parse_percentage(
    obj: &serde_json::Map<String, serde_json::Value>,
    field: &str,
) -> Result<Option<u8>, String> {
    match obj.get("percentage") {
        None => Ok(None),
        Some(v) => {
            let n = v
                .as_u64()
                .filter(|n| *n <= 100)
                .ok_or_else(|| format!("{field}.percentage must be an integer 0-100"))?;
            Ok(Some(n as u8))
        }
    }
}

/// Accepts `headers` as a map (`{name: value}`) or as the UI editor's array
/// form (`[{name, value}]`); scalar values are stringified, other shapes are
/// rejected. Header names are lowercased.
fn parse_headers(v: &serde_json::Value, field: &str) -> Result<Vec<(String, String)>, String> {
    let scalar = |v: &serde_json::Value| -> Option<String> {
        match v {
            serde_json::Value::String(s) => Some(s.clone()),
            serde_json::Value::Number(n) => Some(n.to_string()),
            serde_json::Value::Bool(b) => Some(b.to_string()),
            _ => None,
        }
    };
    match v {
        serde_json::Value::Object(m) => m
            .iter()
            .map(|(k, v)| {
                scalar(v)
                    .map(|s| (k.to_lowercase(), s))
                    .ok_or_else(|| format!("{field}['{k}'] must be a scalar value"))
            })
            .collect(),
        serde_json::Value::Array(items) => {
            let mut out = Vec::new();
            for item in items {
                let obj = item.as_object().ok_or_else(|| {
                    format!("{field} entries must be objects with 'name' and 'value'")
                })?;
                let name = obj.get("name").and_then(|v| v.as_str()).unwrap_or("");
                if name.trim().is_empty() {
                    continue; // blank row left in the UI editor
                }
                let value = match obj.get("value") {
                    None => String::new(),
                    Some(v) => scalar(v)
                        .ok_or_else(|| format!("{field}['{name}'] must be a scalar value"))?,
                };
                out.push((name.to_lowercase(), value));
            }
            Ok(out)
        }
        _ => Err(format!(
            "{field} must be a map of name: value or a list of {{name, value}} objects"
        )),
    }
}

impl FaultInjectionPlugin {
    /// Builds the plugin from node config. At least one of `abort` / `delay`
    /// is required; expressions and shapes are validated here at config load.
    ///
    /// Accepted keys:
    /// - `abort` (object): injected response.
    ///   - `http_status` (integer >= 200, **required**): response status.
    ///   - `body` (string): response body; supports `$var` interpolation.
    ///     Empty body when unset.
    ///   - `headers` (map `{name: value}` or array `[{name, value}]`):
    ///     response headers; string values support `$var` interpolation.
    ///   - `percentage` (integer 0-100): chance the abort triggers; unset
    ///     means always.
    ///   - `vars` (array): APISIX condition expressions, OR-ed across items.
    /// - `delay` (object): injected latency.
    ///   - `duration` (number, seconds, may be fractional, **required**).
    ///   - `percentage`, `vars`: same gating as for `abort`.
    ///
    /// ```yaml
    /// type: fault-injection
    /// config:
    ///   delay:
    ///     duration: 0.5
    ///     percentage: 30
    ///   abort:
    ///     http_status: 503
    ///     body: '{"error": "injected for $uri"}'
    ///     percentage: 10
    ///     vars:
    ///       - [["arg_debug", "==", "1"]]
    /// ```
    pub fn from_config(config: &HashMap<String, serde_json::Value>) -> Result<Self, String> {
        let abort = match config.get("abort") {
            None => None,
            Some(v) => {
                let obj = v.as_object().ok_or("abort must be an object")?;
                let http_status = obj
                    .get("http_status")
                    .and_then(|v| v.as_u64())
                    .filter(|n| (200..=599).contains(n))
                    .ok_or("abort.http_status is required and must be an integer >= 200")?
                    as u16;
                let body = match obj.get("body") {
                    None => None,
                    Some(v) => Some(v.as_str().ok_or("abort.body must be a string")?.to_string()),
                };
                let headers = match obj.get("headers") {
                    None => Vec::new(),
                    Some(v) => parse_headers(v, "abort.headers")?,
                };
                let percentage = parse_percentage(obj, "abort")?;
                let vars = match obj.get("vars") {
                    None => None,
                    Some(v) => Some(parse_or_vars(v, "abort")?),
                };
                Some(AbortRule {
                    http_status,
                    body,
                    headers,
                    percentage,
                    vars,
                })
            }
        };

        let delay = match config.get("delay") {
            None => None,
            Some(v) => {
                let obj = v.as_object().ok_or("delay must be an object")?;
                let duration = obj
                    .get("duration")
                    .and_then(|v| v.as_f64())
                    .filter(|d| *d >= 0.0 && d.is_finite())
                    .ok_or(
                        "delay.duration is required and must be a non-negative number of seconds",
                    )?;
                let percentage = parse_percentage(obj, "delay")?;
                let vars = match obj.get("vars") {
                    None => None,
                    Some(v) => Some(parse_or_vars(v, "delay")?),
                };
                Some(DelayRule {
                    duration: Duration::from_secs_f64(duration),
                    percentage,
                    vars,
                })
            }
        };

        if abort.is_none() && delay.is_none() {
            return Err("fault-injection requires at least one of 'abort' or 'delay'".to_string());
        }

        Ok(Self { abort, delay })
    }
}

#[async_trait]
impl Plugin for FaultInjectionPlugin {
    fn plugin_type(&self) -> &str {
        "fault-injection"
    }

    async fn execute(
        &self,
        mut ctx: Context,
        _named_inputs: &HashMap<String, serde_json::Value>,
    ) -> PluginResult {
        if let Some(delay) = &self.delay {
            if sample_hit(delay.percentage) && vars_match(&delay.vars, &ctx) {
                tokio::time::sleep(delay.duration).await;
            }
        }

        if let Some(abort) = &self.abort {
            if sample_hit(abort.percentage) && vars_match(&abort.vars, &ctx) {
                // Interpolate against the request state before mutating the response.
                let body = abort
                    .body
                    .as_ref()
                    .map(|b| interpolate(&ctx, b))
                    .unwrap_or_default();
                let headers: Vec<(String, String)> = abort
                    .headers
                    .iter()
                    .map(|(name, tmpl)| (name.clone(), interpolate(&ctx, tmpl)))
                    .collect();

                ctx.response.status_code = abort.http_status;
                ctx.response.body = Bytes::from(body);
                for (name, value) in headers {
                    ctx.response.headers.insert(name, vec![value]);
                }

                return Err(PluginExecutionError {
                    context: ctx,
                    error: GatewayError {
                        node_id: String::new(),
                        code: "FAULT_INJECTED".to_string(),
                        message: format!("fault injected: abort with status {}", abort.http_status),
                        metadata: HashMap::new(),
                    },
                });
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
    use std::time::Instant;

    fn test_ctx() -> Context {
        let mut query = HashMap::new();
        query.insert("name".to_string(), vec!["jack".to_string()]);
        Context {
            request: GatewayRequest {
                method: "GET".to_string(),
                path: "/api/users".to_string(),
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

    fn plugin(config: serde_json::Value) -> Result<FaultInjectionPlugin, String> {
        let map: HashMap<String, serde_json::Value> = serde_json::from_value(config).unwrap();
        FaultInjectionPlugin::from_config(&map)
    }

    #[tokio::test]
    async fn test_abort_prepares_response_and_errors() {
        let p = plugin(serde_json::json!({
            "abort": {
                "http_status": 503,
                "body": "injected for $uri",
                "headers": { "X-Fault": "yes" }
            }
        }))
        .unwrap();

        let err = p.execute(test_ctx(), &HashMap::new()).await.unwrap_err();
        assert_eq!(err.error.code, "FAULT_INJECTED");
        let ctx = err.context;
        assert_eq!(ctx.response.status_code, 503);
        assert_eq!(ctx.response.body, Bytes::from("injected for /api/users"));
        assert_eq!(
            ctx.response.headers.get("x-fault"),
            Some(&vec!["yes".to_string()])
        );
    }

    #[tokio::test]
    async fn test_abort_vars_gate() {
        // OR-of-expressions shape: second expression matches.
        let p = plugin(serde_json::json!({
            "abort": {
                "http_status": 500,
                "vars": [
                    [["arg_name", "==", "nope"]],
                    [["arg_name", "==", "jack"]]
                ]
            }
        }))
        .unwrap();
        assert!(p.execute(test_ctx(), &HashMap::new()).await.is_err());

        // No expression matches -> passthrough on success.
        let p = plugin(serde_json::json!({
            "abort": {
                "http_status": 500,
                "vars": [[["arg_name", "==", "jill"]]]
            }
        }))
        .unwrap();
        let out = p.execute(test_ctx(), &HashMap::new()).await.unwrap();
        assert_eq!(out.context.response.status_code, 0);
    }

    #[tokio::test]
    async fn test_flat_vars_shape_accepted() {
        let p = plugin(serde_json::json!({
            "abort": {
                "http_status": 500,
                "vars": [["arg_name", "==", "jack"]]
            }
        }))
        .unwrap();
        assert!(p.execute(test_ctx(), &HashMap::new()).await.is_err());
    }

    #[tokio::test]
    async fn test_percentage_edges() {
        // 0% never aborts, 100% always aborts.
        let p0 = plugin(serde_json::json!({
            "abort": { "http_status": 500, "percentage": 0 }
        }))
        .unwrap();
        let p100 = plugin(serde_json::json!({
            "abort": { "http_status": 500, "percentage": 100 }
        }))
        .unwrap();
        for _ in 0..20 {
            assert!(p0.execute(test_ctx(), &HashMap::new()).await.is_ok());
            assert!(p100.execute(test_ctx(), &HashMap::new()).await.is_err());
        }
    }

    #[tokio::test]
    async fn test_delay_sleeps() {
        let p = plugin(serde_json::json!({
            "delay": { "duration": 0.05 }
        }))
        .unwrap();
        let start = Instant::now();
        let out = p.execute(test_ctx(), &HashMap::new()).await.unwrap();
        assert!(start.elapsed() >= Duration::from_millis(45));
        assert_eq!(out.context.response.status_code, 0);
    }

    #[tokio::test]
    async fn test_delay_vars_gate_skips_sleep() {
        let p = plugin(serde_json::json!({
            "delay": {
                "duration": 5.0,
                "vars": [[["arg_name", "==", "nobody"]]]
            }
        }))
        .unwrap();
        let start = Instant::now();
        p.execute(test_ctx(), &HashMap::new()).await.unwrap();
        assert!(start.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn test_config_errors() {
        // Neither abort nor delay.
        assert!(plugin(serde_json::json!({})).is_err());
        // abort without http_status.
        assert!(plugin(serde_json::json!({ "abort": { "body": "x" } })).is_err());
        // http_status below 200.
        assert!(plugin(serde_json::json!({ "abort": { "http_status": 100 } })).is_err());
        // delay without duration.
        assert!(plugin(serde_json::json!({ "delay": { "percentage": 50 } })).is_err());
        // percentage out of range.
        assert!(plugin(serde_json::json!({
            "abort": { "http_status": 500, "percentage": 101 }
        }))
        .is_err());
        // invalid vars expression surfaces at load.
        assert!(plugin(serde_json::json!({
            "abort": { "http_status": 500, "vars": [[["arg_x", "bogus_op", "1"]]] }
        }))
        .is_err());
    }
}
