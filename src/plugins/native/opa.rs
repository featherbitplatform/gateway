//! Open Policy Agent authorization plugin (`opa`).
//!
//! Delegates the access decision for each request to an external OPA server.
//! The plugin builds an OPA *input document* describing the request (and,
//! optionally, the matched consumer), POSTs it to
//! `<host>/v1/data/<policy>`, and interprets the decision under `result`:
//! `allow: true` continues (optionally copying selected OPA-provided headers
//! onto the request forwarded upstream), while `allow: false`/missing rejects,
//! honoring OPA-supplied status, headers, and reason. A callout or
//! response-parse failure blocks the request by default.
//!
//! Ports the APISIX `opa` plugin (and its `opa/helper.lua` input builder) onto
//! featherbit's shared outbound HTTP client.
//!
//! ## Deviations from APISIX
//!
//! featherbit has no route/service objects, so `with_route` and `with_service`
//! are accepted for config compatibility but are **no-ops** (no `route` /
//! `service` object is added to the input document). The input `var` block
//! omits `server_addr` / `server_port`, which featherbit does not track in the
//! request context.

use async_trait::async_trait;
use bytes::Bytes;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::context::{Context, GatewayError};
use crate::outbound::{OutboundClient, OutboundRequest};
use crate::plugins::resources::PluginResources;
use crate::plugins::{Plugin, PluginExecutionError, PluginOutput, PluginResult};

/// Sends an OPA input document to an external policy server and routes on the
/// returned decision: `allow: true` continues (success port), otherwise
/// rejects (error port).
pub struct OpaPlugin {
    /// OPA base URL (e.g. `http://opa:8181`).
    host: String,
    /// Decision path appended as `/v1/data/<policy>`.
    policy: String,
    /// TLS certificate verification for `https` callouts.
    ssl_verify: bool,
    /// Whole-call callout deadline.
    timeout: Duration,
    /// Include the matched consumer object in the input document.
    with_consumer: bool,
    /// OPA-response header names copied onto the request forwarded upstream on
    /// allow (lowercased). Empty means none are copied.
    send_headers_upstream: Vec<String>,
    /// Shared pooled HTTP client (from [`PluginResources`]).
    client: Arc<OutboundClient>,
}

/// A parsed OPA decision (the `result` object of the response).
#[derive(Debug, PartialEq)]
struct OpaDecision {
    allow: bool,
    /// Deny status from `result.status_code` (or `result.status`), if present.
    status: Option<u16>,
    /// Stringified `result.reason`, if present (objects are JSON-encoded).
    reason: Option<String>,
    /// `result.headers` as lowercased name→value pairs.
    headers: HashMap<String, String>,
}

/// Why an OPA response could not be interpreted as a decision.
#[derive(Debug, PartialEq)]
enum OpaParseError {
    /// Body was not valid JSON.
    NotJson,
    /// Body parsed but had no `result` object.
    MissingResult,
}

impl OpaPlugin {
    /// Builds the plugin from node config.
    ///
    /// Accepted keys:
    /// - `host` (string, **required**): OPA base URL. Missing → config error.
    /// - `policy` (string, **required**): decision path appended as
    ///   `/v1/data/<policy>`. Missing → config error.
    /// - `ssl_verify` (bool, default `true`): verify TLS certificates for
    ///   `https` callouts.
    /// - `timeout` (integer ms, default `3000`): whole-call callout deadline.
    /// - `with_consumer` (bool, default `false`): include a `consumer` object
    ///   (built from `context.message`'s `consumer.*` keys) in the input
    ///   document.
    /// - `with_route` / `with_service` (bool, default `false`): accepted for
    ///   APISIX compatibility but **no-ops** — featherbit has no route/service
    ///   objects.
    /// - `send_headers_upstream` (array of strings, optional): OPA-response
    ///   header names copied onto the request forwarded upstream on allow. A
    ///   configured name absent from the OPA response removes any
    ///   client-supplied value.
    ///
    /// ```yaml
    /// type: opa
    /// config:
    ///   host: http://opa:8181
    ///   policy: example/allow
    ///   with_consumer: true
    ///   send_headers_upstream: [x-user-id]
    ///   ssl_verify: true
    ///   timeout: 3000
    /// ```
    pub fn from_config(
        config: &HashMap<String, serde_json::Value>,
        resources: &Arc<PluginResources>,
    ) -> Result<Self, String> {
        let host = config
            .get("host")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| "opa plugin requires 'host'".to_string())?
            .trim_end_matches('/')
            .to_string();

        let policy = config
            .get("policy")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| "opa plugin requires 'policy'".to_string())?
            .trim_start_matches('/')
            .to_string();

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

        let with_consumer = config
            .get("with_consumer")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        let send_headers_upstream = config
            .get("send_headers_upstream")
            .and_then(|v| v.as_array())
            .map(|seq| {
                seq.iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_lowercase()))
                    .collect()
            })
            .unwrap_or_default();

        Ok(Self {
            host,
            policy,
            ssl_verify,
            timeout,
            with_consumer,
            send_headers_upstream,
            client: resources.outbound.clone(),
        })
    }

    /// Builds the OPA input document from the context, mirroring
    /// `opa/helper.lua`'s `build_opa_input` as closely as featherbit's
    /// [`Context`] allows. `now_unix` is injected so the (otherwise
    /// wall-clock) `var.timestamp` is testable.
    fn build_opa_input(&self, ctx: &Context, now_unix: u64) -> serde_json::Value {
        let (host, port) = split_host_port(&ctx.request.host, &ctx.request.scheme);
        let (remote_addr, remote_port) = split_addr_port(&ctx.request.remote_addr);

        let request = serde_json::json!({
            "scheme": ctx.request.scheme,
            "method": ctx.request.method,
            "host": host,
            "port": port,
            "path": ctx.request.path,
            "headers": collapse_multi(&ctx.request.headers),
            "query": collapse_multi(&ctx.request.query_params),
        });

        let mut var = serde_json::Map::new();
        var.insert("remote_addr".to_string(), serde_json::json!(remote_addr));
        if let Some(p) = remote_port {
            var.insert("remote_port".to_string(), serde_json::json!(p));
        }
        var.insert("timestamp".to_string(), serde_json::json!(now_unix));

        let mut input = serde_json::Map::new();
        input.insert("type".to_string(), serde_json::json!("http"));
        input.insert("request".to_string(), request);
        input.insert("var".to_string(), serde_json::Value::Object(var));

        if self.with_consumer {
            if let Some(consumer) = build_consumer(ctx) {
                input.insert("consumer".to_string(), consumer);
            }
        }

        serde_json::json!({ "input": serde_json::Value::Object(input) })
    }

    /// On an allow decision, copies the configured `send_headers_upstream`
    /// from the OPA response onto the request forwarded upstream. A configured
    /// header absent from the OPA response removes any client-supplied value.
    fn apply_allow(&self, ctx: &mut Context, decision: &OpaDecision) {
        for name in &self.send_headers_upstream {
            match decision.headers.get(name) {
                Some(value) => {
                    ctx.request
                        .headers
                        .insert(name.clone(), vec![value.clone()]);
                }
                None => {
                    ctx.request.headers.remove(name);
                }
            }
        }
    }

    /// Builds the `OPA_DENIED` rejection from a deny decision, honoring
    /// OPA-supplied status (default `403`), headers, and reason (as the body).
    fn build_deny(&self, mut ctx: Context, decision: &OpaDecision) -> PluginExecutionError {
        let status = decision.status.unwrap_or(403);
        ctx.response.status_code = status;
        if let Some(reason) = &decision.reason {
            ctx.response.body = Bytes::from(reason.clone());
        }
        for (name, value) in &decision.headers {
            ctx.response
                .headers
                .insert(name.clone(), vec![value.clone()]);
        }
        PluginExecutionError {
            context: ctx,
            error: GatewayError {
                node_id: String::new(),
                code: "OPA_DENIED".to_string(),
                message: format!("OPA denied the request ({})", status),
                metadata: HashMap::new(),
            },
        }
    }

    /// Builds the `OPA_ERROR` rejection used when the callout fails or the
    /// response cannot be interpreted as a decision.
    fn build_error(&self, mut ctx: Context, status: u16, message: String) -> PluginExecutionError {
        ctx.response.status_code = status;
        PluginExecutionError {
            context: ctx,
            error: GatewayError {
                node_id: String::new(),
                code: "OPA_ERROR".to_string(),
                message,
                metadata: HashMap::new(),
            },
        }
    }
}

/// Parses an OPA response body into an [`OpaDecision`]. Pure and network-free
/// so the decision-mapping logic is unit-testable.
fn parse_opa_response(body: &[u8]) -> Result<OpaDecision, OpaParseError> {
    let data: serde_json::Value =
        serde_json::from_slice(body).map_err(|_| OpaParseError::NotJson)?;
    let result = data.get("result").ok_or(OpaParseError::MissingResult)?;

    let allow = result
        .get("allow")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let status = result
        .get("status_code")
        .or_else(|| result.get("status"))
        .and_then(|v| v.as_u64())
        .map(|n| n as u16);

    let reason = result.get("reason").and_then(|v| match v {
        serde_json::Value::Null => None,
        serde_json::Value::String(s) => Some(s.clone()),
        other => Some(other.to_string()),
    });

    let headers = result
        .get("headers")
        .and_then(|v| v.as_object())
        .map(|obj| {
            obj.iter()
                .filter_map(|(k, v)| {
                    let value = match v {
                        serde_json::Value::String(s) => s.clone(),
                        serde_json::Value::Number(n) => n.to_string(),
                        serde_json::Value::Bool(b) => b.to_string(),
                        _ => return None,
                    };
                    Some((k.to_lowercase(), value))
                })
                .collect()
        })
        .unwrap_or_default();

    Ok(OpaDecision {
        allow,
        status,
        reason,
        headers,
    })
}

/// Collapses a multi-valued header/query map into single strings where there
/// is one value and arrays where there are several — mirroring OpenResty's
/// `core.request.headers` / `get_uri_args` shape.
fn collapse_multi(map: &HashMap<String, Vec<String>>) -> serde_json::Value {
    let mut out = serde_json::Map::new();
    for (k, values) in map {
        let value = match values.as_slice() {
            [single] => serde_json::json!(single),
            _ => serde_json::json!(values),
        };
        out.insert(k.clone(), value);
    }
    serde_json::Value::Object(out)
}

/// Builds the `consumer` object from `context.message`'s `consumer.*` keys,
/// stripping the prefix. Returns `None` when no consumer is attached.
fn build_consumer(ctx: &Context) -> Option<serde_json::Value> {
    let mut obj = serde_json::Map::new();
    for (key, value) in &ctx.message {
        if let Some(field) = key.strip_prefix("consumer.") {
            obj.insert(field.to_string(), value.clone());
        }
    }
    if obj.is_empty() {
        None
    } else {
        Some(serde_json::Value::Object(obj))
    }
}

/// Splits a `Host` header into `(hostname, port)`, defaulting the port from
/// the scheme when the host carries none.
fn split_host_port(host: &str, scheme: &str) -> (String, u16) {
    let default_port = if scheme.eq_ignore_ascii_case("https") {
        443
    } else {
        80
    };
    // Bracketed IPv6 literal: [::1]:8080
    if let Some(stripped) = host.strip_prefix('[') {
        if let Some((h, rest)) = stripped.split_once(']') {
            let port = rest
                .strip_prefix(':')
                .and_then(|p| p.parse().ok())
                .unwrap_or(default_port);
            return (h.to_string(), port);
        }
    }
    match host.rsplit_once(':') {
        // Bare IPv6 (multiple colons) — no port.
        Some((h, port)) if !h.contains(':') => {
            (h.to_string(), port.parse().unwrap_or(default_port))
        }
        _ => (host.to_string(), default_port),
    }
}

/// Splits a client `ip:port` into `(ip, port)`, tolerating bare IPs and
/// bracketed IPv6.
fn split_addr_port(addr: &str) -> (String, Option<u16>) {
    if let Some(stripped) = addr.strip_prefix('[') {
        if let Some((ip, rest)) = stripped.split_once(']') {
            return (
                ip.to_string(),
                rest.strip_prefix(':').and_then(|p| p.parse().ok()),
            );
        }
    }
    match addr.rsplit_once(':') {
        Some((ip, port)) if !ip.contains(':') => (ip.to_string(), port.parse().ok()),
        _ => (addr.to_string(), None),
    }
}

#[async_trait]
impl Plugin for OpaPlugin {
    fn plugin_type(&self) -> &str {
        "opa"
    }

    async fn execute(
        &self,
        mut ctx: Context,
        _named_inputs: &HashMap<String, serde_json::Value>,
    ) -> PluginResult {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let input = self.build_opa_input(&ctx, now);
        let body = match serde_json::to_vec(&input) {
            Ok(b) => Bytes::from(b),
            Err(e) => {
                return Err(self.build_error(
                    ctx,
                    503,
                    format!("failed to encode OPA input: {}", e),
                ))
            }
        };

        let url = format!("{}/v1/data/{}", self.host, self.policy);
        let request = OutboundRequest {
            method: http::Method::POST,
            url,
            headers: vec![("Content-Type".to_string(), "application/json".to_string())],
            body,
            timeout: self.timeout,
            ssl_verify: self.ssl_verify,
        };

        // Block by default when the decision is unavailable.
        let response = match self.client.request(request).await {
            Ok(resp) => resp,
            Err(e) => return Err(self.build_error(ctx, 403, format!("OPA callout failed: {}", e))),
        };

        let decision = match parse_opa_response(&response.body) {
            Ok(d) => d,
            Err(e) => {
                let message = match e {
                    OpaParseError::NotJson => "OPA response was not valid JSON".to_string(),
                    OpaParseError::MissingResult => {
                        "OPA response missing 'result' field".to_string()
                    }
                };
                return Err(self.build_error(ctx, 503, message));
            }
        };

        if !decision.allow {
            return Err(self.build_deny(ctx, &decision));
        }

        self.apply_allow(&mut ctx, &decision);
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

    fn test_ctx() -> Context {
        let mut headers = HashMap::new();
        headers.insert("accept".to_string(), vec!["application/json".to_string()]);
        headers.insert(
            "x-multi".to_string(),
            vec!["a".to_string(), "b".to_string()],
        );
        let mut query = HashMap::new();
        query.insert("q".to_string(), vec!["1".to_string()]);
        let mut message = HashMap::new();
        message.insert("consumer.name".to_string(), serde_json::json!("alice"));
        message.insert(
            "consumer.auth_type".to_string(),
            serde_json::json!("key-auth"),
        );
        Context {
            request: GatewayRequest {
                method: "GET".to_string(),
                path: "/api/users".to_string(),
                host: "example.com:8080".to_string(),
                scheme: "http".to_string(),
                headers,
                query_params: query,
                body: Bytes::new(),
                remote_addr: "10.0.0.7:5555".to_string(),
                protocol: Protocol::Http1,
            },
            response: GatewayResponse {
                status_code: 0,
                headers: HashMap::new(),
                body: Bytes::new(),
            },
            message,
            errors: Vec::new(),
        }
    }

    fn plugin(config: serde_json::Value) -> OpaPlugin {
        let map: HashMap<String, serde_json::Value> = serde_json::from_value(config).unwrap();
        OpaPlugin::from_config(&map, &PluginResources::empty()).unwrap()
    }

    #[test]
    fn test_requires_host_and_policy() {
        assert!(OpaPlugin::from_config(&HashMap::new(), &PluginResources::empty()).is_err());
        let mut map = HashMap::new();
        map.insert("host".to_string(), serde_json::json!("http://opa"));
        assert!(OpaPlugin::from_config(&map, &PluginResources::empty()).is_err());
        map.insert("policy".to_string(), serde_json::json!("p/allow"));
        assert!(OpaPlugin::from_config(&map, &PluginResources::empty()).is_ok());
    }

    #[test]
    fn test_host_and_policy_trimmed() {
        let p = plugin(serde_json::json!({ "host": "http://opa/", "policy": "/p/allow" }));
        assert_eq!(p.host, "http://opa");
        assert_eq!(p.policy, "p/allow");
    }

    #[test]
    fn test_build_input_shape() {
        let p = plugin(serde_json::json!({
            "host": "http://opa",
            "policy": "p/allow",
            "with_consumer": true
        }));
        let input = p.build_opa_input(&test_ctx(), 1234);
        let req = &input["input"]["request"];
        assert_eq!(req["scheme"], serde_json::json!("http"));
        assert_eq!(req["method"], serde_json::json!("GET"));
        assert_eq!(req["host"], serde_json::json!("example.com"));
        assert_eq!(req["port"], serde_json::json!(8080));
        assert_eq!(req["path"], serde_json::json!("/api/users"));
        // single header collapses to string, multi stays array
        assert_eq!(
            req["headers"]["accept"],
            serde_json::json!("application/json")
        );
        assert_eq!(req["headers"]["x-multi"], serde_json::json!(["a", "b"]));
        assert_eq!(req["query"]["q"], serde_json::json!("1"));
        assert_eq!(
            input["input"]["var"]["remote_addr"],
            serde_json::json!("10.0.0.7")
        );
        assert_eq!(
            input["input"]["var"]["remote_port"],
            serde_json::json!(5555)
        );
        assert_eq!(input["input"]["var"]["timestamp"], serde_json::json!(1234));
        // consumer built from message consumer.* keys
        assert_eq!(
            input["input"]["consumer"]["name"],
            serde_json::json!("alice")
        );
        assert_eq!(
            input["input"]["consumer"]["auth_type"],
            serde_json::json!("key-auth")
        );
    }

    #[test]
    fn test_build_input_omits_consumer_when_disabled() {
        let p = plugin(serde_json::json!({ "host": "http://opa", "policy": "p" }));
        let input = p.build_opa_input(&test_ctx(), 0);
        assert!(input["input"].get("consumer").is_none());
    }

    #[test]
    fn test_parse_allow() {
        let d = parse_opa_response(br#"{"result": {"allow": true}}"#).unwrap();
        assert!(d.allow);
        assert_eq!(d.status, None);
    }

    #[test]
    fn test_parse_deny_with_details() {
        let body = br#"{"result": {"allow": false, "status_code": 401,
            "reason": "no", "headers": {"WWW-Authenticate": "Bearer"}}}"#;
        let d = parse_opa_response(body).unwrap();
        assert!(!d.allow);
        assert_eq!(d.status, Some(401));
        assert_eq!(d.reason.as_deref(), Some("no"));
        assert_eq!(
            d.headers.get("www-authenticate"),
            Some(&"Bearer".to_string())
        );
    }

    #[test]
    fn test_parse_object_reason_encoded() {
        let d =
            parse_opa_response(br#"{"result": {"allow": false, "reason": {"m": "x"}}}"#).unwrap();
        assert_eq!(d.reason.as_deref(), Some("{\"m\":\"x\"}"));
    }

    #[test]
    fn test_parse_errors() {
        assert_eq!(parse_opa_response(b"not json"), Err(OpaParseError::NotJson));
        assert_eq!(
            parse_opa_response(br#"{"foo": 1}"#),
            Err(OpaParseError::MissingResult)
        );
        // missing allow defaults to false (deny)
        let d = parse_opa_response(br#"{"result": {}}"#).unwrap();
        assert!(!d.allow);
    }

    #[test]
    fn test_build_deny_applies_status_headers_reason() {
        let p = plugin(serde_json::json!({ "host": "http://opa", "policy": "p" }));
        let decision = OpaDecision {
            allow: false,
            status: Some(401),
            reason: Some("denied".to_string()),
            headers: HashMap::from([("x-why".to_string(), "policy".to_string())]),
        };
        let err = p.build_deny(test_ctx(), &decision);
        assert_eq!(err.error.code, "OPA_DENIED");
        assert_eq!(err.context.response.status_code, 401);
        assert_eq!(err.context.response.body, Bytes::from_static(b"denied"));
        assert_eq!(
            err.context.response.headers.get("x-why"),
            Some(&vec!["policy".to_string()])
        );
    }

    #[test]
    fn test_apply_allow_sets_and_removes() {
        let p = plugin(serde_json::json!({
            "host": "http://opa",
            "policy": "p",
            "send_headers_upstream": ["X-User-Id", "X-Absent"]
        }));
        let mut ctx = test_ctx();
        ctx.request
            .headers
            .insert("x-absent".to_string(), vec!["stale".to_string()]);
        let decision = OpaDecision {
            allow: true,
            status: None,
            reason: None,
            headers: HashMap::from([("x-user-id".to_string(), "u42".to_string())]),
        };
        p.apply_allow(&mut ctx, &decision);
        assert_eq!(
            ctx.request.headers.get("x-user-id"),
            Some(&vec!["u42".to_string()])
        );
        assert!(!ctx.request.headers.contains_key("x-absent"));
    }

    #[test]
    fn test_split_host_port() {
        assert_eq!(split_host_port("h:8080", "http"), ("h".to_string(), 8080));
        assert_eq!(split_host_port("h", "https"), ("h".to_string(), 443));
        assert_eq!(split_host_port("h", "http"), ("h".to_string(), 80));
        assert_eq!(
            split_host_port("[::1]:9000", "http"),
            ("::1".to_string(), 9000)
        );
    }
}
