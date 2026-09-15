//! The per-request `Context` object that flows through every node of a policy
//! graph, carrying the inbound request, the response under construction,
//! free-form inter-node state, and any errors accumulated along the way.
//! Serializable so it can be marshalled to and from Lua scripts.

use bytes::Bytes;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::net::SocketAddr;

pub mod stream;

/// Holds all state for one request as it travels through a policy graph.
///
/// Each plugin receives the context, may mutate any part of it, and passes it
/// on through its success or error port. The engine sends `response` back to
/// the client once graph execution finishes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Context {
    /// The inbound HTTP request as received by the listener (plugins may rewrite it before proxying).
    pub request: GatewayRequest,
    /// The response being built; whatever is here when the graph finishes is sent to the client.
    pub response: GatewayResponse,
    /// Free-form key/value scratch space for passing data between nodes (e.g. auth claims).
    pub message: HashMap<String, serde_json::Value>,
    /// Errors recorded by nodes; routing through a node's `error` port appends here.
    pub errors: Vec<GatewayError>,
}

/// Protocol-agnostic snapshot of the inbound request, decoupled from hyper types.
///
/// Multi-valued headers and query parameters are preserved as `Vec<String>`.
/// The body is fully buffered; it serializes as base64 (see `bytes_serde`),
/// which is how it crosses into Lua scripts.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GatewayRequest {
    /// HTTP method (`GET`, `POST`, ...).
    pub method: String,
    /// Request path without the query string.
    pub path: String,
    /// Request authority: the `Host` header, or the HTTP/2 `:authority`
    /// pseudo-header when no `Host` is present (empty string when neither is).
    pub host: String,
    /// URI scheme, defaulting to `http` when the URI carries none.
    pub scheme: String,
    /// Header name → list of values (headers may repeat).
    pub headers: HashMap<String, Vec<String>>,
    /// Query parameter name → list of values (parameters may repeat).
    pub query_params: HashMap<String, Vec<String>>,
    /// Fully buffered request body, serialized as base64.
    #[serde(with = "bytes_serde")]
    pub body: Bytes,
    /// Client socket address as `ip:port`.
    pub remote_addr: String,
    /// Wire protocol the request arrived on.
    pub protocol: Protocol,
}

/// The response under construction, ultimately returned to the client.
#[derive(Debug, Serialize, Deserialize)]
pub struct GatewayResponse {
    /// HTTP status code; `0` until a node (e.g. upstream or error-handler) sets it.
    pub status_code: u16,
    /// Header name → list of values.
    pub headers: HashMap<String, Vec<String>>,
    /// Response body, serialized as base64.
    #[serde(with = "bytes_serde")]
    pub body: Bytes,
    /// Streaming body, when the upstream response is relayed unbuffered.
    /// Skipped by serde: `Context` must stay serializable for Lua marshalling
    /// and debug snapshots. Invariant: when this is `Some`, `body` is empty.
    /// Set by a node starting in Task 2; read by the listener starting in
    /// Task 6, so it is unread on any path exercised today.
    #[serde(skip)]
    #[allow(dead_code)]
    pub stream: Option<crate::context::stream::ResponseStream>,
}

impl Clone for GatewayResponse {
    /// Cloning drops any stream: a stream has exactly one consumer, and every
    /// caller that clones a response (debug snapshots, cache stores) wants the
    /// buffered form.
    fn clone(&self) -> Self {
        Self {
            status_code: self.status_code,
            headers: self.headers.clone(),
            body: self.body.clone(),
            stream: None,
        }
    }
}

/// An error recorded by a node during graph execution.
///
/// Errors do not abort the pipeline by themselves; the graph engine routes
/// the context through the failing node's `error` port (or the policy's
/// error handler) with this record appended to `Context::errors`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct GatewayError {
    /// Id of the node that produced the error.
    pub node_id: String,
    /// Machine-readable error code (e.g. `unauthorized`, `rate_limited`).
    pub code: String,
    /// Human-readable description.
    pub message: String,
    /// Optional structured details; defaults to empty when absent.
    #[serde(default)]
    pub metadata: HashMap<String, serde_json::Value>,
}

/// Wire protocol of the inbound connection.
///
/// Serialized in lowercase (`http1`, `http2`, ...). Only `Http1` and `Http2`
/// are currently produced; the remaining variants are reserved for planned
/// WebSocket/TCP/UDP proxying.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum Protocol {
    Http1,
    Http2,
    WebSocket,
    Tcp,
    Udp,
}

impl Context {
    /// Creates a fresh context for an inbound request.
    ///
    /// The response starts empty with `status_code == 0` (i.e. "unset"),
    /// and `message`/`errors` start empty.
    pub fn new(request: GatewayRequest) -> Self {
        Self {
            request,
            response: GatewayResponse {
                status_code: 0,
                headers: HashMap::new(),
                body: Bytes::new(),
                stream: None,
            },
            message: HashMap::new(),
            errors: Vec::new(),
        }
    }
}

impl GatewayRequest {
    /// Builds a `GatewayRequest` from parsed hyper request parts and a buffered body.
    ///
    /// Non-UTF-8 header values become empty strings, the query string is split
    /// on `&`/`=` without percent-decoding, and the protocol is classified as
    /// HTTP/2 only for `http::Version::HTTP_2` (HTTP/1.x otherwise).
    pub fn from_hyper(req: &http::request::Parts, body: Bytes, remote_addr: SocketAddr) -> Self {
        let mut headers: HashMap<String, Vec<String>> = HashMap::new();
        for (name, value) in req.headers.iter() {
            headers
                .entry(name.as_str().to_string())
                .or_default()
                .push(value.to_str().unwrap_or("").to_string());
        }

        // RFC 9113 §8.2.3: an HTTP/2 client may split one request's cookies
        // across several `cookie` fields, and the server must concatenate them
        // before processing. Join here, once, rather than in each reader —
        // every cookie consumer in the gateway takes the first field only, so
        // an unjoined second field is invisible to all of them.
        if let Some(cookies) = headers.get_mut("cookie") {
            if cookies.len() > 1 {
                *cookies = vec![cookies.join("; ")];
            }
        }

        let mut query_params: HashMap<String, Vec<String>> = HashMap::new();
        if let Some(query) = req.uri.query() {
            for pair in query.split('&') {
                let mut parts = pair.splitn(2, '=');
                let key = parts.next().unwrap_or("").to_string();
                let value = parts.next().unwrap_or("").to_string();
                query_params.entry(key).or_default().push(value);
            }
        }

        // HTTP/1.x carries the authority in the `Host` header. HTTP/2 carries
        // it in the `:authority` pseudo-header instead — which hyper exposes on
        // the URI, not as a header — and browsers send no `Host` at all over
        // h2. Falling back to the URI authority keeps `request.host` populated
        // on both, so `match.host` route rules, `$host` and the
        // `http_to_https` redirect target behave the same either way.
        let host = req
            .headers
            .get("host")
            .and_then(|v| v.to_str().ok())
            .filter(|h| !h.is_empty())
            .map(|h| h.to_string())
            .or_else(|| req.uri.authority().map(|a| a.as_str().to_string()))
            .unwrap_or_default();

        let scheme = req.uri.scheme_str().unwrap_or("http").to_string();

        let protocol = if req.version == http::Version::HTTP_2 {
            Protocol::Http2
        } else {
            Protocol::Http1
        };

        Self {
            method: req.method.as_str().to_string(),
            path: req.uri.path().to_string(),
            host,
            scheme,
            headers,
            query_params,
            body,
            remote_addr: remote_addr.to_string(),
            protocol,
        }
    }
}

/// Serde adapter that encodes `Bytes` as a base64 string, keeping binary
/// bodies intact through JSON/Lua round-trips.
mod bytes_serde {
    use base64::{engine::general_purpose::STANDARD, Engine};
    use bytes::Bytes;
    use serde::{self, Deserialize, Deserializer, Serializer};

    pub fn serialize<S>(bytes: &Bytes, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&STANDARD.encode(bytes))
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Bytes, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        STANDARD
            .decode(&s)
            .map(Bytes::from)
            .map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::SocketAddr;

    fn parts_with(headers: Vec<(&str, &str)>) -> http::request::Parts {
        let mut builder = http::Request::builder().uri("/cb").method("GET");
        for (k, v) in headers {
            builder = builder.header(k, v);
        }
        builder.body(()).unwrap().into_parts().0
    }

    /// HTTP/2 clients may split the cookies of one request across several
    /// `cookie` header fields -- RFC 9113 sec. 8.2.3 permits it and requires the
    /// server to join them before processing. Firefox does exactly this.
    /// Every cookie reader in the gateway takes `.first()`, so an unjoined
    /// second field is silently invisible: the OIDC login-flow cookie goes
    /// missing and the callback rejects a perfectly good login.
    #[test]
    fn test_multiple_cookie_fields_are_joined() {
        let parts = parts_with(vec![
            ("cookie", "oidc_session=abc"),
            ("cookie", "oidc_session_flow=xyz"),
        ]);
        let req = GatewayRequest::from_hyper(
            &parts,
            Bytes::new(),
            "1.2.3.4:5".parse::<SocketAddr>().unwrap(),
        );

        let cookies = req.headers.get("cookie").expect("cookie header present");
        assert_eq!(
            cookies.len(),
            1,
            "cookie fields must be joined into one, got {cookies:?}"
        );
        assert_eq!(cookies[0], "oidc_session=abc; oidc_session_flow=xyz");
    }

    /// HTTP/2 carries the authority in the `:authority` pseudo-header, which
    /// hyper exposes on the request URI rather than as a `Host` header --
    /// browsers do not send `Host` over h2 at all. Reading only the header
    /// leaves `request.host` empty, which silently breaks every `match.host`
    /// route rule (the request matches no host-scoped route), `$host` /
    /// `{{request.host}}`, and the `http_to_https` redirect target.
    #[test]
    fn test_http2_authority_populates_host() {
        let parts = http::Request::builder()
            .method("GET")
            .version(http::Version::HTTP_2)
            .uri("https://api.example.com/thing")
            .body(())
            .unwrap()
            .into_parts()
            .0;

        let req = GatewayRequest::from_hyper(
            &parts,
            Bytes::new(),
            "1.2.3.4:5".parse::<SocketAddr>().unwrap(),
        );

        assert_eq!(req.host, "api.example.com");
    }

    /// The `Host` header remains authoritative for HTTP/1.x, where hyper gives
    /// the URI in origin-form and there is no authority to fall back to.
    #[test]
    fn test_http1_host_header_still_used() {
        let parts = parts_with(vec![("host", "legacy.example.com")]);
        let req = GatewayRequest::from_hyper(
            &parts,
            Bytes::new(),
            "1.2.3.4:5".parse::<SocketAddr>().unwrap(),
        );

        assert_eq!(req.host, "legacy.example.com");
    }
}
