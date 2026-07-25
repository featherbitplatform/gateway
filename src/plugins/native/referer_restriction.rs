//! Referer allow/deny list plugin (`referer-restriction`).
//!
//! Port of APISIX's `referer-restriction`: parses the host out of the
//! `Referer` request header and matches it against a whitelist or blacklist
//! of host patterns (exact hosts or leading-`*` wildcards). Rejections are
//! routed through the node's error port with error code `REFERER_RESTRICTED`.

use async_trait::async_trait;
use bytes::Bytes;
use std::collections::HashMap;

use crate::context::{Context, GatewayError};
use crate::plugins::{Plugin, PluginExecutionError, PluginOutput, PluginResult};

/// Restricts access based on the host of the `Referer` request header.
///
/// Exactly one of `whitelist` / `blacklist` is configured (APISIX `oneOf`
/// parity). A missing or malformed Referer is rejected unless
/// `bypass_missing` is set. Rejections produce a 403 JSON response.
pub struct RefererRestrictionPlugin {
    /// Host patterns that may pass; non-empty means whitelist mode.
    whitelist: HostMatcher,
    /// Host patterns that are rejected; non-empty means blacklist mode.
    blacklist: HostMatcher,
    /// Pass requests whose Referer is missing or malformed (default `false`).
    bypass_missing: bool,
    /// Body message for rejections.
    message: String,
}

/// Pre-split host patterns: exact hosts and `*`-prefix wildcard suffixes
/// (mirrors APISIX's `create_host_matcher`).
struct HostMatcher {
    /// Lowercased exact host names.
    exact: Vec<String>,
    /// Suffixes from `*`-prefixed patterns: `*.example.com` stores
    /// `.example.com` (so the bare apex `example.com` does NOT match).
    suffixes: Vec<String>,
}

impl HostMatcher {
    fn is_empty(&self) -> bool {
        self.exact.is_empty() && self.suffixes.is_empty()
    }

    /// True when `host` equals an exact entry or ends with a wildcard suffix.
    fn matches(&self, host: &str) -> bool {
        self.exact.iter().any(|h| h == host)
            || self.suffixes.iter().any(|s| host.ends_with(s.as_str()))
    }
}

/// Parses a config key as an array of host patterns. Patterns starting with
/// `*` become suffix matches on the remainder; others are exact (lowercased).
fn parse_host_list(
    config: &HashMap<String, serde_json::Value>,
    key: &str,
) -> Result<HostMatcher, String> {
    let mut matcher = HostMatcher {
        exact: Vec::new(),
        suffixes: Vec::new(),
    };
    let Some(raw) = config.get(key) else {
        return Ok(matcher);
    };
    let arr = raw
        .as_array()
        .ok_or_else(|| format!("{} must be an array of host patterns", key))?;
    for item in arr {
        let s = item
            .as_str()
            .ok_or_else(|| format!("{} entries must be strings", key))?;
        if s.is_empty() || s == "*" {
            return Err(format!(
                "{} entries must be hosts like example.com or *.example.com",
                key
            ));
        }
        if let Some(suffix) = s.strip_prefix('*') {
            matcher.suffixes.push(suffix.to_ascii_lowercase());
        } else {
            matcher.exact.push(s.to_ascii_lowercase());
        }
    }
    Ok(matcher)
}

/// Extracts the lowercased host from a Referer value. Only `http`/`https`
/// URLs parse (APISIX `http.parse_uri` parity); anything else — including a
/// bare host without a scheme — is treated as malformed and returns `None`.
fn referer_host(referer: &str) -> Option<String> {
    let (scheme, rest) = referer.split_once("://")?;
    if !scheme.eq_ignore_ascii_case("http") && !scheme.eq_ignore_ascii_case("https") {
        return None;
    }
    let end = rest.find([':', '/', '?', '#']).unwrap_or(rest.len());
    let host = &rest[..end];
    if host.is_empty() {
        None
    } else {
        Some(host.to_ascii_lowercase())
    }
}

impl RefererRestrictionPlugin {
    /// Builds the plugin from node config.
    ///
    /// Accepted keys:
    /// - `whitelist` (array of host patterns): only these Referer hosts pass.
    /// - `blacklist` (array of host patterns): these Referer hosts are
    ///   rejected. Exactly **one** of `whitelist` / `blacklist` must be
    ///   non-empty (APISIX `oneOf` parity). Patterns are exact hosts
    ///   (`example.com`) or leading-`*` wildcards (`*.example.com`, which
    ///   matches any subdomain but not the bare apex).
    /// - `bypass_missing` (bool, default `false`): pass requests whose
    ///   Referer header is missing or not a parseable http(s) URL.
    /// - `message` (string, default `"Your referer host is not allowed"`):
    ///   rejection message, returned as `{"message": ...}`.
    ///
    /// ```yaml
    /// type: referer-restriction
    /// config:
    ///   whitelist: ["example.com", "*.example.org"]
    ///   bypass_missing: true
    /// ```
    pub fn from_config(config: &HashMap<String, serde_json::Value>) -> Result<Self, String> {
        let whitelist = parse_host_list(config, "whitelist")?;
        let blacklist = parse_host_list(config, "blacklist")?;

        if !whitelist.is_empty() && !blacklist.is_empty() {
            return Err(
                "referer-restriction: whitelist and blacklist cannot both be set".to_string(),
            );
        }
        if whitelist.is_empty() && blacklist.is_empty() {
            return Err(
                "referer-restriction: exactly one of whitelist or blacklist must be non-empty"
                    .to_string(),
            );
        }

        Ok(Self {
            whitelist,
            blacklist,
            bypass_missing: config
                .get("bypass_missing")
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
            message: config
                .get("message")
                .and_then(|v| v.as_str())
                .unwrap_or("Your referer host is not allowed")
                .to_string(),
        })
    }

    /// Builds the 403 rejection routed through the error port with code
    /// `REFERER_RESTRICTED`.
    fn reject(&self, mut ctx: Context) -> PluginResult {
        ctx.response.status_code = 403;
        ctx.response.body = Bytes::from(serde_json::json!({ "message": self.message }).to_string());
        ctx.response.headers.insert(
            "content-type".to_string(),
            vec!["application/json".to_string()],
        );
        Err(PluginExecutionError {
            context: ctx,
            error: GatewayError {
                node_id: String::new(),
                code: "REFERER_RESTRICTED".to_string(),
                message: self.message.clone(),
                metadata: HashMap::new(),
            },
        })
    }
}

#[async_trait]
impl Plugin for RefererRestrictionPlugin {
    fn plugin_type(&self) -> &str {
        "referer-restriction"
    }

    async fn execute(
        &self,
        ctx: Context,
        _named_inputs: &HashMap<String, serde_json::Value>,
    ) -> PluginResult {
        let host = ctx
            .request
            .headers
            .get("referer")
            .and_then(|v| v.first())
            .and_then(|r| referer_host(r));

        let block = match host {
            // Missing or malformed Referer.
            None => !self.bypass_missing,
            Some(host) => {
                if !self.whitelist.is_empty() {
                    !self.whitelist.matches(&host)
                } else {
                    self.blacklist.matches(&host)
                }
            }
        };

        if block {
            return self.reject(ctx);
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

    fn test_context(referer: Option<&str>) -> Context {
        let mut headers = HashMap::new();
        if let Some(r) = referer {
            headers.insert("referer".to_string(), vec![r.to_string()]);
        }
        Context {
            request: GatewayRequest {
                method: "GET".to_string(),
                path: "/test".to_string(),
                host: "localhost".to_string(),
                scheme: "http".to_string(),
                headers,
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
    fn test_config_requires_exactly_one_list() {
        assert!(RefererRestrictionPlugin::from_config(&config(serde_json::json!({}))).is_err());
        assert!(
            RefererRestrictionPlugin::from_config(&config(serde_json::json!({
                "whitelist": ["a.com"], "blacklist": ["b.com"]
            })))
            .is_err()
        );
        assert!(
            RefererRestrictionPlugin::from_config(&config(serde_json::json!({
                "whitelist": ["a.com"]
            })))
            .is_ok()
        );
        // bad shapes
        assert!(
            RefererRestrictionPlugin::from_config(&config(serde_json::json!({
                "whitelist": "a.com"
            })))
            .is_err()
        );
        assert!(
            RefererRestrictionPlugin::from_config(&config(serde_json::json!({
                "whitelist": [""]
            })))
            .is_err()
        );
    }

    #[test]
    fn test_referer_host_parsing() {
        assert_eq!(
            referer_host("http://example.com/path"),
            Some("example.com".to_string())
        );
        assert_eq!(
            referer_host("https://Example.COM:8443?q=1"),
            Some("example.com".to_string())
        );
        assert_eq!(
            referer_host("https://example.com"),
            Some("example.com".to_string())
        );
        // malformed: no scheme, wrong scheme, empty host
        assert_eq!(referer_host("example.com/path"), None);
        assert_eq!(referer_host("ftp://example.com"), None);
        assert_eq!(referer_host("http://"), None);
    }

    #[tokio::test]
    async fn test_whitelist_exact_and_wildcard() {
        let plugin = RefererRestrictionPlugin::from_config(&config(serde_json::json!({
            "whitelist": ["example.com", "*.example.org"]
        })))
        .unwrap();

        assert!(plugin
            .execute(test_context(Some("http://example.com/x")), &HashMap::new())
            .await
            .is_ok());
        assert!(plugin
            .execute(
                test_context(Some("https://api.example.org/x")),
                &HashMap::new()
            )
            .await
            .is_ok());
        // apex does not match "*.example.org"
        assert!(plugin
            .execute(test_context(Some("https://example.org/")), &HashMap::new())
            .await
            .is_err());
        let err = plugin
            .execute(test_context(Some("https://evil.com/")), &HashMap::new())
            .await
            .unwrap_err();
        assert_eq!(err.error.code, "REFERER_RESTRICTED");
        assert_eq!(err.context.response.status_code, 403);
    }

    #[tokio::test]
    async fn test_blacklist_blocks_matching_host() {
        let plugin = RefererRestrictionPlugin::from_config(&config(serde_json::json!({
            "blacklist": ["*.evil.com", "bad.org"]
        })))
        .unwrap();

        assert!(plugin
            .execute(test_context(Some("http://sub.evil.com/")), &HashMap::new())
            .await
            .is_err());
        assert!(plugin
            .execute(test_context(Some("http://bad.org/")), &HashMap::new())
            .await
            .is_err());
        assert!(plugin
            .execute(test_context(Some("http://good.org/")), &HashMap::new())
            .await
            .is_ok());
        // blacklist mode: missing referer is still blocked by default
        assert!(plugin
            .execute(test_context(None), &HashMap::new())
            .await
            .is_err());
    }

    #[tokio::test]
    async fn test_bypass_missing_and_malformed() {
        let plugin = RefererRestrictionPlugin::from_config(&config(serde_json::json!({
            "whitelist": ["example.com"], "bypass_missing": true
        })))
        .unwrap();

        assert!(plugin
            .execute(test_context(None), &HashMap::new())
            .await
            .is_ok());
        // malformed referer counts as missing
        assert!(plugin
            .execute(test_context(Some("not a url")), &HashMap::new())
            .await
            .is_ok());

        let strict = RefererRestrictionPlugin::from_config(&config(serde_json::json!({
            "whitelist": ["example.com"]
        })))
        .unwrap();
        assert!(strict
            .execute(test_context(None), &HashMap::new())
            .await
            .is_err());
        assert!(strict
            .execute(test_context(Some("not a url")), &HashMap::new())
            .await
            .is_err());
    }
}
