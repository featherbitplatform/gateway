//! Circuit breaker (`api-breaker`), ported from APISIX's
//! `apisix/plugins/api-breaker.lua`.
//!
//! A circuit breaker must both *decide* whether to admit a request (before the
//! upstream call) and *observe* the outcome (after it) — two moments a single
//! featherbit node cannot span. The behavior is expressed as a **pair of
//! nodes** sharing one breaker, linked by a required `id`:
//!
//! - a **check** node placed *before* `upstream`, which trips to the
//!   short-circuit response while the breaker is open, and
//! - an **observe** node placed *after* `upstream`, which feeds the response
//!   status back into the breaker so it opens and closes.
//!
//! Both nodes carry the same `id`, so [`crate::traffic::BreakerRegistry`] hands
//! them the same [`crate::traffic::BreakerState`].
//!
//! # Wiring
//!
//! ```text
//!            ┌──────────────────┐        ┌──────────┐        ┌──────────────────┐
//!  listener →│ api-breaker      │success →│ upstream │─ any ─→│ api-breaker      │→ client
//!            │  (phase=check)   │        │          │        │  (phase=observe) │
//!            └──────────────────┘        └──────────┘        └──────────────────┘
//!                    │ error                                        (records the
//!                    ▼                                          upstream status into
//!               client.in                                        the shared breaker)
//!          (break_response_code)
//! ```
//!
//! The check node's `error` port goes to `client.in`: while the breaker is open
//! the request short-circuits to the client with the configured break response.
//! The observe node passes the response through untouched and simply records
//! the status; wire it on the path(s) out of `upstream` that carry the real
//! upstream response.

use async_trait::async_trait;
use bytes::Bytes;
use std::collections::HashMap;
use std::sync::Arc;

use crate::context::{Context, GatewayError};
use crate::plugins::resources::PluginResources;
use crate::plugins::{Plugin, PluginExecutionError, PluginOutput, PluginResult};

/// Which half of the pair this node is.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Role {
    /// Runs before `upstream`: rejects while the breaker is open.
    Check,
    /// Runs after `upstream`: records the response status.
    Observe,
}

/// One node of an `api-breaker` check/observe pair.
///
/// Holds a handle to the process-wide [`crate::traffic::BreakerRegistry`]; the
/// shared breaker is resolved by the configured `id`.
pub struct ApiBreakerPlugin {
    role: Role,
    /// Shared breaker identity — links the check and observe nodes.
    id: String,
    /// Statuses that count as unhealthy (open the breaker).
    unhealthy_statuses: Vec<u16>,
    /// Consecutive unhealthy responses before the breaker opens.
    unhealthy_failures: u32,
    /// Statuses that count as healthy (close the breaker).
    healthy_statuses: Vec<u16>,
    /// Consecutive healthy responses before the breaker fully closes.
    healthy_successes: u32,
    /// Base cooldown seconds; doubles each successive trip (APISIX `2^n`).
    break_base_sec: u64,
    /// Upper bound on the cooldown.
    max_breaker_sec: u64,
    /// Status returned while the breaker is open.
    break_response_code: u16,
    /// Optional body returned while the breaker is open.
    break_response_body: Option<String>,
    resources: Arc<PluginResources>,
}

/// Reads a `Vec<u16>` of HTTP statuses from a JSON array, if present and valid.
fn parse_statuses(v: Option<&serde_json::Value>) -> Result<Option<Vec<u16>>, String> {
    let Some(v) = v else { return Ok(None) };
    let arr = v
        .as_array()
        .ok_or("http_statuses must be an array of integers")?;
    let mut out = Vec::with_capacity(arr.len());
    for item in arr {
        let n = item
            .as_u64()
            .ok_or("http_statuses entries must be integers")?;
        if !(200..=599).contains(&n) {
            return Err(format!(
                "http_statuses entry {} is out of range (200-599)",
                n
            ));
        }
        out.push(n as u16);
    }
    if out.is_empty() {
        return Err("http_statuses must not be empty".to_string());
    }
    Ok(Some(out))
}

impl ApiBreakerPlugin {
    /// Builds one node of the pair from node config.
    ///
    /// Accepted keys:
    /// - `phase` / `role` (string, **required**): `check` (before upstream) or
    ///   `observe` (after upstream).
    /// - `id` (string, **required**): shared breaker identity; the check and
    ///   observe nodes of one pair must use the same `id`.
    /// - `unhealthy.http_statuses` (array, default `[500]`): statuses counted
    ///   as failures. `unhealthy.failures` (integer, default `3`): consecutive
    ///   failures before the breaker opens.
    /// - `healthy.http_statuses` (array, default `[200]`): statuses counted as
    ///   successes. `healthy.successes` (integer, default `3`): consecutive
    ///   successes before the breaker fully closes.
    /// - `break_response_code` (integer, default `502`): status returned while
    ///   the breaker is open.
    /// - `break_response_body` (string, optional): body returned while open.
    /// - `break_base_sec` (integer, default `2`): base cooldown; the open
    ///   window grows as `break_base_sec * 2^trip` (APISIX's `2^n` backoff).
    /// - `max_breaker_sec` (integer, default `300`, min `3`): cooldown ceiling.
    ///
    /// ```yaml
    /// # before upstream
    /// type: api-breaker
    /// config:
    ///   phase: check
    ///   id: orders-api
    ///   break_response_code: 502
    ///   unhealthy: { http_statuses: [500, 503], failures: 3 }
    ///   healthy: { http_statuses: [200], successes: 3 }
    /// ---
    /// # after upstream
    /// type: api-breaker
    /// config:
    ///   phase: observe
    ///   id: orders-api
    ///   unhealthy: { http_statuses: [500, 503], failures: 3 }
    ///   healthy: { http_statuses: [200], successes: 3 }
    /// ```
    pub fn from_config(
        config: &HashMap<String, serde_json::Value>,
        resources: &Arc<PluginResources>,
    ) -> Result<Self, String> {
        let role = match config
            .get("phase")
            .or_else(|| config.get("role"))
            .and_then(|v| v.as_str())
        {
            Some("check") => Role::Check,
            Some("observe") => Role::Observe,
            Some(other) => {
                return Err(format!(
                    "api-breaker: unknown phase/role '{}' (expected 'check' or 'observe')",
                    other
                ))
            }
            None => {
                return Err(
                    "api-breaker: 'phase' (or 'role') is required: 'check' or 'observe'"
                        .to_string(),
                )
            }
        };

        let id = config
            .get("id")
            .and_then(|v| v.as_str())
            .filter(|s| !s.trim().is_empty())
            .ok_or("api-breaker: 'id' is required (links the check/observe pair)")?
            .to_string();

        let unhealthy = config.get("unhealthy");
        let healthy = config.get("healthy");

        let unhealthy_statuses = parse_statuses(unhealthy.and_then(|u| u.get("http_statuses")))?
            .unwrap_or_else(|| vec![500]);
        let healthy_statuses = parse_statuses(healthy.and_then(|h| h.get("http_statuses")))?
            .unwrap_or_else(|| vec![200]);

        let unhealthy_failures = unhealthy
            .and_then(|u| u.get("failures"))
            .and_then(|v| v.as_u64())
            .unwrap_or(3);
        if unhealthy_failures < 1 {
            return Err("api-breaker: unhealthy.failures must be >= 1".to_string());
        }

        let healthy_successes = healthy
            .and_then(|h| h.get("successes"))
            .and_then(|v| v.as_u64())
            .unwrap_or(3);
        if healthy_successes < 1 {
            return Err("api-breaker: healthy.successes must be >= 1".to_string());
        }

        let break_response_code = config
            .get("break_response_code")
            .and_then(|v| v.as_u64())
            .map(|c| c as u16)
            .unwrap_or(502);
        if !(200..=599).contains(&break_response_code) {
            return Err(format!(
                "api-breaker: break_response_code {} is out of range (200-599)",
                break_response_code
            ));
        }

        let break_response_body = config
            .get("break_response_body")
            .and_then(|v| v.as_str())
            .map(String::from);

        let break_base_sec = config
            .get("break_base_sec")
            .and_then(|v| v.as_u64())
            .unwrap_or(2)
            .max(1);

        let max_breaker_sec = config
            .get("max_breaker_sec")
            .and_then(|v| v.as_u64())
            .unwrap_or(300);
        if max_breaker_sec < 3 {
            return Err("api-breaker: max_breaker_sec must be >= 3".to_string());
        }

        Ok(Self {
            role,
            id,
            unhealthy_statuses,
            unhealthy_failures: unhealthy_failures as u32,
            healthy_statuses,
            healthy_successes: healthy_successes as u32,
            break_base_sec,
            max_breaker_sec,
            break_response_code,
            break_response_body,
            resources: resources.clone(),
        })
    }
}

#[async_trait]
impl Plugin for ApiBreakerPlugin {
    fn plugin_type(&self) -> &str {
        "api-breaker"
    }

    async fn execute(
        &self,
        mut ctx: Context,
        _named_inputs: &HashMap<String, serde_json::Value>,
    ) -> PluginResult {
        let breaker = self.resources.traffic.breakers.breaker(&self.id);

        match self.role {
            Role::Check => {
                let allowed = breaker.lock().await.allow();
                if allowed {
                    Ok(PluginOutput {
                        context: ctx,
                        named_outputs: HashMap::new(),
                    })
                } else {
                    ctx.response.status_code = self.break_response_code;
                    if let Some(body) = &self.break_response_body {
                        ctx.response.body = Bytes::from(body.clone());
                    }
                    let error = GatewayError {
                        node_id: String::new(),
                        code: "API_BREAKER_OPEN".to_string(),
                        message: "Circuit breaker is open".to_string(),
                        metadata: HashMap::new(),
                    };
                    Err(PluginExecutionError {
                        context: ctx,
                        error,
                    })
                }
            }
            Role::Observe => {
                let status = ctx.response.status_code;
                if self.unhealthy_statuses.contains(&status) {
                    breaker.lock().await.record_unhealthy(
                        self.unhealthy_failures,
                        self.break_base_sec,
                        self.max_breaker_sec,
                    );
                } else if self.healthy_statuses.contains(&status) {
                    breaker.lock().await.record_healthy(self.healthy_successes);
                }
                Ok(PluginOutput {
                    context: ctx,
                    named_outputs: HashMap::new(),
                })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::{GatewayRequest, GatewayResponse, Protocol};

    fn ctx(status: u16) -> Context {
        Context {
            request: GatewayRequest {
                method: "GET".to_string(),
                path: "/".to_string(),
                host: "localhost".to_string(),
                scheme: "http".to_string(),
                headers: HashMap::new(),
                query_params: HashMap::new(),
                body: Bytes::new(),
                remote_addr: "10.0.0.1:5000".to_string(),
                protocol: Protocol::Http1,
            },
            response: GatewayResponse {
                status_code: status,
                headers: HashMap::new(),
                body: Bytes::new(),
            },
            message: HashMap::new(),
            errors: Vec::new(),
        }
    }

    fn cfg(pairs: &[(&str, serde_json::Value)]) -> HashMap<String, serde_json::Value> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect()
    }

    #[test]
    fn test_missing_id_and_bad_role_fail() {
        let r = PluginResources::empty();
        assert!(
            ApiBreakerPlugin::from_config(&cfg(&[("phase", serde_json::json!("check"))]), &r)
                .is_err()
        );
        assert!(ApiBreakerPlugin::from_config(
            &cfg(&[
                ("phase", serde_json::json!("bogus")),
                ("id", serde_json::json!("x"))
            ]),
            &r
        )
        .is_err());
        assert!(
            ApiBreakerPlugin::from_config(&cfg(&[("id", serde_json::json!("x"))]), &r).is_err()
        );
    }

    #[tokio::test]
    async fn test_observe_trips_and_check_rejects() {
        let r = PluginResources::empty();
        let check = ApiBreakerPlugin::from_config(
            &cfg(&[
                ("phase", serde_json::json!("check")),
                ("id", serde_json::json!("svc")),
                ("break_response_code", serde_json::json!(502)),
                (
                    "unhealthy",
                    serde_json::json!({"http_statuses": [500], "failures": 2}),
                ),
                (
                    "healthy",
                    serde_json::json!({"http_statuses": [200], "successes": 2}),
                ),
                ("max_breaker_sec", serde_json::json!(60)),
            ]),
            &r,
        )
        .unwrap();
        let observe = ApiBreakerPlugin::from_config(
            &cfg(&[
                ("phase", serde_json::json!("observe")),
                ("id", serde_json::json!("svc")),
                (
                    "unhealthy",
                    serde_json::json!({"http_statuses": [500], "failures": 2}),
                ),
                (
                    "healthy",
                    serde_json::json!({"http_statuses": [200], "successes": 2}),
                ),
                ("max_breaker_sec", serde_json::json!(60)),
            ]),
            &r,
        )
        .unwrap();

        // Breaker starts closed → check allows.
        assert!(check.execute(ctx(0), &HashMap::new()).await.is_ok());

        // Two unhealthy responses (threshold 2) → breaker opens.
        observe.execute(ctx(500), &HashMap::new()).await.unwrap();
        observe.execute(ctx(500), &HashMap::new()).await.unwrap();

        // Now check rejects with the break response.
        let err = check
            .execute(ctx(0), &HashMap::new())
            .await
            .expect_err("check should reject while the breaker is open");
        assert_eq!(err.error.code, "API_BREAKER_OPEN");
        assert_eq!(err.context.response.status_code, 502);
    }

    #[tokio::test]
    async fn test_healthy_status_resets_streak() {
        let r = PluginResources::empty();
        let check = ApiBreakerPlugin::from_config(
            &cfg(&[
                ("phase", serde_json::json!("check")),
                ("id", serde_json::json!("svc2")),
                (
                    "unhealthy",
                    serde_json::json!({"http_statuses": [500], "failures": 3}),
                ),
            ]),
            &r,
        )
        .unwrap();
        let observe = ApiBreakerPlugin::from_config(
            &cfg(&[
                ("phase", serde_json::json!("observe")),
                ("id", serde_json::json!("svc2")),
                (
                    "unhealthy",
                    serde_json::json!({"http_statuses": [500], "failures": 3}),
                ),
                (
                    "healthy",
                    serde_json::json!({"http_statuses": [200], "successes": 1}),
                ),
            ]),
            &r,
        )
        .unwrap();

        observe.execute(ctx(500), &HashMap::new()).await.unwrap();
        observe.execute(ctx(500), &HashMap::new()).await.unwrap();
        observe.execute(ctx(200), &HashMap::new()).await.unwrap(); // resets
        observe.execute(ctx(500), &HashMap::new()).await.unwrap();
        // Only one unhealthy since the reset (< threshold 3) → still closed.
        assert!(check.execute(ctx(0), &HashMap::new()).await.is_ok());
    }
}
