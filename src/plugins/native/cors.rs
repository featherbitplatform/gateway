//! CORS plugin (`cors`).
//!
//! Adds `Access-Control-*` response headers for allowed origins and answers
//! `OPTIONS` preflight requests with a prepared 204, exiting through the
//! dedicated `preflight` port so the engine routes the response straight to
//! the client instead of continuing to `upstream`. Never errors: disallowed
//! origins simply pass through on `success` without CORS headers.

use async_trait::async_trait;
use bytes::Bytes;
use std::collections::HashMap;

use crate::context::Context;
use crate::plugins::{Plugin, PluginOutput, PluginResult};
use crate::vars::template::Template;

/// Applies CORS response headers based on the request's `Origin` header.
///
/// For an allowed origin the plugin sets `access-control-allow-origin`
/// (echoing the origin, or `*` when wildcarded) and, when enabled,
/// `access-control-allow-credentials`. For preflight (`OPTIONS`) requests it
/// additionally sets the allow-methods/allow-headers/max-age headers,
/// prepares a 204 empty response, and exits through the `preflight` port —
/// the policy must wire that port (typically straight to `client`) or
/// compilation rejects it. Does not write to `context.message` and always
/// succeeds.
pub struct CorsPlugin {
    /// Origins granted CORS access; `"*"` matches any origin. Never
    /// templated — semantic tokens (`*`/origin-echo) stay literal.
    allowed_origins: Vec<String>,
    /// Methods advertised in preflight responses. Values support
    /// `{{namespace.path}}` references (no legacy `$var` interpolation —
    /// these headers never supported it, so this sweep must not start).
    allowed_methods: Vec<Template>,
    /// Request headers advertised in preflight responses; may be `"*"`.
    /// Values support `{{namespace.path}}` references, same as
    /// `allowed_methods`.
    allowed_headers: Vec<Template>,
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
    /// - `allowed_origins` (array of strings, default `["*"]`) — never
    ///   templated; semantic tokens (`*`/origin-echo) stay literal.
    /// - `allowed_methods` (array of strings, default
    ///   `["GET", "POST", "PUT", "DELETE", "OPTIONS"]`); values support
    ///   `{{namespace.path}}` references.
    /// - `allowed_headers` (array of strings, default `["*"]`); values
    ///   support `{{namespace.path}}` references.
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

        // Discard warnings here — the compile-time walk (a later task)
        // reports well-formed-but-unknown references; execution must not.
        let allowed_methods: Vec<String> = config
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
        let allowed_methods = allowed_methods
            .into_iter()
            .map(|s| Template::parse(&s).0)
            .collect();

        let allowed_headers: Vec<String> = config
            .get("allowed_headers")
            .and_then(|v| v.as_array())
            .map(|seq| {
                seq.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_else(|| vec!["*".to_string()]);
        let allowed_headers = allowed_headers
            .into_iter()
            .map(|s| Template::parse(&s).0)
            .collect();

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
                let methods = self
                    .allowed_methods
                    .iter()
                    .map(|tmpl| tmpl.render(&ctx))
                    .collect::<Vec<_>>()
                    .join(", ");
                let headers = self
                    .allowed_headers
                    .iter()
                    .map(|tmpl| tmpl.render(&ctx))
                    .collect::<Vec<_>>()
                    .join(", ");
                ctx.response
                    .headers
                    .insert("access-control-allow-methods".to_string(), vec![methods]);
                ctx.response
                    .headers
                    .insert("access-control-allow-headers".to_string(), vec![headers]);
                ctx.response.headers.insert(
                    "access-control-max-age".to_string(),
                    vec![self.max_age.to_string()],
                );
                // Short-circuit: the 204 is fully prepared, exit on the
                // dedicated `preflight` port rather than continuing to
                // `success` (and from there to `upstream`).
                ctx.response.status_code = 204;
                ctx.response.body = Bytes::new();
                return Ok(PluginOutput::on_port(ctx, "preflight"));
            }
        }

        Ok(PluginOutput::success(ctx))
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
            .execute(ctx("GET", Some("http://anything.example")))
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
        .execute(ctx("GET", Some("http://sub2.domain.com")))
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
        .execute(ctx("GET", Some("http://evil.example")))
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
        .execute(ctx("GET", None))
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
        .execute(ctx("GET", Some("http://sub.domain.com")))
        .await
        .unwrap();
        assert_eq!(
            hdr(&out.context, "access-control-allow-credentials"),
            Some("true")
        );
    }

    /// APISIX TEST 14: an OPTIONS preflight on an allowed origin exits on the
    /// `preflight` port with the 204 fully prepared — the engine routes it
    /// away from upstream (E2E-DP-09 covers the end-to-end short-circuit).
    #[tokio::test]
    async fn test_preflight_exits_on_preflight_port() {
        let out = plugin(serde_json::json!({
            "allowed_origins": ["http://sub.domain.com"],
            "allowed_methods": ["GET", "POST"],
            "max_age": 50
        }))
        .execute(ctx("OPTIONS", Some("http://sub.domain.com")))
        .await
        .unwrap();
        assert_eq!(out.port, Some("preflight"));
        assert_eq!(out.context.response.status_code, 204);
        assert_eq!(hdr(&out.context, "access-control-allow-methods"), Some("GET, POST"));
        assert_eq!(hdr(&out.context, "access-control-max-age"), Some("50"));
        assert!(out.context.response.body.is_empty());
    }

    /// Non-preflight requests and disallowed origins stay on success.
    #[tokio::test]
    async fn test_non_preflight_stays_on_success() {
        let out = plugin(serde_json::json!({}))
            .execute(ctx("GET", Some("http://x.example")))
            .await
            .unwrap();
        assert_eq!(out.port, None);
    }

    /// `allowed_methods`/`allowed_headers` values render `{{namespace.path}}`
    /// references per request.
    #[tokio::test]
    async fn test_preflight_headers_and_methods_render_template() {
        let out = plugin(serde_json::json!({
            "allowed_origins": ["http://sub.domain.com"],
            "allowed_methods": ["GET", "{{request.headers.x-extra-method}}"],
            "allowed_headers": ["{{request.headers.x-extra-header}}"]
        }))
        .execute({
            let mut c = ctx("OPTIONS", Some("http://sub.domain.com"));
            c.request
                .headers
                .insert("x-extra-method".to_string(), vec!["PATCH".to_string()]);
            c.request
                .headers
                .insert("x-extra-header".to_string(), vec!["x-custom".to_string()]);
            c
        })
        .await
        .unwrap();
        assert_eq!(
            hdr(&out.context, "access-control-allow-methods"),
            Some("GET, PATCH")
        );
        assert_eq!(
            hdr(&out.context, "access-control-allow-headers"),
            Some("x-custom")
        );
    }

    /// A preflight for a *disallowed* origin is not short-circuited (no 204, no
    /// CORS headers) — it falls through on success untouched.
    #[tokio::test]
    async fn test_preflight_disallowed_origin_untouched() {
        let out = plugin(serde_json::json!({
            "allowed_origins": ["http://sub.domain.com"]
        }))
        .execute(ctx("OPTIONS", Some("http://evil.example")))
        .await
        .unwrap();
        assert_eq!(out.port, None);
        assert_ne!(out.context.response.status_code, 204);
        assert_eq!(hdr(&out.context, "access-control-allow-origin"), None);
    }
}
