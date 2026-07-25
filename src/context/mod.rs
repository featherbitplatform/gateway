//! The per-request `Context` object that flows through every node of a policy
//! graph, carrying the inbound request, the response under construction,
//! free-form inter-node state, and any errors accumulated along the way.
//! Serializable so it can be marshalled to and from Lua scripts.

use bytes::Bytes;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::net::SocketAddr;

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
    /// Value of the `Host` header (empty string when absent).
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
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GatewayResponse {
    /// HTTP status code; `0` until a node (e.g. upstream or error-handler) sets it.
    pub status_code: u16,
    /// Header name → list of values.
    pub headers: HashMap<String, Vec<String>>,
    /// Response body, serialized as base64.
    #[serde(with = "bytes_serde")]
    pub body: Bytes,
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

        let mut query_params: HashMap<String, Vec<String>> = HashMap::new();
        if let Some(query) = req.uri.query() {
            for pair in query.split('&') {
                let mut parts = pair.splitn(2, '=');
                let key = parts.next().unwrap_or("").to_string();
                let value = parts.next().unwrap_or("").to_string();
                query_params.entry(key).or_default().push(value);
            }
        }

        let host = req
            .headers
            .get("host")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();

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
