//! The `redirect` node — answers the request with an HTTP redirect instead of
//! proxying it upstream.
//!
//! Port of APISIX's `redirect` plugin (the `uri` and `http_to_https` modes;
//! APISIX's `regex_uri` and `encode_uri` are not implemented, and
//! `http_to_https` always redirects to the default HTTPS port — there is no
//! `https_port` plugin attribute).
//!
//! Redirecting *stops* the pipeline in featherbit terms: the plugin fills in
//! `context.response` (status + `location`) and returns success, so the
//! node's `success` edge should go straight to `client.in` — not through an
//! `upstream` node, which would overwrite the response.

use async_trait::async_trait;
use bytes::Bytes;
use std::collections::HashMap;

use crate::context::Context;
use crate::plugins::{Plugin, PluginOutput, PluginResult};
use crate::vars::{interpolate, resolve};

/// Builds a redirect response from either a `uri` template (with `$var`
/// interpolation) or the `http_to_https` shortcut.
///
/// This plugin never fails at execution time. The only case where it does
/// **not** redirect is `http_to_https` on a request that is already HTTPS
/// (per `x-forwarded-proto` or the request scheme): the context passes
/// through unchanged, so `http_to_https` should only be wired into routes
/// served over plain HTTP.
pub struct RedirectPlugin {
    /// Redirect plain-HTTP requests to `https://$host$request_uri`.
    http_to_https: bool,
    /// Redirect target template; `$var` / `${var}` are interpolated.
    uri: Option<String>,
    /// Status code for `uri` redirects (`http_to_https` picks 301/308 itself).
    ret_code: u16,
    /// Append the original query string to the target.
    append_query_string: bool,
}

impl RedirectPlugin {
    /// Builds the plugin from node config.
    ///
    /// Accepted keys — exactly one of `uri` / `http_to_https: true` is
    /// required:
    /// - `uri` (string): redirect target template. `$var` and `${var}`
    ///   references are interpolated against the context (see
    ///   [`crate::vars::interpolate`]), e.g. `/new$request_uri` or
    ///   `https://$host/login`. Unknown variables resolve to `""`.
    /// - `http_to_https` (bool): redirect HTTP requests to
    ///   `https://$host$request_uri` with `301` for GET/HEAD and `308`
    ///   otherwise (so the method and body survive). Already-HTTPS requests
    ///   pass through unchanged.
    /// - `ret_code` (integer, default `302`, minimum `200`): status code for
    ///   `uri` redirects.
    /// - `append_query_string` (bool, default `false`): append the original
    ///   query string to the target (`?` or `&` as appropriate). Cannot be
    ///   combined with `http_to_https`, which already keeps the query string
    ///   via `$request_uri`.
    ///
    /// ```yaml
    /// type: redirect
    /// config:
    ///   uri: https://$host/new-prefix$uri
    ///   ret_code: 301
    ///   append_query_string: true
    /// ```
    pub fn from_config(config: &HashMap<String, serde_json::Value>) -> Result<Self, String> {
        let http_to_https = config
            .get("http_to_https")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        let uri = config.get("uri").and_then(|v| v.as_str()).map(String::from);

        if http_to_https == uri.is_some() {
            return Err(
                "redirect plugin requires exactly one of 'uri' or 'http_to_https: true'"
                    .to_string(),
            );
        }

        let ret_code = match config.get("ret_code") {
            None => 302,
            Some(v) => {
                let code = v
                    .as_u64()
                    .filter(|c| (200..600).contains(c))
                    .ok_or("ret_code must be an integer status code >= 200")?;
                code as u16
            }
        };

        let append_query_string = config
            .get("append_query_string")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        if http_to_https && append_query_string {
            return Err(
                "only one of 'http_to_https' and 'append_query_string' can be configured"
                    .to_string(),
            );
        }

        Ok(Self {
            http_to_https,
            uri,
            ret_code,
            append_query_string,
        })
    }
}

#[async_trait]
impl Plugin for RedirectPlugin {
    fn plugin_type(&self) -> &str {
        "redirect"
    }

    async fn execute(
        &self,
        mut ctx: Context,
        _named_inputs: &HashMap<String, serde_json::Value>,
    ) -> PluginResult {
        let (new_uri, ret_code) = if self.http_to_https {
            // Honor x-forwarded-proto from an outer proxy, like APISIX.
            let scheme = ctx
                .request
                .headers
                .get("x-forwarded-proto")
                .and_then(|v| v.first())
                .cloned()
                .unwrap_or_else(|| ctx.request.scheme.clone());

            if scheme == "https" {
                // Already secure: pass through untouched.
                return Ok(PluginOutput {
                    context: ctx,
                    named_outputs: HashMap::new(),
                });
            }

            let ret_code = match ctx.request.method.as_str() {
                "GET" | "HEAD" => 301,
                // 308 keeps the method and body across the redirect.
                _ => 308,
            };
            (interpolate(&ctx, "https://$host$request_uri"), ret_code)
        } else {
            // from_config guarantees `uri` is set in this branch.
            let template = self.uri.as_deref().unwrap_or("");
            let mut new_uri = interpolate(&ctx, template);

            if self.append_query_string {
                if let Some(qs) = resolve(&ctx, "query_string") {
                    let sep = if new_uri.contains('?') { '&' } else { '?' };
                    new_uri.push(sep);
                    new_uri.push_str(&qs);
                }
            }
            (new_uri, self.ret_code)
        };

        ctx.response.status_code = ret_code;
        ctx.response
            .headers
            .insert("location".to_string(), vec![new_uri]);
        // The redirect body is empty; drop stale body-metadata headers per
        // the body-mutation convention.
        ctx.response.body = Bytes::new();
        ctx.response.headers.remove("content-length");
        ctx.response.headers.remove("content-encoding");

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

    fn test_context(path: &str) -> Context {
        Context {
            request: GatewayRequest {
                method: "GET".to_string(),
                path: path.to_string(),
                host: "example.com".to_string(),
                scheme: "http".to_string(),
                headers: HashMap::new(),
                query_params: HashMap::new(),
                body: Bytes::new(),
                remote_addr: "127.0.0.1:12345".to_string(),
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

    fn config(json: serde_json::Value) -> HashMap<String, serde_json::Value> {
        serde_json::from_value(json).unwrap()
    }

    #[test]
    fn test_redirect_config_validation() {
        // neither mode configured
        assert!(RedirectPlugin::from_config(&HashMap::new()).is_err());
        // both modes configured
        assert!(RedirectPlugin::from_config(&config(serde_json::json!({
            "uri": "/new",
            "http_to_https": true
        })))
        .is_err());
        // http_to_https: false does not count as choosing the mode
        assert!(RedirectPlugin::from_config(&config(serde_json::json!({
            "http_to_https": false
        })))
        .is_err());
        // http_to_https + append_query_string is rejected (APISIX parity)
        assert!(RedirectPlugin::from_config(&config(serde_json::json!({
            "http_to_https": true,
            "append_query_string": true
        })))
        .is_err());
        // ret_code below 200 rejected
        assert!(RedirectPlugin::from_config(&config(serde_json::json!({
            "uri": "/new",
            "ret_code": 100
        })))
        .is_err());
        // valid configs
        assert!(RedirectPlugin::from_config(&config(serde_json::json!({"uri": "/new"}))).is_ok());
        assert!(
            RedirectPlugin::from_config(&config(serde_json::json!({"http_to_https": true})))
                .is_ok()
        );
    }

    #[tokio::test]
    async fn test_redirect_uri_template() {
        let plugin = RedirectPlugin::from_config(&config(serde_json::json!({
            "uri": "https://$host/moved$uri"
        })))
        .unwrap();

        let result = plugin
            .execute(test_context("/old/path"), &HashMap::new())
            .await
            .unwrap();
        let ctx = result.context;
        assert_eq!(ctx.response.status_code, 302); // default ret_code
        assert_eq!(
            ctx.response.headers.get("location"),
            Some(&vec!["https://example.com/moved/old/path".to_string()])
        );
        assert!(ctx.response.body.is_empty());
    }

    #[tokio::test]
    async fn test_redirect_custom_ret_code() {
        let plugin = RedirectPlugin::from_config(&config(serde_json::json!({
            "uri": "/new",
            "ret_code": 301
        })))
        .unwrap();

        let result = plugin
            .execute(test_context("/old"), &HashMap::new())
            .await
            .unwrap();
        assert_eq!(result.context.response.status_code, 301);
    }

    #[tokio::test]
    async fn test_redirect_append_query_string() {
        let plugin = RedirectPlugin::from_config(&config(serde_json::json!({
            "uri": "/new",
            "append_query_string": true
        })))
        .unwrap();

        let mut ctx = test_context("/old");
        ctx.request
            .query_params
            .insert("a".to_string(), vec!["1".to_string()]);
        let result = plugin.execute(ctx, &HashMap::new()).await.unwrap();
        assert_eq!(
            result.context.response.headers.get("location"),
            Some(&vec!["/new?a=1".to_string()])
        );

        // Target already has a query string: append with '&'.
        let plugin = RedirectPlugin::from_config(&config(serde_json::json!({
            "uri": "/new?x=y",
            "append_query_string": true
        })))
        .unwrap();
        let mut ctx = test_context("/old");
        ctx.request
            .query_params
            .insert("a".to_string(), vec!["1".to_string()]);
        let result = plugin.execute(ctx, &HashMap::new()).await.unwrap();
        assert_eq!(
            result.context.response.headers.get("location"),
            Some(&vec!["/new?x=y&a=1".to_string()])
        );

        // No query params: nothing appended.
        let result = plugin
            .execute(test_context("/old"), &HashMap::new())
            .await
            .unwrap();
        assert_eq!(
            result.context.response.headers.get("location"),
            Some(&vec!["/new?x=y".to_string()])
        );
    }

    #[tokio::test]
    async fn test_redirect_http_to_https() {
        let plugin = RedirectPlugin::from_config(&config(serde_json::json!({
            "http_to_https": true
        })))
        .unwrap();

        // GET -> 301, query string preserved via $request_uri
        let mut ctx = test_context("/path");
        ctx.request
            .query_params
            .insert("a".to_string(), vec!["1".to_string()]);
        let result = plugin.execute(ctx, &HashMap::new()).await.unwrap();
        assert_eq!(result.context.response.status_code, 301);
        assert_eq!(
            result.context.response.headers.get("location"),
            Some(&vec!["https://example.com/path?a=1".to_string()])
        );

        // Non-GET/HEAD -> 308
        let mut ctx = test_context("/path");
        ctx.request.method = "POST".to_string();
        let result = plugin.execute(ctx, &HashMap::new()).await.unwrap();
        assert_eq!(result.context.response.status_code, 308);
    }

    #[tokio::test]
    async fn test_redirect_http_to_https_already_https_passthrough() {
        let plugin = RedirectPlugin::from_config(&config(serde_json::json!({
            "http_to_https": true
        })))
        .unwrap();

        // Scheme https
        let mut ctx = test_context("/path");
        ctx.request.scheme = "https".to_string();
        let result = plugin.execute(ctx, &HashMap::new()).await.unwrap();
        assert_eq!(result.context.response.status_code, 0);
        assert!(!result.context.response.headers.contains_key("location"));

        // x-forwarded-proto https from an outer proxy wins over the scheme
        let mut ctx = test_context("/path");
        ctx.request
            .headers
            .insert("x-forwarded-proto".to_string(), vec!["https".to_string()]);
        let result = plugin.execute(ctx, &HashMap::new()).await.unwrap();
        assert_eq!(result.context.response.status_code, 0);
    }
}
