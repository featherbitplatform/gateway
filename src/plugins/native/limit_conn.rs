//! Concurrent-request limiting (`limit-conn`), ported from APISIX's
//! `apisix/plugins/limit-conn`.
//!
//! Concurrency is a property of the *span* between entering and leaving the
//! upstream call, which a single featherbit node cannot observe — a node runs
//! once, at one point in the graph. The behavior is therefore expressed as a
//! **pair of nodes** sharing one in-flight counter:
//!
//! - an **acquire** node placed *before* `upstream`, which increments the
//!   counter and rejects when the ceiling is reached, and
//! - a **release** node placed *after* `upstream` (on **both** the success and
//!   error paths), which decrements the counter.
//!
//! Both nodes are configured with the same `key` template (and identical
//! `conn`/`burst`), so [`crate::traffic::ConnRegistry`] hands them the same
//! [`std::sync::atomic::AtomicI64`]. This is the same "two phases, one shared
//! key" shape `proxy-rewrite` uses for request/response.
//!
//! # Wiring
//!
//! ```text
//!            ┌──────────────────┐        ┌──────────┐        ┌──────────────────┐
//!  listener →│ limit-conn       │success →│ upstream │success →│ limit-conn       │→ client
//!            │  (phase=acquire) │        │          │        │  (phase=release) │
//!            └──────────────────┘        └──────────┘        └──────────────────┘
//!                    │ error                   │ error               ▲
//!                    ▼                         └─────────────────────┘
//!               client.in                 (release also runs on the error path
//!             (503 rejection)              so the counter always decrements)
//! ```
//!
//! The acquire node's `error` port goes to `client.in`: an over-limit request
//! short-circuits straight to the client with the rejection response. The
//! release node must sit on *every* path out of `upstream` (success and error)
//! so a failed upstream call still frees the slot.

use async_trait::async_trait;
use bytes::Bytes;
use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use crate::context::{Context, GatewayError};
use crate::plugins::resources::PluginResources;
use crate::plugins::{Plugin, PluginExecutionError, PluginOutput, PluginResult};

/// Which half of the pair this node is.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Role {
    /// Runs before `upstream`: increments the counter, rejects at the ceiling.
    Acquire,
    /// Runs after `upstream`: decrements the counter (floored at zero).
    Release,
}

/// One node of a `limit-conn` acquire/release pair.
///
/// Holds a handle to the process-wide [`crate::traffic::ConnRegistry`]; the
/// per-key counter is resolved at request time from the interpolated `key` so
/// concurrency is measured per client (or per whatever the key selects), shared
/// across both nodes of the pair.
pub struct LimitConnPlugin {
    role: Role,
    /// Maximum sustained concurrent requests.
    conn: i64,
    /// Extra concurrent requests tolerated above `conn`.
    burst: i64,
    /// Key template (interpolated per request unless `key_type` is `constant`).
    key: String,
    /// When `true`, `key` is used verbatim rather than interpolated.
    key_constant: bool,
    /// Status returned when the limit is exceeded.
    rejected_code: u16,
    /// Optional human-readable rejection message (JSON `{"error_msg": ...}`).
    rejected_msg: Option<String>,
    resources: Arc<PluginResources>,
}

impl LimitConnPlugin {
    /// Builds one node of the pair from node config.
    ///
    /// Accepted keys:
    /// - `phase` / `role` (string, **required**): `acquire` (before upstream)
    ///   or `release` (after upstream). Any other value is a config error.
    /// - `conn` (integer, default `20`): maximum sustained concurrent requests.
    ///   Must be `> 0`.
    /// - `burst` (integer, default `0`): extra concurrent requests tolerated
    ///   above `conn`. Must be `>= 0`. The hard ceiling is `conn + burst`.
    /// - `key` (string template, default `"$remote_addr"`): the concurrency
    ///   key, interpolated per request (see [`crate::vars::interpolate`]). Both
    ///   nodes of a pair **must** use the same `key` and `conn`/`burst` so they
    ///   share one counter.
    /// - `key_type` (string, default `var`): `constant` uses `key` verbatim;
    ///   any other value (`var`, `var_combination`) interpolates it.
    /// - `rejected_code` (integer, default `503`): status for over-limit
    ///   requests.
    /// - `rejected_msg` (string, optional): message returned as a JSON body
    ///   `{"error_msg": "..."}` on rejection.
    /// - `default_conn_delay` (number, optional): accepted for APISIX config
    ///   compatibility but ignored — featherbit rejects rather than delays.
    ///
    /// ```yaml
    /// # before upstream
    /// type: limit-conn
    /// config:
    ///   phase: acquire
    ///   conn: 100
    ///   burst: 50
    ///   key: $remote_addr
    ///   rejected_code: 503
    /// ---
    /// # after upstream (on both success and error paths)
    /// type: limit-conn
    /// config:
    ///   phase: release
    ///   conn: 100
    ///   burst: 50
    ///   key: $remote_addr
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
            Some("acquire") => Role::Acquire,
            Some("release") => Role::Release,
            Some(other) => {
                return Err(format!(
                    "limit-conn: unknown phase/role '{}' (expected 'acquire' or 'release')",
                    other
                ))
            }
            None => {
                return Err(
                    "limit-conn: 'phase' (or 'role') is required: 'acquire' or 'release'"
                        .to_string(),
                )
            }
        };

        let conn = config.get("conn").and_then(|v| v.as_i64()).unwrap_or(20);
        if conn <= 0 {
            return Err(format!(
                "limit-conn: 'conn' must be a positive integer, got {}",
                conn
            ));
        }

        let burst = config.get("burst").and_then(|v| v.as_i64()).unwrap_or(0);
        if burst < 0 {
            return Err(format!(
                "limit-conn: 'burst' must be non-negative, got {}",
                burst
            ));
        }

        let key = config
            .get("key")
            .and_then(|v| v.as_str())
            .unwrap_or("$remote_addr")
            .to_string();

        let key_constant = matches!(
            config.get("key_type").and_then(|v| v.as_str()),
            Some("constant")
        );

        let rejected_code = config
            .get("rejected_code")
            .and_then(|v| v.as_u64())
            .map(|c| c as u16)
            .unwrap_or(503);

        let rejected_msg = config
            .get("rejected_msg")
            .and_then(|v| v.as_str())
            .map(String::from);

        Ok(Self {
            role,
            conn,
            burst,
            key,
            key_constant,
            rejected_code,
            rejected_msg,
            resources: resources.clone(),
        })
    }

    /// Resolves the concurrency key for this request.
    fn resolve_key(&self, ctx: &Context) -> String {
        if self.key_constant {
            self.key.clone()
        } else {
            crate::vars::interpolate(ctx, &self.key)
        }
    }
}

#[async_trait]
impl Plugin for LimitConnPlugin {
    fn plugin_type(&self) -> &str {
        "limit-conn"
    }

    async fn execute(
        &self,
        mut ctx: Context,
        _named_inputs: &HashMap<String, serde_json::Value>,
    ) -> PluginResult {
        let key = self.resolve_key(&ctx);
        let counter = self.resources.traffic.conn.counter(&key);

        match self.role {
            Role::Acquire => {
                // fetch_add returns the value *before* the increment, i.e. the
                // number of requests already in flight.
                let in_flight = counter.fetch_add(1, Ordering::SeqCst);
                if in_flight >= self.conn + self.burst {
                    // Over the ceiling — undo our increment and reject.
                    counter.fetch_sub(1, Ordering::SeqCst);

                    ctx.response.status_code = self.rejected_code;
                    let body = match &self.rejected_msg {
                        Some(msg) => format!(r#"{{"error_msg":{}}}"#, json_string(msg)),
                        None => r#"{"error":"limit_conn_exceeded"}"#.to_string(),
                    };
                    ctx.response.body = Bytes::from(body);
                    ctx.response.headers.insert(
                        "content-type".to_string(),
                        vec!["application/json".to_string()],
                    );

                    let error = GatewayError {
                        node_id: String::new(),
                        code: "LIMIT_CONN_EXCEEDED".to_string(),
                        message: "Concurrent request limit exceeded".to_string(),
                        metadata: HashMap::new(),
                    };
                    return Err(PluginExecutionError {
                        context: ctx,
                        error,
                    });
                }
                Ok(PluginOutput {
                    context: ctx,
                    named_outputs: HashMap::new(),
                })
            }
            Role::Release => {
                // Decrement, floored at zero so a stray release (e.g. an
                // acquire that rejected but was still wired to a release) can
                // never drive the counter negative.
                let mut cur = counter.load(Ordering::SeqCst);
                while cur > 0 {
                    match counter.compare_exchange_weak(
                        cur,
                        cur - 1,
                        Ordering::SeqCst,
                        Ordering::SeqCst,
                    ) {
                        Ok(_) => break,
                        Err(actual) => cur = actual,
                    }
                }
                Ok(PluginOutput {
                    context: ctx,
                    named_outputs: HashMap::new(),
                })
            }
        }
    }
}

/// Minimal JSON string escaping for the rejection message.
fn json_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::{GatewayRequest, GatewayResponse, Protocol};

    fn ctx() -> Context {
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
                status_code: 0,
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
    fn test_unknown_role_and_bad_thresholds_fail() {
        let r = PluginResources::empty();
        assert!(
            LimitConnPlugin::from_config(&cfg(&[("phase", serde_json::json!("nope"))]), &r)
                .is_err()
        );
        assert!(
            LimitConnPlugin::from_config(&cfg(&[]), &r).is_err(),
            "missing phase must fail"
        );
        assert!(LimitConnPlugin::from_config(
            &cfg(&[
                ("phase", serde_json::json!("acquire")),
                ("conn", serde_json::json!(0))
            ]),
            &r
        )
        .is_err());
        assert!(LimitConnPlugin::from_config(
            &cfg(&[
                ("phase", serde_json::json!("acquire")),
                ("burst", serde_json::json!(-1))
            ]),
            &r
        )
        .is_err());
    }

    #[tokio::test]
    async fn test_acquire_increments_rejects_at_limit_release_decrements() {
        // Shared resources → shared counter registry across the pair.
        let r = PluginResources::empty();
        let acquire = LimitConnPlugin::from_config(
            &cfg(&[
                ("phase", serde_json::json!("acquire")),
                ("conn", serde_json::json!(3)),
                ("key", serde_json::json!("$remote_addr")),
                ("rejected_code", serde_json::json!(503)),
            ]),
            &r,
        )
        .unwrap();
        let release = LimitConnPlugin::from_config(
            &cfg(&[
                ("phase", serde_json::json!("release")),
                ("conn", serde_json::json!(3)),
                ("key", serde_json::json!("$remote_addr")),
            ]),
            &r,
        )
        .unwrap();

        // conn=3, burst=0 → 3 concurrent allowed, the 4th rejected.
        for i in 0..3 {
            let out = acquire.execute(ctx(), &HashMap::new()).await;
            assert!(out.is_ok(), "acquire #{} should be allowed", i + 1);
        }
        let rejected = acquire.execute(ctx(), &HashMap::new()).await;
        let err = rejected.expect_err("4th concurrent acquire must be rejected");
        assert_eq!(err.error.code, "LIMIT_CONN_EXCEEDED");
        assert_eq!(err.context.response.status_code, 503);

        // Release one slot → the next acquire succeeds again.
        release.execute(ctx(), &HashMap::new()).await.unwrap();
        let out = acquire.execute(ctx(), &HashMap::new()).await;
        assert!(
            out.is_ok(),
            "acquire should succeed after a release freed a slot"
        );
    }

    #[tokio::test]
    async fn test_release_floors_at_zero() {
        let r = PluginResources::empty();
        let release = LimitConnPlugin::from_config(
            &cfg(&[
                ("phase", serde_json::json!("release")),
                ("conn", serde_json::json!(5)),
                ("key", serde_json::json!("const")),
                ("key_type", serde_json::json!("constant")),
            ]),
            &r,
        )
        .unwrap();
        // Extra releases must not drive the counter negative.
        release.execute(ctx(), &HashMap::new()).await.unwrap();
        release.execute(ctx(), &HashMap::new()).await.unwrap();
        assert_eq!(r.traffic.conn.counter("const").load(Ordering::SeqCst), 0);
    }
}
