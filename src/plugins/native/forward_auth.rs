//! Forward-authentication plugin (`forward-auth`).
//!
//! Delegates the access decision for each request to an external HTTP
//! authorization service. The plugin issues a callout carrying the original
//! request's forwarding metadata (`X-Forwarded-*`) plus any configured client
//! headers; a 2xx reply lets the request continue (optionally copying selected
//! auth-response headers onto the request forwarded upstream), while a non-2xx
//! reply rejects the request, mirroring the auth service's status/body/headers
//! back to the client. A callout failure either degrades open or rejects with
//! a configurable status, depending on `allow_degradation`.
//!
//! Ports the APISIX `forward-auth` plugin onto featherbit's shared outbound
//! HTTP client.

use async_trait::async_trait;
use bytes::Bytes;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use crate::context::{Context, GatewayError};
use crate::outbound::{OutboundClient, OutboundRequest, OutboundResponse};
use crate::plugins::resources::PluginResources;
use crate::plugins::{Plugin, PluginExecutionError, PluginOutput, PluginResult};

/// Sends each request to an external authorization endpoint and routes on its
/// verdict: 2xx continues (success port), non-2xx or (non-degrading) callout
/// failure rejects (error port).
pub struct ForwardAuthPlugin {
    /// External authorization endpoint.
    uri: String,
    /// Callout method (`GET` or `POST`); `POST` forwards the client body.
    method: http::Method,
    /// Whether the callout is a `POST` (forwards the client body).
    is_post: bool,
    /// Client request header names copied onto the callout (lowercased).
    request_headers: Vec<String>,
    /// Auth-response header names copied onto the request forwarded upstream
    /// on success (lowercased).
    upstream_headers: Vec<String>,
    /// Auth-response header names copied onto the client-facing response on
    /// failure (lowercased).
    client_headers: Vec<String>,
    /// Extra callout headers whose values may reference `$var` templates.
    extra_headers: Vec<(String, String)>,
    /// TLS certificate verification for `https` callouts.
    ssl_verify: bool,
    /// Whole-call callout deadline.
    timeout: Duration,
    /// Status returned when the callout fails and degradation is disabled.
    status_on_error: u16,
    /// When true, a callout failure lets the request through instead of
    /// rejecting it.
    allow_degradation: bool,
    /// Shared pooled HTTP client (from [`PluginResources`]).
    client: Arc<OutboundClient>,
}

impl ForwardAuthPlugin {
    /// Builds the plugin from node config.
    ///
    /// Accepted keys:
    /// - `uri` (string, **required**): external authorization endpoint. A
    ///   missing `uri` is a config-load error.
    /// - `request_method` (string, default `GET`): callout method; `GET` or
    ///   `POST`. When `POST`, the buffered client body is forwarded and the
    ///   client's `Content-Encoding` header is preserved on the callout.
    /// - `request_headers` (array of strings, default `[]`): client header
    ///   names forwarded to the authorization service (looked up
    ///   case-insensitively).
    /// - `upstream_headers` (array of strings, default `[]`): auth-response
    ///   header names copied onto the request forwarded upstream on success.
    ///   A configured name absent from the auth response removes any
    ///   client-supplied value.
    /// - `client_headers` (array of strings, default `[]`): auth-response
    ///   header names copied onto the client-facing response on failure.
    /// - `extra_headers` (object of string→string, optional): additional
    ///   callout headers; values support `$var` / `${var}` interpolation
    ///   (e.g. `$remote_addr`, `$request_uri`).
    /// - `ssl_verify` (bool, default `true`): verify TLS certificates for
    ///   `https` callouts.
    /// - `timeout` (integer ms, default `3000`): whole-call callout deadline.
    /// - `status_on_error` (integer, default `403`): status returned when the
    ///   callout fails and `allow_degradation` is false.
    /// - `allow_degradation` (bool, default `false`): when true, a callout
    ///   failure lets the request continue instead of rejecting it.
    ///
    /// ```yaml
    /// type: forward-auth
    /// config:
    ///   uri: http://auth-service:8080/verify
    ///   request_method: GET
    ///   request_headers: [authorization, cookie]
    ///   upstream_headers: [x-user-id]
    ///   client_headers: [www-authenticate]
    ///   ssl_verify: true
    ///   timeout: 3000
    ///   status_on_error: 403
    ///   allow_degradation: false
    /// ```
    pub fn from_config(
        config: &HashMap<String, serde_json::Value>,
        resources: &Arc<PluginResources>,
    ) -> Result<Self, String> {
        let uri = config
            .get("uri")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| "forward-auth plugin requires 'uri'".to_string())?
            .to_string();

        let method_str = config
            .get("request_method")
            .and_then(|v| v.as_str())
            .unwrap_or("GET")
            .to_uppercase();
        let (method, is_post) = match method_str.as_str() {
            "GET" => (http::Method::GET, false),
            "POST" => (http::Method::POST, true),
            other => {
                return Err(format!(
                    "forward-auth request_method must be GET or POST, got '{}'",
                    other
                ))
            }
        };

        let string_list = |key: &str| -> Vec<String> {
            config
                .get(key)
                .and_then(|v| v.as_array())
                .map(|seq| {
                    seq.iter()
                        .filter_map(|v| v.as_str().map(|s| s.to_lowercase()))
                        .collect()
                })
                .unwrap_or_default()
        };

        let extra_headers = config
            .get("extra_headers")
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
                        Some((k.clone(), value))
                    })
                    .collect()
            })
            .unwrap_or_default();

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

        let status_on_error = config
            .get("status_on_error")
            .and_then(|v| v.as_u64())
            .unwrap_or(403) as u16;

        let allow_degradation = config
            .get("allow_degradation")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        Ok(Self {
            uri,
            method,
            is_post,
            request_headers: string_list("request_headers"),
            upstream_headers: string_list("upstream_headers"),
            client_headers: string_list("client_headers"),
            extra_headers,
            ssl_verify,
            timeout,
            status_on_error,
            allow_degradation,
            client: resources.outbound.clone(),
        })
    }

    /// Builds the header list sent on the callout to the authorization
    /// service: the `X-Forwarded-*` forwarding set, `Content-Encoding` for
    /// `POST`, any interpolated `extra_headers`, and the configured
    /// `request_headers` copied from the client request.
    fn build_callout_headers(&self, ctx: &Context) -> Vec<(String, String)> {
        let request_uri = crate::vars::resolve(ctx, "request_uri")
            .map(|c| c.into_owned())
            .unwrap_or_else(|| ctx.request.path.clone());
        let remote_ip = crate::vars::resolve(ctx, "remote_addr")
            .map(|c| c.into_owned())
            .unwrap_or_default();

        let mut headers: Vec<(String, String)> = vec![
            ("X-Forwarded-Proto".to_string(), ctx.request.scheme.clone()),
            ("X-Forwarded-Method".to_string(), ctx.request.method.clone()),
            ("X-Forwarded-Host".to_string(), ctx.request.host.clone()),
            ("X-Forwarded-Uri".to_string(), request_uri),
            ("X-Forwarded-For".to_string(), remote_ip),
        ];

        if self.is_post {
            if let Some(enc) = ctx
                .request
                .headers
                .get("content-encoding")
                .and_then(|v| v.first())
            {
                headers.push(("Content-Encoding".to_string(), enc.clone()));
            }
        }

        for (name, template) in &self.extra_headers {
            headers.push((name.clone(), crate::vars::interpolate(ctx, template)));
        }

        // Copy configured client headers unless already set above.
        for name in &self.request_headers {
            let already = headers
                .iter()
                .any(|(existing, _)| existing.eq_ignore_ascii_case(name));
            if already {
                continue;
            }
            if let Some(value) = ctx.request.headers.get(name).and_then(|v| v.first()) {
                headers.push((name.clone(), value.clone()));
            }
        }

        headers
    }

    /// On a 2xx auth reply, copies the configured `upstream_headers` from the
    /// auth response onto the request forwarded upstream. A configured header
    /// absent from the auth response removes any client-supplied value.
    fn apply_allow(&self, ctx: &mut Context, resp_headers: &HashMap<String, Vec<String>>) {
        for name in &self.upstream_headers {
            match resp_headers.get(name) {
                Some(values) => {
                    ctx.request.headers.insert(name.clone(), values.clone());
                }
                None => {
                    ctx.request.headers.remove(name);
                }
            }
        }
    }

    /// On a non-2xx auth reply, mirrors the auth service's status and body onto
    /// the client-facing response, copies the configured `client_headers`, and
    /// returns the `FORWARD_AUTH_DENIED` rejection carrying the context.
    fn build_deny(
        &self,
        mut ctx: Context,
        status: u16,
        body: Bytes,
        resp_headers: &HashMap<String, Vec<String>>,
    ) -> PluginExecutionError {
        ctx.response.status_code = status;
        ctx.response.body = body;
        for name in &self.client_headers {
            if let Some(values) = resp_headers.get(name) {
                ctx.response.headers.insert(name.clone(), values.clone());
            }
        }
        PluginExecutionError {
            context: ctx,
            error: GatewayError {
                node_id: String::new(),
                code: "FORWARD_AUTH_DENIED".to_string(),
                message: format!("Authorization service denied the request ({})", status),
                metadata: HashMap::new(),
            },
        }
    }

    /// Builds the `FORWARD_AUTH_ERROR` rejection used when the callout fails
    /// and degradation is disabled.
    fn build_error(&self, mut ctx: Context, message: String) -> PluginExecutionError {
        ctx.response.status_code = self.status_on_error;
        PluginExecutionError {
            context: ctx,
            error: GatewayError {
                node_id: String::new(),
                code: "FORWARD_AUTH_ERROR".to_string(),
                message,
                metadata: HashMap::new(),
            },
        }
    }
}

#[async_trait]
impl Plugin for ForwardAuthPlugin {
    fn plugin_type(&self) -> &str {
        "forward-auth"
    }

    async fn execute(
        &self,
        mut ctx: Context,
        _named_inputs: &HashMap<String, serde_json::Value>,
    ) -> PluginResult {
        let headers = self.build_callout_headers(&ctx);
        let body = if self.is_post {
            ctx.request.body.clone()
        } else {
            Bytes::new()
        };

        let request = OutboundRequest {
            method: self.method.clone(),
            url: self.uri.clone(),
            headers,
            body,
            timeout: self.timeout,
            ssl_verify: self.ssl_verify,
        };

        let response: OutboundResponse = match self.client.request(request).await {
            Ok(resp) => resp,
            Err(e) => {
                if self.allow_degradation {
                    // Degrade open: let the request continue unchanged.
                    return Ok(PluginOutput {
                        context: ctx,
                        named_outputs: HashMap::new(),
                    });
                }
                return Err(self.build_error(ctx, format!("forward-auth callout failed: {}", e)));
            }
        };

        if response.status >= 300 {
            return Err(self.build_deny(ctx, response.status, response.body, &response.headers));
        }

        self.apply_allow(&mut ctx, &response.headers);
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
        headers.insert("authorization".to_string(), vec!["Bearer tok".to_string()]);
        headers.insert("content-encoding".to_string(), vec!["gzip".to_string()]);
        let mut query = HashMap::new();
        query.insert("q".to_string(), vec!["1".to_string()]);
        Context {
            request: GatewayRequest {
                method: "POST".to_string(),
                path: "/api/x".to_string(),
                host: "example.com".to_string(),
                scheme: "https".to_string(),
                headers,
                query_params: query,
                body: Bytes::from_static(b"payload"),
                remote_addr: "10.0.0.7:5555".to_string(),
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

    fn plugin(config: serde_json::Value) -> ForwardAuthPlugin {
        let map: HashMap<String, serde_json::Value> = serde_json::from_value(config).unwrap();
        ForwardAuthPlugin::from_config(&map, &PluginResources::empty()).unwrap()
    }

    #[test]
    fn test_requires_uri() {
        assert!(
            ForwardAuthPlugin::from_config(&HashMap::new(), &PluginResources::empty()).is_err()
        );
        // present but empty is also rejected
        let mut map = HashMap::new();
        map.insert("uri".to_string(), serde_json::json!(""));
        assert!(ForwardAuthPlugin::from_config(&map, &PluginResources::empty()).is_err());
    }

    #[test]
    fn test_rejects_bad_method() {
        let mut map = HashMap::new();
        map.insert("uri".to_string(), serde_json::json!("http://a"));
        map.insert("request_method".to_string(), serde_json::json!("PUT"));
        assert!(ForwardAuthPlugin::from_config(&map, &PluginResources::empty()).is_err());
    }

    #[test]
    fn test_defaults() {
        let p = plugin(serde_json::json!({ "uri": "http://auth" }));
        assert_eq!(p.method, http::Method::GET);
        assert!(!p.is_post);
        assert!(p.ssl_verify);
        assert_eq!(p.status_on_error, 403);
        assert!(!p.allow_degradation);
        assert_eq!(p.timeout, Duration::from_millis(3000));
    }

    #[test]
    fn test_build_callout_headers_forwarded_and_client() {
        let p = plugin(serde_json::json!({
            "uri": "http://auth",
            "request_method": "POST",
            "request_headers": ["Authorization"],
            "extra_headers": { "X-Src": "$remote_addr" }
        }));
        let headers = p.build_callout_headers(&test_ctx());
        let get = |name: &str| {
            headers
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(name))
                .map(|(_, v)| v.as_str())
        };
        assert_eq!(get("X-Forwarded-Proto"), Some("https"));
        assert_eq!(get("X-Forwarded-Method"), Some("POST"));
        assert_eq!(get("X-Forwarded-Host"), Some("example.com"));
        assert_eq!(get("X-Forwarded-Uri"), Some("/api/x?q=1"));
        assert_eq!(get("X-Forwarded-For"), Some("10.0.0.7"));
        // POST preserves content-encoding
        assert_eq!(get("Content-Encoding"), Some("gzip"));
        // configured client header copied
        assert_eq!(get("authorization"), Some("Bearer tok"));
        // extra header interpolated
        assert_eq!(get("X-Src"), Some("10.0.0.7"));
    }

    #[test]
    fn test_get_callout_omits_body_headers() {
        let p = plugin(serde_json::json!({ "uri": "http://auth" }));
        let headers = p.build_callout_headers(&test_ctx());
        // GET does not forward Content-Encoding
        assert!(!headers
            .iter()
            .any(|(k, _)| k.eq_ignore_ascii_case("content-encoding")));
    }

    #[test]
    fn test_apply_allow_sets_and_removes() {
        let p = plugin(serde_json::json!({
            "uri": "http://auth",
            "upstream_headers": ["X-User-Id", "X-Absent"]
        }));
        let mut ctx = test_ctx();
        ctx.request
            .headers
            .insert("x-absent".to_string(), vec!["stale".to_string()]);
        let mut resp_headers = HashMap::new();
        resp_headers.insert("x-user-id".to_string(), vec!["u42".to_string()]);
        p.apply_allow(&mut ctx, &resp_headers);
        assert_eq!(
            ctx.request.headers.get("x-user-id"),
            Some(&vec!["u42".to_string()])
        );
        // configured header absent from auth response -> client value removed
        assert!(!ctx.request.headers.contains_key("x-absent"));
    }

    #[test]
    fn test_build_deny_mirrors_status_body_and_headers() {
        let p = plugin(serde_json::json!({
            "uri": "http://auth",
            "client_headers": ["WWW-Authenticate"]
        }));
        let mut resp_headers = HashMap::new();
        resp_headers.insert("www-authenticate".to_string(), vec!["Bearer".to_string()]);
        let err = p.build_deny(
            test_ctx(),
            401,
            Bytes::from_static(b"denied"),
            &resp_headers,
        );
        assert_eq!(err.error.code, "FORWARD_AUTH_DENIED");
        assert_eq!(err.context.response.status_code, 401);
        assert_eq!(err.context.response.body, Bytes::from_static(b"denied"));
        assert_eq!(
            err.context.response.headers.get("www-authenticate"),
            Some(&vec!["Bearer".to_string()])
        );
    }
}
