//! CORS plugin (`cors`).
//!
//! Adds `Access-Control-*` response headers for allowed origins and
//! short-circuits `OPTIONS` preflight requests with a 204 response. Never
//! errors: disallowed origins simply pass through without CORS headers.

use async_trait::async_trait;
use bytes::Bytes;
use std::collections::HashMap;

use crate::context::Context;
use crate::plugins::{Plugin, PluginOutput, PluginResult};

/// Applies CORS response headers based on the request's `Origin` header.
///
/// For an allowed origin the plugin sets `access-control-allow-origin`
/// (echoing the origin, or `*` when wildcarded) and, when enabled,
/// `access-control-allow-credentials`. For preflight (`OPTIONS`) requests it
/// additionally sets the allow-methods/allow-headers/max-age headers and
/// short-circuits with a 204 empty response. Does not write to
/// `context.message` and always succeeds.
pub struct CorsPlugin {
    /// Origins granted CORS access; `"*"` matches any origin.
    allowed_origins: Vec<String>,
    /// Methods advertised in preflight responses.
    allowed_methods: Vec<String>,
    /// Request headers advertised in preflight responses; may be `"*"`.
    allowed_headers: Vec<String>,
    /// Preflight cache lifetime in seconds (`access-control-max-age`).
    max_age: u64,
    /// Whether to emit `access-control-allow-credentials: true`.
    allow_credentials: bool,
}

impl CorsPlugin {
    /// Builds the plugin from node config. Never fails; every key has a
    /// default.
    ///
    /// Accepted keys:
    /// - `allowed_origins` (array of strings, default `["*"]`)
    /// - `allowed_methods` (array of strings, default
    ///   `["GET", "POST", "PUT", "DELETE", "OPTIONS"]`)
    /// - `allowed_headers` (array of strings, default `["*"]`)
    /// - `max_age` (integer seconds, default `3600`)
    /// - `allow_credentials` (bool, default `false`)
    ///
    /// ```yaml
    /// type: cors
    /// config:
    ///   allowed_origins: ["https://app.example.com"]
    ///   allowed_methods: ["GET", "POST"]
    ///   max_age: 600
    ///   allow_credentials: true
    /// ```
    pub fn from_config(config: &HashMap<String, serde_json::Value>) -> Result<Self, String> {
        let allowed_origins = config
            .get("allowed_origins")
            .and_then(|v| v.as_array())
            .map(|seq| {
                seq.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_else(|| vec!["*".to_string()]);

        let allowed_methods = config
            .get("allowed_methods")
            .and_then(|v| v.as_array())
            .map(|seq| {
                seq.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_else(|| {
                vec![
                    "GET".to_string(),
                    "POST".to_string(),
                    "PUT".to_string(),
                    "DELETE".to_string(),
                    "OPTIONS".to_string(),
                ]
            });

        let allowed_headers = config
            .get("allowed_headers")
            .and_then(|v| v.as_array())
            .map(|seq| {
                seq.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_else(|| vec!["*".to_string()]);

        let max_age = config
            .get("max_age")
            .and_then(|v| v.as_u64())
            .unwrap_or(3600);

        let allow_credentials = config
            .get("allow_credentials")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        Ok(Self {
            allowed_origins,
            allowed_methods,
            allowed_headers,
            max_age,
            allow_credentials,
        })
    }

    /// Returns true when the origin exactly matches an allowed origin or the
    /// list contains the `"*"` wildcard.
    fn origin_allowed(&self, origin: &str) -> bool {
        self.allowed_origins.iter().any(|o| o == "*" || o == origin)
    }
}

#[async_trait]
impl Plugin for CorsPlugin {
    fn plugin_type(&self) -> &str {
        "cors"
    }

    async fn execute(
        &self,
        mut ctx: Context,
        _named_inputs: &HashMap<String, serde_json::Value>,
    ) -> PluginResult {
        let origin = ctx
            .request
            .headers
            .get("origin")
            .and_then(|v| v.first())
            .cloned()
            .unwrap_or_default();

        let is_preflight = ctx.request.method == "OPTIONS";

        if self.origin_allowed(&origin) {
            let resp_origin = if self.allowed_origins.iter().any(|o| o == "*") {
                "*".to_string()
            } else {
                origin
            };

            ctx.response
                .headers
                .insert("access-control-allow-origin".to_string(), vec![resp_origin]);

            if self.allow_credentials {
                ctx.response.headers.insert(
                    "access-control-allow-credentials".to_string(),
                    vec!["true".to_string()],
                );
            }

            if is_preflight {
                ctx.response.headers.insert(
                    "access-control-allow-methods".to_string(),
                    vec![self.allowed_methods.join(", ")],
                );
                ctx.response.headers.insert(
                    "access-control-allow-headers".to_string(),
                    vec![self.allowed_headers.join(", ")],
                );
                ctx.response.headers.insert(
                    "access-control-max-age".to_string(),
                    vec![self.max_age.to_string()],
                );
                // Short-circuit: return 204 for preflight
                ctx.response.status_code = 204;
                ctx.response.body = Bytes::new();
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
    //! Behavioral tests translated from Apache APISIX's `t/plugin/cors.t`,
    //! adapted to featherbit's config keys and its documented subset of the
    //! APISIX plugin (no regex origins, no `expose_headers`, no `**` force mode).
    //! The APISIX `=== TEST N` each scenario derives from is noted inline.
    use super::*;
    use crate::context::{GatewayRequest, GatewayResponse, Protocol};

    /// Builds a request context with the given method and optional Origin header.
    fn ctx(method: &str, origin: Option<&str>) -> Context {
        let mut headers = HashMap::new();
        if let Some(o) = origin {
            headers.insert("origin".to_string(), vec![o.to_string()]);
        }
        Context {
            request: GatewayRequest {
                method: method.to_string(),
                path: "/hello".to_string(),
                host: "h".to_string(),
                scheme: "http".to_string(),
                headers,
                query_params: HashMap::new(),
                body: Bytes::new(),
                remote_addr: "1.2.3.4:5".to_string(),
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

    fn plugin(config: serde_json::Value) -> CorsPlugin {
        let map: HashMap<String, serde_json::Value> =
            config.as_object().unwrap().clone().into_iter().collect();
        CorsPlugin::from_config(&map).unwrap()
    }

    /// First value of a response header, or None.
    fn hdr<'a>(ctx: &'a Context, name: &str) -> Option<&'a str> {
        ctx.response
            .headers
            .get(name)
            .and_then(|v| v.first())
            .map(String::as_str)
    }

    /// APISIX TEST 6-7: default config echoes `*` for any origin.
    #[tokio::test]
    async fn test_default_config_allows_any_origin() {
        let out = plugin(serde_json::json!({}))
            .execute(ctx("GET", Some("http://anything.example")), &HashMap::new())
            .await
            .unwrap();
        assert_eq!(hdr(&out.context, "access-control-allow-origin"), Some("*"));
    }

    /// APISIX TEST 8-9: a specific allowed origin is echoed back.
    #[tokio::test]
    async fn test_specific_origin_matched() {
        let out = plugin(serde_json::json!({
            "allowed_origins": ["http://sub.domain.com", "http://sub2.domain.com"]
        }))
        .execute(ctx("GET", Some("http://sub2.domain.com")), &HashMap::new())
        .await
        .unwrap();
        // The matched origin is echoed, not `*`.
        assert_eq!(
            hdr(&out.context, "access-control-allow-origin"),
            Some("http://sub2.domain.com")
        );
    }

    /// APISIX TEST 10: an origin not in the allowlist gets no CORS headers.
    #[tokio::test]
    async fn test_non_matching_origin_rejected() {
        let out = plugin(serde_json::json!({
            "allowed_origins": ["http://sub.domain.com"]
        }))
        .execute(ctx("GET", Some("http://evil.example")), &HashMap::new())
        .await
        .unwrap();
        assert_eq!(hdr(&out.context, "access-control-allow-origin"), None);
    }

    /// APISIX TEST 37: a request with no Origin header produces no CORS headers
    /// and no error.
    #[tokio::test]
    async fn test_no_origin_header_no_cors() {
        let out = plugin(serde_json::json!({
            "allowed_origins": ["http://sub.domain.com"]
        }))
        .execute(ctx("GET", None), &HashMap::new())
        .await
        .unwrap();
        assert_eq!(hdr(&out.context, "access-control-allow-origin"), None);
    }

    /// APISIX TEST 8-9: `allow_credentials` emits the credentials header.
    #[tokio::test]
    async fn test_allow_credentials_header() {
        let out = plugin(serde_json::json!({
            "allowed_origins": ["http://sub.domain.com"],
            "allow_credentials": true
        }))
        .execute(ctx("GET", Some("http://sub.domain.com")), &HashMap::new())
        .await
        .unwrap();
        assert_eq!(
            hdr(&out.context, "access-control-allow-credentials"),
            Some("true")
        );
    }

    /// APISIX TEST 14: an OPTIONS preflight on an allowed origin is answered with
    /// 204 and the advertised methods/headers/max-age.
    ///
    /// NOTE: this is the *plugin-level* contract — the plugin prepares the 204.
    /// End-to-end the graph engine still walks the success edge into `upstream`,
    /// so a real preflight is not short-circuited; that gap is tracked by the
    /// (expected-failure) `E2E-DP-09` e2e test, not here.
    #[tokio::test]
    async fn test_preflight_prepares_204() {
        let out = plugin(serde_json::json!({
            "allowed_origins": ["http://sub.domain.com"],
            "allowed_methods": ["GET", "POST"],
            "max_age": 50
        }))
        .execute(
            ctx("OPTIONS", Some("http://sub.domain.com")),
            &HashMap::new(),
        )
        .await
        .unwrap();
        assert_eq!(out.context.response.status_code, 204);
        assert_eq!(
            hdr(&out.context, "access-control-allow-methods"),
            Some("GET, POST")
        );
        assert_eq!(hdr(&out.context, "access-control-max-age"), Some("50"));
        assert!(out.context.response.body.is_empty());
    }

    /// A preflight for a *disallowed* origin is not short-circuited (no 204, no
    /// CORS headers) — it falls through untouched.
    #[tokio::test]
    async fn test_preflight_disallowed_origin_untouched() {
        let out = plugin(serde_json::json!({
            "allowed_origins": ["http://sub.domain.com"]
        }))
        .execute(ctx("OPTIONS", Some("http://evil.example")), &HashMap::new())
        .await
        .unwrap();
        assert_ne!(out.context.response.status_code, 204);
        assert_eq!(hdr(&out.context, "access-control-allow-origin"), None);
    }
}
