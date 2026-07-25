//! Shared helpers for the serverless / function-as-a-service upstream plugins
//! (`azure-functions`, `openfunction`, `openwhisk`).
//!
//! These plugins all invoke an external FaaS endpoint and return its reply as
//! the gateway response. The helpers here centralize the request-forwarding
//! shape (method, headers, query, target URL) and the outbound-error → status
//! mapping so each plugin only adds its own auth header and response handling.

use std::collections::HashMap;

use crate::context::Context;
use crate::outbound::OutboundError;

/// The forwarded request parts for a FaaS callout: header list, target URL, and
/// HTTP method.
pub type ForwardParts = (Vec<(String, String)>, String, http::Method);

/// Builds the forwarded request parts for a FaaS callout: the header list
/// (client headers minus hop-by-hop, with `Host` overridden to the endpoint),
/// the fully-qualified target URL (`function_uri` path plus the client's query
/// string), and the client's HTTP method.
///
/// Returns an error string when `function_uri` cannot be parsed or has no host.
pub fn forward_parts(function_uri: &str, ctx: &Context) -> Result<ForwardParts, String> {
    let parsed: http::Uri = function_uri
        .parse()
        .map_err(|e| format!("invalid function_uri '{}': {}", function_uri, e))?;
    let authority = parsed
        .authority()
        .map(|a| a.as_str().to_string())
        .ok_or_else(|| format!("function_uri '{}' has no host", function_uri))?;
    let scheme = parsed.scheme_str().unwrap_or("https");
    let path = if parsed.path().is_empty() {
        "/"
    } else {
        parsed.path()
    };

    let mut headers: Vec<(String, String)> = Vec::new();
    for (name, values) in &ctx.request.headers {
        let lname = name.to_ascii_lowercase();
        if matches!(lname.as_str(), "host" | "connection" | "content-length") {
            continue;
        }
        for value in values {
            headers.push((lname.clone(), value.clone()));
        }
    }
    headers.push(("host".to_string(), authority.clone()));

    // Prefer the client's query string; fall back to any query on function_uri.
    let query = query_string(&ctx.request.query_params);
    let query = if query.is_empty() {
        parsed.query().unwrap_or("").to_string()
    } else {
        query
    };
    let url = if query.is_empty() {
        format!("{}://{}{}", scheme, authority, path)
    } else {
        format!("{}://{}{}?{}", scheme, authority, path, query)
    };

    let method: http::Method = ctx.request.method.parse().unwrap_or(http::Method::POST);

    Ok((headers, url, method))
}

/// Encodes the request query parameters as a `&`-joined `name=value` string,
/// sorted for stable output. Names and values are percent-encoded.
fn query_string(query: &HashMap<String, Vec<String>>) -> String {
    let mut pairs: Vec<(String, String)> = Vec::new();
    for (name, values) in query {
        for value in values {
            pairs.push((uri_encode(name), uri_encode(value)));
        }
    }
    pairs.sort();
    pairs
        .iter()
        .map(|(k, v)| format!("{}={}", k, v))
        .collect::<Vec<_>>()
        .join("&")
}

/// Percent-encodes per RFC 3986, keeping the unreserved set intact.
fn uri_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{:02X}", b));
        }
    }
    out
}

/// Maps an [`OutboundError`] to a client-facing `(status, message)` pair used
/// by the FaaS plugins' error ports.
pub fn classify_error(plugin: &str, err: &OutboundError) -> (u16, String) {
    match err {
        OutboundError::Timeout(d) => (504, format!("{} callout timed out after {:?}", plugin, d)),
        OutboundError::InvalidRequest(m) => (502, format!("{} request build error: {}", plugin, m)),
        OutboundError::Transport(m) => (503, format!("{} callout failed: {}", plugin, m)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::{GatewayRequest, GatewayResponse, Protocol};
    use bytes::Bytes;

    fn ctx() -> Context {
        Context {
            request: GatewayRequest {
                method: "POST".to_string(),
                path: "/orig".to_string(),
                host: "gw".to_string(),
                scheme: "http".to_string(),
                headers: HashMap::new(),
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

    #[test]
    fn test_forward_parts_url_and_host() {
        let mut c = ctx();
        c.request
            .headers
            .insert("x-test".to_string(), vec!["v".to_string()]);
        c.request
            .query_params
            .insert("b".to_string(), vec!["2".to_string()]);
        c.request
            .query_params
            .insert("a".to_string(), vec!["1".to_string()]);
        let (headers, url, method) = forward_parts("https://host.example.com/api/fn", &c).unwrap();
        assert_eq!(url, "https://host.example.com/api/fn?a=1&b=2");
        assert_eq!(method, http::Method::POST);
        let get = |n: &str| {
            headers
                .iter()
                .find(|(k, _)| k == n)
                .map(|(_, v)| v.as_str())
        };
        assert_eq!(get("host"), Some("host.example.com"));
        assert_eq!(get("x-test"), Some("v"));
    }

    #[test]
    fn test_forward_parts_rejects_bad_uri() {
        assert!(forward_parts("not a uri", &ctx()).is_err());
        assert!(forward_parts("/no-host", &ctx()).is_err());
    }
}
