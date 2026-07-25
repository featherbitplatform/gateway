//! Traffic-split plugin (`traffic-split`) — a port of Apache APISIX's
//! `traffic-split` plugin for weighted / conditional traffic steering
//! (canary, blue-green, A/B).
//!
//! A request is matched against an ordered list of `rules`; the **first** rule
//! whose `match` condition passes selects a set of `weighted_upstreams`. One
//! weighted slot is then picked by weighted round-robin. A slot either names a
//! concrete target set (`upstream.targets`) or carries only a `weight` (the
//! "default" slot, meaning "use the route's normal upstream").
//!
//! **Split-node wiring** (featherbit design — read this before wiring the
//! graph). This node sits **before** the route's normal `upstream` node. It
//! exposes the usual `success` / `error` ports and the caller wires them so
//! that the two outcomes reach the right place:
//!
//! - **Default slot picked (or no rule matched):** the plugin returns `Ok(ctx)`
//!   unchanged. Wire `success` → the rest of the pipeline (the normal
//!   `upstream` node). The request is proxied by the route as usual.
//! - **Target slot picked:** the plugin proxies the request itself to the
//!   chosen target (reusing the shared outbound client), writes the backend's
//!   reply onto `Context.response`, and **short-circuits** by returning
//!   `Err(code TRAFFIC_SPLIT_ROUTED)` with the response already populated. Wire
//!   `error` → `client.in` (the same convention `fault-injection` and
//!   `mocking` use for "stop here, send this response"). If the split target
//!   itself is unreachable the node fails with `TRAFFIC_SPLIT_UPSTREAM_ERROR`
//!   and a prepared `502` body — also routed through `error`.
//!
//! This makes canary/blue-green trivial: give the default slot weight 90 and a
//! canary target set weight 10, and 10% of matching traffic is proxied to the
//! canary while the other 90% falls through to the normal upstream.

use async_trait::async_trait;
use bytes::Bytes;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crate::context::{Context, GatewayError};
use crate::outbound::{OutboundClient, OutboundError, OutboundRequest};
use crate::plugins::resources::PluginResources;
use crate::plugins::{Plugin, PluginExecutionError, PluginOutput, PluginResult};
use crate::vars::Expr;

/// Steers matching requests to a weighted set of upstream targets, or lets them
/// fall through to the route's normal upstream.
pub struct TrafficSplitPlugin {
    rules: Vec<Rule>,
    /// Whole-call deadline per request the plugin proxies itself.
    timeout: Duration,
    /// Shared pooled HTTP client (from `PluginResources`).
    client: Arc<OutboundClient>,
}

struct Rule {
    /// One APISIX triple-array expression (rules AND-ed); `None` matches all.
    matcher: Option<Expr>,
    slots: Vec<Slot>,
    /// Sum of slot weights (always > 0 — enforced at load).
    total_weight: u64,
    /// Round-robin cursor for weighted selection between `slots`.
    cursor: AtomicU64,
}

struct Slot {
    weight: u64,
    /// `Some` → proxy to one of these targets; `None` → the "default" slot,
    /// meaning fall through to the route's normal upstream.
    targets: Option<Vec<Target>>,
    /// Round-robin cursor within the target set (unused for the default slot).
    target_cursor: AtomicUsize,
}

/// A single backend address (`host:port`) a split slot forwards to.
#[derive(Debug, Clone)]
struct Target {
    host: String,
    port: u16,
}

/// Parses a `{targets: [{host, port}], ...}` upstream object into a target
/// list. Entries missing `host`/`port` are skipped; an empty result is an
/// error (an `upstream` block must name at least one reachable target).
fn parse_targets(v: &serde_json::Value, field: &str) -> Result<Vec<Target>, String> {
    let obj = v
        .as_object()
        .ok_or_else(|| format!("{field} must be an object"))?;
    let targets = obj
        .get("targets")
        .or_else(|| obj.get("nodes"))
        .and_then(|v| v.as_array())
        .map(|seq| {
            seq.iter()
                .filter_map(|t| {
                    let m = t.as_object()?;
                    let host = m.get("host")?.as_str()?.to_string();
                    let port = m.get("port")?.as_u64()? as u16;
                    Some(Target { host, port })
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    if targets.is_empty() {
        return Err(format!(
            "{field}.targets must contain at least one {{host, port}} entry"
        ));
    }
    Ok(targets)
}

impl TrafficSplitPlugin {
    /// Builds the plugin from node config. All `match` expressions are compiled
    /// here at config load (fail-fast), weights must be non-negative integers,
    /// and every rule needs at least one positively-weighted slot.
    ///
    /// Accepted keys:
    /// - `rules` (array, **required**, non-empty): evaluated in order; the
    ///   first rule whose `match` passes is used.
    ///   - `match` (array, optional): an APISIX triple-array condition (rules
    ///     AND-ed, see [`crate::vars`]). Omit to match every request.
    ///   - `weighted_upstreams` (array, **required**, non-empty): the weighted
    ///     slots one of which is chosen by weighted round-robin. Each slot:
    ///     - `upstream` (object, optional): `{targets: [{host, port}], ...}`.
    ///       When present, this slot proxies matching requests to one of the
    ///       targets (round-robin within the set). When **absent**, the slot is
    ///       the "default" — matching requests fall through to the route's
    ///       normal upstream.
    ///     - `weight` (integer >= 0, default `1`): selection weight.
    /// - `timeout_ms` (integer, default `60000`): whole-call deadline for
    ///   requests the plugin proxies itself.
    ///
    /// ```yaml
    /// type: traffic-split
    /// config:
    ///   timeout_ms: 60000
    ///   rules:
    ///     - match:
    ///         - ["arg_canary", "==", "1"]
    ///       weighted_upstreams:
    ///         # 90% fall through to the route's normal upstream
    ///         - weight: 90
    ///         # 10% proxied to the canary target set
    ///         - upstream:
    ///             targets:
    ///               - host: canary-backend
    ///                 port: 8080
    ///           weight: 10
    /// ```
    pub fn from_config(
        config: &HashMap<String, serde_json::Value>,
        resources: &Arc<PluginResources>,
    ) -> Result<Self, String> {
        let raw_rules = config
            .get("rules")
            .and_then(|v| v.as_array())
            .filter(|r| !r.is_empty())
            .ok_or("traffic-split requires a non-empty 'rules' array")?;

        let mut rules = Vec::with_capacity(raw_rules.len());
        for (idx, raw) in raw_rules.iter().enumerate() {
            let obj = raw
                .as_object()
                .ok_or_else(|| format!("rules[{idx}] must be an object"))?;

            let matcher = match obj.get("match") {
                None | Some(serde_json::Value::Null) => None,
                Some(v) => Some(Expr::parse(v).map_err(|e| format!("rules[{idx}].match: {e}"))?),
            };

            let raw_slots = obj
                .get("weighted_upstreams")
                .and_then(|v| v.as_array())
                .filter(|s| !s.is_empty())
                .ok_or_else(|| {
                    format!("rules[{idx}] requires a non-empty 'weighted_upstreams' array")
                })?;

            let mut slots = Vec::with_capacity(raw_slots.len());
            for (sidx, raw_slot) in raw_slots.iter().enumerate() {
                let sobj = raw_slot.as_object().ok_or_else(|| {
                    format!("rules[{idx}].weighted_upstreams[{sidx}] must be an object")
                })?;

                let weight = match sobj.get("weight") {
                    None => 1,
                    Some(v) => v.as_u64().ok_or_else(|| {
                        format!(
                            "rules[{idx}].weighted_upstreams[{sidx}].weight must be a non-negative integer"
                        )
                    })?,
                };

                let targets = match sobj.get("upstream") {
                    None | Some(serde_json::Value::Null) => None,
                    Some(v) => Some(parse_targets(
                        v,
                        &format!("rules[{idx}].weighted_upstreams[{sidx}].upstream"),
                    )?),
                };

                slots.push(Slot {
                    weight,
                    targets,
                    target_cursor: AtomicUsize::new(0),
                });
            }

            let total_weight: u64 = slots.iter().map(|s| s.weight).sum();
            if total_weight == 0 {
                return Err(format!(
                    "rules[{idx}] needs at least one weighted_upstream with weight > 0"
                ));
            }

            rules.push(Rule {
                matcher,
                slots,
                total_weight,
                cursor: AtomicU64::new(0),
            });
        }

        let timeout = Duration::from_millis(
            config
                .get("timeout_ms")
                .and_then(|v| v.as_u64())
                .unwrap_or(60_000),
        );

        Ok(Self {
            rules,
            timeout,
            client: resources.outbound.clone(),
        })
    }

    /// Returns the first rule whose `match` passes (or that has no `match`),
    /// or `None` when no rule matches (→ passthrough to the normal upstream).
    fn select_rule(&self, ctx: &Context) -> Option<&Rule> {
        self.rules
            .iter()
            .find(|rule| rule.matcher.as_ref().is_none_or(|e| e.eval(ctx)))
    }

    /// Proxies the request to `target`, writing the backend reply onto
    /// `ctx.response`. Mirrors the `upstream` node: overrides the `Host` header
    /// with the target and forwards method/headers/body unchanged. On success
    /// returns `Ok(())`; on transport/timeout failure returns the gateway
    /// error code + message so the caller can prepare a 502.
    async fn proxy_to_target(
        &self,
        ctx: &mut Context,
        target: &Target,
    ) -> Result<(), (&'static str, String)> {
        let url = format!("http://{}:{}{}", target.host, target.port, ctx.request.path);
        let method: http::Method = ctx.request.method.parse().unwrap_or(http::Method::GET);

        let mut headers: Vec<(String, String)> = Vec::new();
        for (key, values) in &ctx.request.headers {
            if key.eq_ignore_ascii_case("host") {
                continue;
            }
            for value in values {
                headers.push((key.clone(), value.clone()));
            }
        }
        headers.push((
            "host".to_string(),
            format!("{}:{}", target.host, target.port),
        ));

        let outbound = OutboundRequest {
            method,
            url,
            headers,
            body: ctx.request.body.clone(),
            timeout: self.timeout,
            ssl_verify: true,
        };

        match self.client.request(outbound).await {
            Ok(resp) => {
                ctx.response.status_code = resp.status;
                ctx.response.headers = resp.headers;
                ctx.response.body = resp.body;
                Ok(())
            }
            Err(e) => {
                let (code, message) = match &e {
                    OutboundError::Timeout(d) => (
                        "TRAFFIC_SPLIT_UPSTREAM_ERROR",
                        format!(
                            "traffic-split target {}:{} timed out after {:?}",
                            target.host, target.port, d
                        ),
                    ),
                    OutboundError::InvalidRequest(m) => (
                        "TRAFFIC_SPLIT_UPSTREAM_ERROR",
                        format!("traffic-split failed to build request: {m}"),
                    ),
                    OutboundError::Transport(m) => (
                        "TRAFFIC_SPLIT_UPSTREAM_ERROR",
                        format!(
                            "traffic-split failed to reach target {}:{}: {m}",
                            target.host, target.port
                        ),
                    ),
                };
                Err((code, message))
            }
        }
    }
}

impl Rule {
    /// Picks a slot by weighted round-robin: a shared cursor modulo the rule's
    /// total weight walks the cumulative weight buckets, so a slot with weight
    /// 3 is chosen 3 out of every `total_weight` calls. Zero-weight slots are
    /// never selected.
    fn pick_slot(&self) -> &Slot {
        let mark = self.cursor.fetch_add(1, Ordering::Relaxed) % self.total_weight;
        let mut acc = 0;
        for slot in &self.slots {
            acc += slot.weight;
            if mark < acc {
                return slot;
            }
        }
        // Unreachable: mark < total_weight == sum(weights) and total_weight > 0.
        self.slots.last().expect("slots is non-empty")
    }
}

impl Slot {
    /// Round-robins across the slot's target set. Only valid for target slots
    /// (`targets` is `Some`).
    fn pick_target<'a>(&self, targets: &'a [Target]) -> &'a Target {
        let idx = self.target_cursor.fetch_add(1, Ordering::Relaxed) % targets.len();
        &targets[idx]
    }
}

#[async_trait]
impl Plugin for TrafficSplitPlugin {
    fn plugin_type(&self) -> &str {
        "traffic-split"
    }

    async fn execute(
        &self,
        mut ctx: Context,
        _named_inputs: &HashMap<String, serde_json::Value>,
    ) -> PluginResult {
        // No rule matched → fall through to the route's normal upstream.
        let rule = match self.select_rule(&ctx) {
            Some(rule) => rule,
            None => {
                return Ok(PluginOutput {
                    context: ctx,
                    named_outputs: HashMap::new(),
                })
            }
        };

        let slot = rule.pick_slot();
        let targets = match &slot.targets {
            // Default slot → fall through to the normal upstream (success port).
            None => {
                return Ok(PluginOutput {
                    context: ctx,
                    named_outputs: HashMap::new(),
                })
            }
            Some(targets) => targets,
        };

        // Target slot → proxy the request ourselves and short-circuit.
        let target = slot.pick_target(targets).clone();
        match self.proxy_to_target(&mut ctx, &target).await {
            Ok(()) => Err(PluginExecutionError {
                context: ctx,
                error: GatewayError {
                    node_id: String::new(),
                    code: "TRAFFIC_SPLIT_ROUTED".to_string(),
                    message: format!(
                        "traffic-split proxied request to {}:{}",
                        target.host, target.port
                    ),
                    metadata: HashMap::new(),
                },
            }),
            Err((code, message)) => {
                // Prepare a 502 so the error edge (→ client.in) has a body.
                ctx.response.status_code = 502;
                ctx.response.body = Bytes::from(
                    r#"{"error": "bad_gateway", "message": "traffic-split target unreachable"}"#,
                );
                ctx.response.headers.insert(
                    "content-type".to_string(),
                    vec!["application/json".to_string()],
                );
                Err(PluginExecutionError {
                    context: ctx,
                    error: GatewayError {
                        node_id: String::new(),
                        code: code.to_string(),
                        message,
                        metadata: HashMap::new(),
                    },
                })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::{GatewayRequest, GatewayResponse, Protocol};

    fn test_ctx(canary: Option<&str>) -> Context {
        let mut query = HashMap::new();
        if let Some(c) = canary {
            query.insert("canary".to_string(), vec![c.to_string()]);
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

    fn plugin(config: serde_json::Value) -> Result<TrafficSplitPlugin, String> {
        let map: HashMap<String, serde_json::Value> = serde_json::from_value(config).unwrap();
        TrafficSplitPlugin::from_config(&map, &PluginResources::empty())
    }

    #[test]
    fn test_select_rule_matches_first_passing() {
        let p = plugin(serde_json::json!({
            "rules": [
                {
                    "match": [["arg_canary", "==", "1"]],
                    "weighted_upstreams": [{ "weight": 1 }]
                },
                {
                    "weighted_upstreams": [{ "weight": 1 }]
                }
            ]
        }))
        .unwrap();

        // canary=1 → first rule selected.
        let ctx = test_ctx(Some("1"));
        assert!(std::ptr::eq(p.select_rule(&ctx).unwrap(), &p.rules[0]));
        // canary absent → first rule's match fails, fall to the catch-all rule.
        let ctx = test_ctx(None);
        assert!(std::ptr::eq(p.select_rule(&ctx).unwrap(), &p.rules[1]));
    }

    #[test]
    fn test_no_rule_matches_returns_none() {
        let p = plugin(serde_json::json!({
            "rules": [{
                "match": [["arg_canary", "==", "1"]],
                "weighted_upstreams": [{ "weight": 1 }]
            }]
        }))
        .unwrap();
        assert!(p.select_rule(&test_ctx(Some("0"))).is_none());
    }

    #[test]
    fn test_weighted_round_robin_distribution() {
        // 3:1 split between a default slot and a target slot over a fixed
        // cursor: 8 draws → 6 default, 2 target, deterministically.
        let p = plugin(serde_json::json!({
            "rules": [{
                "weighted_upstreams": [
                    { "weight": 3 },
                    { "upstream": { "targets": [{ "host": "canary", "port": 80 }] }, "weight": 1 }
                ]
            }]
        }))
        .unwrap();
        let rule = &p.rules[0];

        let mut default_hits = 0;
        let mut target_hits = 0;
        for _ in 0..8 {
            match &rule.pick_slot().targets {
                None => default_hits += 1,
                Some(_) => target_hits += 1,
            }
        }
        assert_eq!(default_hits, 6);
        assert_eq!(target_hits, 2);
    }

    #[test]
    fn test_pick_target_round_robins_within_set() {
        let p = plugin(serde_json::json!({
            "rules": [{
                "weighted_upstreams": [{
                    "upstream": { "targets": [
                        { "host": "a", "port": 80 },
                        { "host": "b", "port": 80 }
                    ] },
                    "weight": 1
                }]
            }]
        }))
        .unwrap();
        let slot = &p.rules[0].slots[0];
        let targets = slot.targets.as_ref().unwrap();
        let picks: Vec<&str> = (0..4)
            .map(|_| slot.pick_target(targets).host.as_str())
            .collect();
        assert_eq!(picks, vec!["a", "b", "a", "b"]);
    }

    #[tokio::test]
    async fn test_default_slot_returns_ok_passthrough() {
        // A rule with only a default (no-upstream) slot passes the request
        // through untouched — no network call.
        let p = plugin(serde_json::json!({
            "rules": [{ "weighted_upstreams": [{ "weight": 1 }] }]
        }))
        .unwrap();
        let out = p.execute(test_ctx(None), &HashMap::new()).await.unwrap();
        assert_eq!(out.context.response.status_code, 0);
    }

    #[tokio::test]
    async fn test_no_match_returns_ok_passthrough() {
        let p = plugin(serde_json::json!({
            "rules": [{
                "match": [["arg_canary", "==", "yes"]],
                "weighted_upstreams": [{
                    "upstream": { "targets": [{ "host": "canary", "port": 80 }] },
                    "weight": 1
                }]
            }]
        }))
        .unwrap();
        // canary != yes → no rule matches → Ok passthrough, no proxy attempt.
        let out = p
            .execute(test_ctx(Some("no")), &HashMap::new())
            .await
            .unwrap();
        assert_eq!(out.context.response.status_code, 0);
    }

    #[test]
    fn test_target_slot_prepares_proxy_path() {
        // The selected target slot resolves to a concrete target (the input to
        // the proxy round-trip) without making any network call.
        let p = plugin(serde_json::json!({
            "rules": [{
                "weighted_upstreams": [{
                    "upstream": { "targets": [{ "host": "canary-backend", "port": 8080 }] },
                    "weight": 1
                }]
            }]
        }))
        .unwrap();
        let slot = p.rules[0].pick_slot();
        let targets = slot.targets.as_ref().expect("target slot");
        let target = slot.pick_target(targets);
        assert_eq!(target.host, "canary-backend");
        assert_eq!(target.port, 8080);
    }

    #[test]
    fn test_config_errors() {
        // No rules.
        assert!(plugin(serde_json::json!({})).is_err());
        assert!(plugin(serde_json::json!({ "rules": [] })).is_err());
        // Empty weighted_upstreams.
        assert!(plugin(serde_json::json!({
            "rules": [{ "weighted_upstreams": [] }]
        }))
        .is_err());
        // All-zero weights.
        assert!(plugin(serde_json::json!({
            "rules": [{ "weighted_upstreams": [{ "weight": 0 }] }]
        }))
        .is_err());
        // Negative weight.
        assert!(plugin(serde_json::json!({
            "rules": [{ "weighted_upstreams": [{ "weight": -1 }] }]
        }))
        .is_err());
        // upstream present but no targets.
        assert!(plugin(serde_json::json!({
            "rules": [{ "weighted_upstreams": [{ "upstream": {}, "weight": 1 }] }]
        }))
        .is_err());
        // Invalid match expression surfaces at load.
        assert!(plugin(serde_json::json!({
            "rules": [{
                "match": [["uri", "bogus", "/x"]],
                "weighted_upstreams": [{ "weight": 1 }]
            }]
        }))
        .is_err());
    }
}
