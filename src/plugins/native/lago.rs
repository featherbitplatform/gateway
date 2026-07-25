//! The `lago` node — meters API traffic into [Lago](https://getlago.com), an
//! open-source usage-based billing platform.
//!
//! Ported from APISIX's `lago.lua`. Unlike the other loggers this node is a
//! **metering** integration, not an access log: it emits **one billing event
//! per request** so Lago can price API consumption against a customer
//! subscription. Each request produces a Lago usage event; events are buffered
//! by a [`BatchSink`] and POSTed as a batch to `<endpoint>/api/v1/events/batch`
//! with a `Bearer <token>` header. The node passes the context through
//! unchanged.
//!
//! ## Deviations from APISIX
//! - APISIX takes `endpoint_addrs` (an array, one picked at random per flush).
//!   featherbit takes a single `endpoint`.
//! - APISIX builds `properties` from a configured `event_properties` map of
//!   `$var` templates. featherbit reuses the shared log-entry builder, so
//!   `properties` is the standard request/response log entry (or a
//!   `log_format` custom entry). This keeps the metering payload consistent
//!   with the other loggers; configure `log_format` to shape it.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{json, Value};

use crate::batch::{BatchConfig, BatchFlusher, BatchSink, FlushError};
use crate::context::Context;
use crate::outbound::{OutboundClient, OutboundRequest};
use crate::plugins::resources::PluginResources;
use crate::plugins::util::log_entry::{build_entry, parse_log_format};
use crate::plugins::{Plugin, PluginOutput, PluginResult};
use crate::vars;

/// Emits one Lago billing event per request, delivered in batches.
pub struct LagoPlugin {
    sink: BatchSink,
    log_format: Option<HashMap<String, Value>>,
    include_req_body: bool,
    include_resp_body: bool,
    event_transaction_id: String,
    subscription_id: String,
    event_code: String,
}

/// Delivers batched usage events to `<endpoint>/api/v1/events/batch`.
struct LagoFlusher {
    client: Arc<OutboundClient>,
    url: String,
    token: String,
    timeout: Duration,
    ssl_verify: bool,
}

impl LagoPlugin {
    /// Builds the plugin from node config.
    ///
    /// Config keys:
    ///
    /// | Key | Type | Default | Description |
    /// |---|---|---|---|
    /// | `endpoint` | string | — (required) | Lago API base, e.g. `http://127.0.0.1:3000`. |
    /// | `token` | string | — (required) | Lago API key, sent as `Authorization: Bearer <token>`. |
    /// | `event_transaction_id` | string | — (required) | `$var` template for the event's idempotency/transaction id, e.g. `req_$request_uri`. |
    /// | `subscription_id` | string | — (required) | `$var` template identifying the customer subscription, e.g. `cus_$consumer_name`. |
    /// | `event_code` | string | — (required) | Lago billable-metric code the event bills against. |
    /// | `endpoint_uri` | string | `/api/v1/events/batch` | Batch-send path appended to `endpoint`. |
    /// | `ssl_verify` | bool | `true` | Verify the Lago TLS certificate. |
    /// | `timeout` | int (ms) | `3000` | Per-flush HTTP timeout. |
    /// | `log_format` | object | — | Custom `name -> "$var template"` entry used for the event `properties`. |
    /// | `include_req_body` / `include_resp_body` | bool | `false` | Include bodies in the default `properties` entry. |
    ///
    /// Batch keys default `batch_max_size` to **100** (Lago's batch limit)
    /// rather than 1000; other batch keys follow [`BatchConfig::from_config`].
    ///
    /// ```yaml
    /// type: lago
    /// config:
    ///   endpoint: http://127.0.0.1:3000
    ///   token: ${LAGO_API_KEY}
    ///   event_transaction_id: req_$request_uri
    ///   subscription_id: cus_$consumer_name
    ///   event_code: api_calls
    /// ```
    pub fn from_config(
        config: &HashMap<String, Value>,
        resources: &Arc<PluginResources>,
    ) -> Result<Self, String> {
        let endpoint = required_str(config, "endpoint")?
            .trim_end_matches('/')
            .to_string();
        let token = required_str(config, "token")?.to_string();
        let event_transaction_id = required_str(config, "event_transaction_id")?.to_string();
        let subscription_id = required_str(config, "subscription_id")?.to_string();
        let event_code = required_str(config, "event_code")?.to_string();

        let endpoint_uri = config
            .get("endpoint_uri")
            .and_then(|v| v.as_str())
            .unwrap_or("/api/v1/events/batch");
        let ssl_verify = config
            .get("ssl_verify")
            .and_then(|v| v.as_bool())
            .unwrap_or(true);
        let timeout = Duration::from_millis(
            config
                .get("timeout")
                .and_then(|v| v.as_u64())
                .unwrap_or(3000),
        );

        let log_format = parse_log_format(config)?;
        let include_req_body = config
            .get("include_req_body")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let include_resp_body = config
            .get("include_resp_body")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        let mut batch_cfg = BatchConfig::from_config(config).map_err(|e| format!("lago: {e}"))?;
        // Lago's batch endpoint caps a batch at 100 events; default to that.
        if !config.contains_key("batch_max_size") {
            batch_cfg.batch_max_size = 100;
        }

        let flusher = Arc::new(LagoFlusher {
            client: resources.outbound.clone(),
            url: format!("{endpoint}{endpoint_uri}"),
            token,
            timeout,
            ssl_verify,
        });
        let sink = BatchSink::spawn("lago", batch_cfg, flusher);

        Ok(Self {
            sink,
            log_format,
            include_req_body,
            include_resp_body,
            event_transaction_id,
            subscription_id,
            event_code,
        })
    }
}

/// Builds one Lago usage event.
fn build_event(
    transaction_id: &str,
    external_subscription_id: &str,
    code: &str,
    timestamp: u64,
    properties: Value,
) -> Value {
    json!({
        "transaction_id": transaction_id,
        "external_subscription_id": external_subscription_id,
        "code": code,
        "timestamp": timestamp,
        "properties": properties,
    })
}

/// Wraps a batch of events into the Lago batch-events request body.
fn build_batch_body(events: &[Value]) -> Value {
    json!({ "events": events })
}

fn required_str<'a>(config: &'a HashMap<String, Value>, key: &str) -> Result<&'a str, String> {
    config
        .get(key)
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| format!("lago: `{key}` is required"))
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[async_trait]
impl BatchFlusher for LagoFlusher {
    async fn flush(&self, entries: &[Value]) -> Result<(), FlushError> {
        let body = serde_json::to_vec(&build_batch_body(entries)).map_err(|e| FlushError {
            message: format!("failed to encode events batch: {e}"),
            first_fail: None,
        })?;

        let req = OutboundRequest {
            method: http::Method::POST,
            url: self.url.clone(),
            headers: vec![
                ("Content-Type".to_string(), "application/json".to_string()),
                (
                    "Authorization".to_string(),
                    format!("Bearer {}", self.token),
                ),
            ],
            body: body.into(),
            timeout: self.timeout,
            ssl_verify: self.ssl_verify,
        };

        match self.client.request(req).await {
            Ok(resp) if resp.status < 300 => Ok(()),
            Ok(resp) => Err(FlushError {
                message: format!(
                    "lago api returned status {}: {}",
                    resp.status,
                    String::from_utf8_lossy(&resp.body)
                ),
                first_fail: None,
            }),
            Err(e) => Err(FlushError {
                message: e.to_string(),
                first_fail: None,
            }),
        }
    }
}

#[async_trait]
impl Plugin for LagoPlugin {
    fn plugin_type(&self) -> &str {
        "lago"
    }

    async fn execute(&self, ctx: Context, _named_inputs: &HashMap<String, Value>) -> PluginResult {
        let transaction_id = vars::interpolate(&ctx, &self.event_transaction_id);
        let external_subscription_id = vars::interpolate(&ctx, &self.subscription_id);
        let properties = build_entry(
            &ctx,
            self.log_format.as_ref(),
            self.include_req_body,
            self.include_resp_body,
        );
        let event = build_event(
            &transaction_id,
            &external_subscription_id,
            &self.event_code,
            now_secs(),
            properties,
        );
        self.sink.push(event);

        Ok(PluginOutput {
            context: ctx,
            named_outputs: HashMap::new(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(pairs: &[(&str, Value)]) -> HashMap<String, Value> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect()
    }

    fn full_cfg() -> HashMap<String, Value> {
        cfg(&[
            ("endpoint", json!("http://lago:3000/")),
            ("token", json!("secret")),
            ("event_transaction_id", json!("req_$request_uri")),
            ("subscription_id", json!("cus_$consumer_name")),
            ("event_code", json!("api_calls")),
        ])
    }

    #[test]
    fn from_config_requires_fields() {
        for missing in [
            "endpoint",
            "token",
            "event_transaction_id",
            "subscription_id",
            "event_code",
        ] {
            let mut c = full_cfg();
            c.remove(missing);
            let res = LagoPlugin::from_config(&c, &PluginResources::empty());
            let Err(e) = res else {
                panic!("expected error when `{missing}` missing")
            };
            assert!(e.contains(missing));
        }
    }

    #[tokio::test]
    async fn from_config_ok() {
        let p = LagoPlugin::from_config(&full_cfg(), &PluginResources::empty()).unwrap();
        assert_eq!(p.event_code, "api_calls");
        assert_eq!(p.event_transaction_id, "req_$request_uri");
    }

    #[test]
    fn event_envelope_shape() {
        let props = json!({ "request": { "method": "GET" } });
        let ev = build_event("txn-1", "sub-9", "api_calls", 1_700_000_000, props);
        assert_eq!(ev["transaction_id"], "txn-1");
        assert_eq!(ev["external_subscription_id"], "sub-9");
        assert_eq!(ev["code"], "api_calls");
        assert_eq!(ev["timestamp"], 1_700_000_000u64);
        assert_eq!(ev["properties"]["request"]["method"], "GET");
    }

    #[test]
    fn batch_body_wraps_events() {
        let events = vec![
            json!({ "transaction_id": "a" }),
            json!({ "transaction_id": "b" }),
        ];
        let body = build_batch_body(&events);
        assert_eq!(body["events"].as_array().unwrap().len(), 2);
        assert_eq!(body["events"][1]["transaction_id"], "b");
    }
}
