//! The `real-ip` node — rewrites the client address seen by the rest of the
//! pipeline (`context.request.remote_addr`) from a request variable such as
//! `http_x_forwarded_for` or `http_x_real_ip`.
//!
//! Port of APISIX's `real-ip` plugin. The rewrite only happens when the
//! *direct* peer address matches `trusted_addresses` (when configured), so a
//! client cannot spoof its IP unless the request came through a trusted
//! proxy. This plugin never fails at execution time: any non-match, missing
//! header, or unparsable address is a silent passthrough.

use async_trait::async_trait;
use std::collections::HashMap;
use std::net::IpAddr;

use ipnet::IpNet;

use crate::context::Context;
use crate::plugins::{Plugin, PluginOutput, PluginResult};

/// Replaces `context.request.remote_addr` with the address carried by a
/// configured variable, guarded by a trusted-proxy allowlist.
///
/// `source: http_x_forwarded_for` gets APISIX's special `X-Forwarded-For`
/// handling (last header value, comma-splitting, optional recursive walk);
/// any other `source` is resolved through the standard variable resolver
/// ([`crate::vars::resolve`]), e.g. `http_x_real_ip` or `arg_realip`.
pub struct RealIpPlugin {
    /// Variable name the real address is read from.
    source: String,
    /// Only rewrite when the direct peer matches one of these networks.
    /// `None` means "always rewrite" (mirrors APISIX, where the field is
    /// optional).
    trusted_addresses: Option<Vec<IpNet>>,
    /// X-Forwarded-For only: walk the list right-to-left, skipping trusted
    /// hops, instead of taking the last (rightmost) entry.
    recursive: bool,
}

/// Parses `ip`, `ip:port`, `[v6]` or `[v6]:port`. Returns `None` for
/// unparsable addresses and out-of-range ports (`0` is rejected, matching
/// APISIX's port validation).
fn parse_ip_port(addr: &str) -> Option<(IpAddr, Option<u16>)> {
    let addr = addr.trim();

    // Bracketed IPv6: [::1] or [::1]:8080
    if let Some(rest) = addr.strip_prefix('[') {
        let (ip, tail) = rest.split_once(']')?;
        let ip: IpAddr = ip.parse().ok()?;
        return match tail.strip_prefix(':') {
            Some(p) => {
                let port = p.parse::<u16>().ok().filter(|p| *p > 0)?;
                Some((ip, Some(port)))
            }
            None if tail.is_empty() => Some((ip, None)),
            None => None,
        };
    }

    // Bare IPv4 or bare (unbracketed) IPv6.
    if let Ok(ip) = addr.parse::<IpAddr>() {
        return Some((ip, None));
    }

    // ip:port — an unbracketed IPv6 with a port is ambiguous, reject it.
    let (ip, port) = addr.rsplit_once(':')?;
    if ip.contains(':') {
        return None;
    }
    let ip: IpAddr = ip.parse().ok()?;
    let port = port.parse::<u16>().ok().filter(|p| *p > 0)?;
    Some((ip, Some(port)))
}

impl RealIpPlugin {
    /// Builds the plugin from node config.
    ///
    /// Accepted keys:
    /// - `source` (string, **required**): variable holding the real address,
    ///   e.g. `http_x_real_ip` or `http_x_forwarded_for` (the latter gets
    ///   comma-list handling). See [`crate::vars::resolve`] for the variable
    ///   namespace.
    /// - `trusted_addresses` (array of IPs/CIDRs, optional): the rewrite only
    ///   applies when the direct peer address matches one of these. When
    ///   omitted the rewrite always applies. An empty array or an invalid
    ///   IP/CIDR is a config error.
    /// - `recursive` (bool, default `false`): for `http_x_forwarded_for` with
    ///   `trusted_addresses`, walk the list from the rightmost entry, skip
    ///   trusted hops, and take the first untrusted address (falling back to
    ///   the leftmost entry when every hop is trusted). When `false`, the
    ///   last (rightmost) entry is used.
    ///
    /// ```yaml
    /// type: real-ip
    /// config:
    ///   source: http_x_forwarded_for
    ///   trusted_addresses: ["127.0.0.0/24", "10.0.0.0/8"]
    ///   recursive: true
    /// ```
    pub fn from_config(config: &HashMap<String, serde_json::Value>) -> Result<Self, String> {
        let source = config
            .get("source")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(String::from)
            .ok_or("real-ip plugin requires 'source' (a variable name, e.g. http_x_real_ip)")?;

        let trusted_addresses = match config.get("trusted_addresses") {
            None => None,
            Some(raw) => {
                let items = raw
                    .as_array()
                    .ok_or("trusted_addresses must be an array of IPs/CIDRs")?;
                if items.is_empty() {
                    return Err("trusted_addresses must contain at least one IP/CIDR".to_string());
                }
                let nets = items
                    .iter()
                    .map(|item| {
                        let s = item
                            .as_str()
                            .ok_or("trusted_addresses items must be strings")?;
                        if let Ok(net) = s.parse::<IpNet>() {
                            Ok(net)
                        } else if let Ok(ip) = s.parse::<IpAddr>() {
                            Ok(IpNet::from(ip))
                        } else {
                            Err(format!("invalid ip address: {}", s))
                        }
                    })
                    .collect::<Result<Vec<_>, String>>()?;
                Some(nets)
            }
        };

        let recursive = config
            .get("recursive")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        Ok(Self {
            source,
            trusted_addresses,
            recursive,
        })
    }

    /// True when `ip` is inside one of the trusted networks. Always false
    /// when no `trusted_addresses` are configured (callers guard on that).
    fn is_trusted(&self, ip: IpAddr) -> bool {
        self.trusted_addresses
            .as_ref()
            .is_some_and(|nets| nets.iter().any(|net| net.contains(&ip)))
    }

    /// Extracts the candidate real address from the configured source,
    /// mirroring APISIX's `get_addr`.
    fn get_addr(&self, ctx: &Context) -> Option<String> {
        if self.source == "http_x_forwarded_for" {
            // APISIX reads the *last* X-Forwarded-For header value when the
            // header repeats, then splits that value on commas.
            let value = ctx.request.headers.get("x-forwarded-for")?.last()?;
            let parts: Vec<&str> = value.split(',').map(str::trim).collect();

            if parts.len() == 1 {
                return Some(parts[0].to_string());
            }

            if self.recursive && self.trusted_addresses.is_some() {
                // Walk right-to-left (excluding the leftmost entry), taking
                // the first hop that is not a trusted proxy; an unparsable
                // entry counts as untrusted, matching APISIX's matcher.
                for part in parts[1..].iter().rev() {
                    let trusted = part.parse::<IpAddr>().is_ok_and(|ip| self.is_trusted(ip));
                    if !trusted {
                        return Some(part.to_string());
                    }
                }
                return Some(parts[0].to_string());
            }

            // Non-recursive: the last (rightmost) entry.
            return parts.last().map(|s| s.to_string());
        }

        crate::vars::resolve(ctx, &self.source).map(|v| v.into_owned())
    }
}

#[async_trait]
impl Plugin for RealIpPlugin {
    fn plugin_type(&self) -> &str {
        "real-ip"
    }

    async fn execute(
        &self,
        mut ctx: Context,
        _named_inputs: &HashMap<String, serde_json::Value>,
    ) -> PluginResult {
        let passthrough = |ctx: Context| {
            Ok(PluginOutput {
                context: ctx,
                named_outputs: HashMap::new(),
            })
        };

        // Only rewrite when the DIRECT peer is a trusted proxy.
        let direct = parse_ip_port(&ctx.request.remote_addr);
        if self.trusted_addresses.is_some() {
            match direct {
                Some((ip, _)) if self.is_trusted(ip) => {}
                _ => return passthrough(ctx),
            }
        }

        let Some(addr) = self.get_addr(&ctx) else {
            return passthrough(ctx);
        };

        let Some((ip, port)) = parse_ip_port(&addr) else {
            // Bad address in the source variable: leave remote_addr alone.
            return passthrough(ctx);
        };

        // Keep the original peer port when the source carries none.
        let port = port.or_else(|| direct.and_then(|(_, p)| p));
        ctx.request.remote_addr = match (ip, port) {
            (IpAddr::V6(v6), Some(p)) => format!("[{}]:{}", v6, p),
            (ip, Some(p)) => format!("{}:{}", ip, p),
            (ip, None) => ip.to_string(),
        };

        passthrough(ctx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::{GatewayRequest, GatewayResponse, Protocol};
    use bytes::Bytes;

    fn test_context(remote_addr: &str) -> Context {
        Context {
            request: GatewayRequest {
                method: "GET".to_string(),
                path: "/".to_string(),
                host: "localhost".to_string(),
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

    fn config(json: serde_json::Value) -> HashMap<String, serde_json::Value> {
        serde_json::from_value(json).unwrap()
    }

    #[test]
    fn test_real_ip_config_validation() {
        // source is required
        assert!(RealIpPlugin::from_config(&HashMap::new()).is_err());

        // invalid trusted address rejected at config load
        assert!(RealIpPlugin::from_config(&config(serde_json::json!({
            "source": "http_x_real_ip",
            "trusted_addresses": ["not-an-ip"]
        })))
        .is_err());

        // empty trusted_addresses rejected (APISIX minItems: 1)
        assert!(RealIpPlugin::from_config(&config(serde_json::json!({
            "source": "http_x_real_ip",
            "trusted_addresses": []
        })))
        .is_err());

        // valid config accepted
        assert!(RealIpPlugin::from_config(&config(serde_json::json!({
            "source": "http_x_forwarded_for",
            "trusted_addresses": ["10.0.0.0/8", "127.0.0.1"],
            "recursive": true
        })))
        .is_ok());
    }

    #[tokio::test]
    async fn test_real_ip_rewrites_from_x_real_ip() {
        let plugin = RealIpPlugin::from_config(&config(serde_json::json!({
            "source": "http_x_real_ip",
            "trusted_addresses": ["127.0.0.0/24"]
        })))
        .unwrap();

        let mut ctx = test_context("127.0.0.1:5000");
        ctx.request
            .headers
            .insert("x-real-ip".to_string(), vec!["203.0.113.7".to_string()]);

        let result = plugin.execute(ctx, &HashMap::new()).await.unwrap();
        // Source had no port: the original peer port is kept.
        assert_eq!(result.context.request.remote_addr, "203.0.113.7:5000");
    }

    #[tokio::test]
    async fn test_real_ip_untrusted_peer_is_passthrough() {
        let plugin = RealIpPlugin::from_config(&config(serde_json::json!({
            "source": "http_x_real_ip",
            "trusted_addresses": ["127.0.0.0/24"]
        })))
        .unwrap();

        let mut ctx = test_context("198.51.100.9:5000");
        ctx.request
            .headers
            .insert("x-real-ip".to_string(), vec!["203.0.113.7".to_string()]);

        let result = plugin.execute(ctx, &HashMap::new()).await.unwrap();
        assert_eq!(result.context.request.remote_addr, "198.51.100.9:5000");
    }

    #[tokio::test]
    async fn test_real_ip_missing_or_bad_source_is_passthrough() {
        let plugin = RealIpPlugin::from_config(&config(serde_json::json!({
            "source": "http_x_real_ip",
            "trusted_addresses": ["127.0.0.0/24"]
        })))
        .unwrap();

        // Header absent
        let ctx = test_context("127.0.0.1:5000");
        let result = plugin.execute(ctx, &HashMap::new()).await.unwrap();
        assert_eq!(result.context.request.remote_addr, "127.0.0.1:5000");

        // Header present but not an IP
        let mut ctx = test_context("127.0.0.1:5000");
        ctx.request
            .headers
            .insert("x-real-ip".to_string(), vec!["unknown".to_string()]);
        let result = plugin.execute(ctx, &HashMap::new()).await.unwrap();
        assert_eq!(result.context.request.remote_addr, "127.0.0.1:5000");
    }

    #[tokio::test]
    async fn test_real_ip_xff_non_recursive_takes_last() {
        let plugin = RealIpPlugin::from_config(&config(serde_json::json!({
            "source": "http_x_forwarded_for",
            "trusted_addresses": ["127.0.0.0/24"]
        })))
        .unwrap();

        let mut ctx = test_context("127.0.0.1:5000");
        ctx.request.headers.insert(
            "x-forwarded-for".to_string(),
            vec!["203.0.113.7, 10.1.1.1, 10.2.2.2".to_string()],
        );

        let result = plugin.execute(ctx, &HashMap::new()).await.unwrap();
        assert_eq!(result.context.request.remote_addr, "10.2.2.2:5000");
    }

    #[tokio::test]
    async fn test_real_ip_xff_recursive_skips_trusted_hops() {
        let plugin = RealIpPlugin::from_config(&config(serde_json::json!({
            "source": "http_x_forwarded_for",
            "trusted_addresses": ["127.0.0.0/24", "10.0.0.0/8"],
            "recursive": true
        })))
        .unwrap();

        // Rightmost hops are trusted proxies; the first untrusted one from
        // the right is the client address.
        let mut ctx = test_context("127.0.0.1:5000");
        ctx.request.headers.insert(
            "x-forwarded-for".to_string(),
            vec!["203.0.113.7, 10.1.1.1, 10.2.2.2".to_string()],
        );
        let result = plugin.execute(ctx, &HashMap::new()).await.unwrap();
        assert_eq!(result.context.request.remote_addr, "203.0.113.7:5000");

        // Every hop trusted: fall back to the leftmost entry.
        let mut ctx = test_context("127.0.0.1:5000");
        ctx.request.headers.insert(
            "x-forwarded-for".to_string(),
            vec!["10.9.9.9, 10.1.1.1".to_string()],
        );
        let result = plugin.execute(ctx, &HashMap::new()).await.unwrap();
        assert_eq!(result.context.request.remote_addr, "10.9.9.9:5000");
    }

    #[tokio::test]
    async fn test_real_ip_source_port_wins_over_peer_port() {
        let plugin = RealIpPlugin::from_config(&config(serde_json::json!({
            "source": "http_x_real_ip"
        })))
        .unwrap();

        // No trusted_addresses: rewrite always applies (APISIX semantics).
        let mut ctx = test_context("127.0.0.1:5000");
        ctx.request.headers.insert(
            "x-real-ip".to_string(),
            vec!["203.0.113.7:8443".to_string()],
        );
        let result = plugin.execute(ctx, &HashMap::new()).await.unwrap();
        assert_eq!(result.context.request.remote_addr, "203.0.113.7:8443");
    }

    #[tokio::test]
    async fn test_real_ip_ipv6_source() {
        let plugin = RealIpPlugin::from_config(&config(serde_json::json!({
            "source": "http_x_real_ip",
            "trusted_addresses": ["127.0.0.0/24"]
        })))
        .unwrap();

        let mut ctx = test_context("127.0.0.1:5000");
        ctx.request.headers.insert(
            "x-real-ip".to_string(),
            vec!["[2001:db8::1]:9000".to_string()],
        );
        let result = plugin.execute(ctx, &HashMap::new()).await.unwrap();
        assert_eq!(result.context.request.remote_addr, "[2001:db8::1]:9000");

        let mut ctx = test_context("127.0.0.1:5000");
        ctx.request
            .headers
            .insert("x-real-ip".to_string(), vec!["2001:db8::1".to_string()]);
        let result = plugin.execute(ctx, &HashMap::new()).await.unwrap();
        // Bare v6 source, original peer port kept, bracketed for ip:port form.
        assert_eq!(result.context.request.remote_addr, "[2001:db8::1]:5000");
    }

    #[test]
    fn test_real_ip_parse_ip_port_forms() {
        assert_eq!(
            parse_ip_port("1.2.3.4:80"),
            Some(("1.2.3.4".parse().unwrap(), Some(80)))
        );
        assert_eq!(
            parse_ip_port("1.2.3.4"),
            Some(("1.2.3.4".parse().unwrap(), None))
        );
        assert_eq!(
            parse_ip_port("[::1]:8080"),
            Some(("::1".parse().unwrap(), Some(8080)))
        );
        assert_eq!(parse_ip_port("::1"), Some(("::1".parse().unwrap(), None)));
        assert_eq!(parse_ip_port("1.2.3.4:0"), None); // port out of range
        assert_eq!(parse_ip_port("1.2.3.4:99999"), None);
        assert_eq!(parse_ip_port("nonsense"), None);
    }
}
