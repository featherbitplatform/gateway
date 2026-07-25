//! IP allow/deny list plugin (`ip-restriction`).
//!
//! Filters requests by the client's remote address against configured allow
//! and deny lists (exact IPs or CIDR blocks); denied clients receive a 403
//! error routed through the node's error port.

use async_trait::async_trait;
use bytes::Bytes;
use std::collections::HashMap;
use std::net::IpAddr;

use crate::context::{Context, GatewayError};
use crate::plugins::{Plugin, PluginExecutionError, PluginOutput, PluginResult};

/// Restricts access based on the request's `remote_addr`.
///
/// The deny list is evaluated first (a match rejects with error code
/// `IP_DENIED`); when the allow list is non-empty, the client IP must match
/// one of its entries or the request is rejected with `IP_NOT_ALLOWED`.
/// Rejections produce a 403 JSON response. Does not write to
/// `context.message`.
pub struct IpRestrictionPlugin {
    /// Allowed IPs/CIDRs; when non-empty, acts as a whitelist.
    allow: Vec<String>,
    /// Denied IPs/CIDRs; checked before the allow list.
    deny: Vec<String>,
}

impl IpRestrictionPlugin {
    /// Builds the plugin from node config. Never fails; both lists default to
    /// empty (which permits all traffic).
    ///
    /// Accepted keys:
    /// - `allow` (array of strings, default `[]`): IPs or CIDR blocks
    ///   (e.g. `"10.0.0.0/8"`) that may pass when the list is non-empty.
    /// - `deny` (array of strings, default `[]`): IPs or CIDR blocks that are
    ///   always rejected; takes precedence over `allow`.
    ///
    /// ```yaml
    /// type: ip-restriction
    /// config:
    ///   allow: ["10.0.0.0/8", "192.168.1.5"]
    ///   deny: ["10.1.2.3"]
    /// ```
    pub fn from_config(config: &HashMap<String, serde_json::Value>) -> Result<Self, String> {
        let allow = config
            .get("allow")
            .and_then(|v| v.as_array())
            .map(|seq| {
                seq.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();

        let deny = config
            .get("deny")
            .and_then(|v| v.as_array())
            .map(|seq| {
                seq.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();

        Ok(Self { allow, deny })
    }

    /// Checks whether `addr` (an IP, optionally with a `:port` suffix)
    /// matches any pattern — either an exact IP or a `net/bits` CIDR block
    /// (IPv4 and IPv6). Unparseable addresses never match.
    fn ip_matches(patterns: &[String], addr: &str) -> bool {
        let ip: IpAddr = match addr.parse() {
            Ok(ip) => ip,
            Err(_) => {
                // Try stripping port
                match addr.rsplit_once(':').and_then(|(ip, _)| ip.parse().ok()) {
                    Some(ip) => ip,
                    None => return false,
                }
            }
        };

        for pattern in patterns {
            if pattern == addr || pattern == &ip.to_string() {
                return true;
            }
            // Simple CIDR check
            if let Some((net, bits)) = pattern.split_once('/') {
                if let (Ok(net_ip), Ok(bits)) = (net.parse::<IpAddr>(), bits.parse::<u32>()) {
                    match (net_ip, ip) {
                        (IpAddr::V4(net), IpAddr::V4(addr)) => {
                            let mask = if bits == 0 { 0 } else { !0u32 << (32 - bits) };
                            if u32::from(net) & mask == u32::from(addr) & mask {
                                return true;
                            }
                        }
                        (IpAddr::V6(net), IpAddr::V6(addr)) => {
                            let mask = if bits == 0 { 0 } else { !0u128 << (128 - bits) };
                            if u128::from(net) & mask == u128::from(addr) & mask {
                                return true;
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
        false
    }
}

#[async_trait]
impl Plugin for IpRestrictionPlugin {
    fn plugin_type(&self) -> &str {
        "ip-restriction"
    }

    async fn execute(
        &self,
        ctx: Context,
        _named_inputs: &HashMap<String, serde_json::Value>,
    ) -> PluginResult {
        let addr = &ctx.request.remote_addr;

        // Deny list takes precedence
        if !self.deny.is_empty() && Self::ip_matches(&self.deny, addr) {
            let mut ctx = ctx;
            ctx.response.status_code = 403;
            ctx.response.body =
                Bytes::from(r#"{"error": "forbidden", "message": "IP address denied"}"#);
            ctx.response.headers.insert(
                "content-type".to_string(),
                vec!["application/json".to_string()],
            );
            return Err(PluginExecutionError {
                context: ctx,
                error: GatewayError {
                    node_id: String::new(),
                    code: "IP_DENIED".to_string(),
                    message: "IP address denied".to_string(),
                    metadata: HashMap::new(),
                },
            });
        }

        // If allow list is non-empty, IP must be in it
        if !self.allow.is_empty() && !Self::ip_matches(&self.allow, addr) {
            let mut ctx = ctx;
            ctx.response.status_code = 403;
            ctx.response.body =
                Bytes::from(r#"{"error": "forbidden", "message": "IP address not allowed"}"#);
            ctx.response.headers.insert(
                "content-type".to_string(),
                vec!["application/json".to_string()],
            );
            return Err(PluginExecutionError {
                context: ctx,
                error: GatewayError {
                    node_id: String::new(),
                    code: "IP_NOT_ALLOWED".to_string(),
                    message: "IP address not allowed".to_string(),
                    metadata: HashMap::new(),
                },
            });
        }

        Ok(PluginOutput {
            context: ctx,
            named_outputs: HashMap::new(),
        })
    }
}

#[cfg(test)]
mod tests {
    //! Behavioral tests translated from Apache APISIX's `t/plugin/ip-restriction.t`,
    //! adapted to featherbit's keys (`allow`/`deny` for APISIX's
    //! `whitelist`/`blacklist`). featherbit does not implement APISIX's custom
    //! `message` / `response_code` (its rejection is a fixed 403), so APISIX
    //! TEST 26-35 are deviations and are not mirrored.
    use super::*;
    use crate::context::{GatewayRequest, GatewayResponse, Protocol};

    fn ctx(remote_addr: &str) -> Context {
        Context {
            request: GatewayRequest {
                method: "GET".to_string(),
                path: "/hello".to_string(),
                host: "h".to_string(),
                scheme: "http".to_string(),
                headers: HashMap::new(),
                query_params: HashMap::new(),
                body: Bytes::new(),
                remote_addr: remote_addr.to_string(),
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

    fn plugin(config: serde_json::Value) -> IpRestrictionPlugin {
        let map: HashMap<String, serde_json::Value> =
            config.as_object().unwrap().clone().into_iter().collect();
        IpRestrictionPlugin::from_config(&map).unwrap()
    }

    /// APISIX TEST 8: an IP inside a whitelisted /24 CIDR is allowed.
    #[tokio::test]
    async fn test_allow_cidr_match() {
        let p = plugin(serde_json::json!({ "allow": ["127.0.0.0/24", "113.74.26.106"] }));
        assert!(p.execute(ctx("127.0.0.1:5"), &HashMap::new()).await.is_ok());
    }

    /// APISIX TEST 9: an exact whitelisted IP is allowed.
    #[tokio::test]
    async fn test_allow_exact_match() {
        let p = plugin(serde_json::json!({ "allow": ["127.0.0.0/24", "113.74.26.106"] }));
        assert!(p
            .execute(ctx("113.74.26.106:80"), &HashMap::new())
            .await
            .is_ok());
    }

    /// APISIX TEST 10: an IP not on the whitelist is rejected with 403.
    #[tokio::test]
    async fn test_allow_unlisted_rejected() {
        let p = plugin(serde_json::json!({ "allow": ["127.0.0.0/24"] }));
        let err = p
            .execute(ctx("114.114.114.114:5"), &HashMap::new())
            .await
            .unwrap_err();
        assert_eq!(err.error.code, "IP_NOT_ALLOWED");
        assert_eq!(err.context.response.status_code, 403);
    }

    /// APISIX TEST 13-14: a blacklisted IP (CIDR or exact) is rejected with 403.
    #[tokio::test]
    async fn test_deny_match_rejected() {
        let p = plugin(serde_json::json!({ "deny": ["127.0.0.0/24", "113.74.26.106"] }));
        let by_cidr = p
            .execute(ctx("127.0.0.1:5"), &HashMap::new())
            .await
            .unwrap_err();
        assert_eq!(by_cidr.error.code, "IP_DENIED");
        assert_eq!(by_cidr.context.response.status_code, 403);
        assert!(p
            .execute(ctx("113.74.26.106:5"), &HashMap::new())
            .await
            .is_err());
    }

    /// APISIX TEST 15: an IP not on the blacklist is allowed.
    #[tokio::test]
    async fn test_deny_unlisted_allowed() {
        let p = plugin(serde_json::json!({ "deny": ["127.0.0.0/24"] }));
        assert!(p
            .execute(ctx("114.114.114.114:5"), &HashMap::new())
            .await
            .is_ok());
    }

    /// APISIX TEST 22-23: IPv6 blacklisting, exact and by CIDR (fe80::/32).
    #[tokio::test]
    async fn test_deny_ipv6() {
        let p = plugin(serde_json::json!({ "deny": ["::1", "fe80::/32"] }));
        assert!(p.execute(ctx("::1"), &HashMap::new()).await.is_err()); // exact
        assert!(p.execute(ctx("fe80::1:1"), &HashMap::new()).await.is_err()); // in fe80::/32
    }

    /// APISIX TEST 21: an IPv4 client is allowed by an IPv6-only blacklist.
    #[tokio::test]
    async fn test_deny_ipv6_allows_ipv4() {
        let p = plugin(serde_json::json!({ "deny": ["::1", "fe80::/32"] }));
        assert!(p.execute(ctx("127.0.0.1:5"), &HashMap::new()).await.is_ok());
    }

    /// Deny takes precedence over allow when an IP is on both lists.
    #[tokio::test]
    async fn test_deny_precedes_allow() {
        let p = plugin(serde_json::json!({ "allow": ["10.0.0.1"], "deny": ["10.0.0.1"] }));
        let err = p
            .execute(ctx("10.0.0.1:5"), &HashMap::new())
            .await
            .unwrap_err();
        assert_eq!(err.error.code, "IP_DENIED");
    }
}
