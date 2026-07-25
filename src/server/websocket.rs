//! WebSocket proxying: detect a client upgrade, open the matching upstream
//! WebSocket handshake, and relay the two upgraded connections byte-for-byte.
//!
//! This is the one place raw hyper connection-upgrade machinery lives. The
//! node-graph still runs for a WebSocket request (so access-phase plugins —
//! auth, cors, rate-limit, path rewrite — apply); the `upstream` node resolves
//! the target and signals intent with `101` + `__ws_upstream_*` context keys,
//! and the listener calls [`proxy_upgrade`] to finish the handshake and start
//! the relay.
//!
//! The relay is a transparent byte pump ([`tokio::io::copy_bidirectional`]) —
//! no frame parsing. The upstream leg is always an HTTP/1.1 WebSocket
//! handshake — `ws://` by default, or `wss://` when the `upstream` node sets
//! `tls` (see [`proxy_upgrade`]).
//!
//! Two **client** transports are supported: HTTP/1.1 (`Connection: Upgrade` →
//! `101`, key/accept forwarded transparently) and HTTP/2 (RFC 8441 extended
//! CONNECT → `200`; the h2 client sends no `Sec-WebSocket-Key`, so the gateway
//! synthesizes one for the upstream handshake). Client-facing `wss://` works
//! for either, because TLS is terminated before this runs.

use bytes::Bytes;
use http::HeaderMap;
use http_body_util::Full;
use hyper::upgrade::OnUpgrade;
use hyper::{Request, Response};
use hyper_util::rt::TokioIo;
use tokio::net::TcpStream;
use tracing::{debug, warn};

/// Failure establishing the upstream side of a WebSocket proxy. All variants
/// map to a `502 Bad Gateway` at the listener — the client was never sent the
/// `101`, so it sees a failed handshake.
#[derive(Debug, thiserror::Error)]
pub enum WsError {
    #[error("failed to connect to upstream {0}: {1}")]
    Connect(String, String),
    #[error("upstream websocket handshake failed: {0}")]
    Handshake(String),
    #[error("upstream rejected the websocket upgrade (status {0})")]
    UpstreamRejected(u16),
    #[error("upstream upgrade failed: {0}")]
    Upgrade(String),
    #[error("upstream TLS setup failed: {0}")]
    Tls(String),
}

/// Returns true when the request headers ask for a WebSocket upgrade:
/// `Connection` carries an `upgrade` token (comma-separated, case-insensitive)
/// **and** `Upgrade: websocket`.
pub fn is_websocket_upgrade(headers: &HeaderMap) -> bool {
    let connection_upgrade = headers
        .get(http::header::CONNECTION)
        .and_then(|v| v.to_str().ok())
        .map(|v| {
            v.split(',')
                .any(|token| token.trim().eq_ignore_ascii_case("upgrade"))
        })
        .unwrap_or(false);

    let upgrade_websocket = headers
        .get(http::header::UPGRADE)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.eq_ignore_ascii_case("websocket"))
        .unwrap_or(false);

    connection_upgrade && upgrade_websocket
}

/// Returns true when this is an HTTP/2 RFC 8441 extended-CONNECT WebSocket
/// request: `:method == CONNECT` and a `:protocol` extension of `websocket`.
///
/// hyper surfaces the `:protocol` pseudo-header as a [`hyper::ext::Protocol`]
/// in the request extensions (behind the enabled `http2` feature).
pub fn is_h2_websocket_connect(method: &http::Method, extensions: &http::Extensions) -> bool {
    method == http::Method::CONNECT
        && extensions
            .get::<hyper::ext::Protocol>()
            .is_some_and(|p| p.as_str().eq_ignore_ascii_case("websocket"))
}

/// Generates a fresh `Sec-WebSocket-Key` (16 random bytes, base64) for the
/// upstream HTTP/1.1 handshake when the client came in over HTTP/2 and thus
/// never sent one.
fn synthesize_ws_key() -> String {
    use base64::{engine::general_purpose::STANDARD, Engine};
    use ring::rand::{SecureRandom, SystemRandom};
    let mut buf = [0u8; 16];
    SystemRandom::new()
        .fill(&mut buf)
        .expect("system RNG unavailable");
    STANDARD.encode(buf)
}

/// The WebSocket handshake headers to forward verbatim from the client request
/// to the upstream. `Host` is set separately to the upstream target.
const FORWARD_HEADERS: &[&str] = &[
    "upgrade",
    "connection",
    "sec-websocket-key",
    "sec-websocket-version",
    "sec-websocket-protocol",
    "sec-websocket-extensions",
];

/// The upstream 101-response headers to echo back to the client.
const ECHO_HEADERS: &[&str] = &[
    "upgrade",
    "connection",
    "sec-websocket-accept",
    "sec-websocket-protocol",
    "sec-websocket-extensions",
];

/// Opens the (always HTTP/1.1) upstream WebSocket handshake to `host:port` at
/// `path`, and — on a successful upstream `101` — returns the client-facing
/// response and spawns a task that relays bytes between the client and upstream
/// once the client connection upgrades.
///
/// `client_is_h2` selects the client-facing semantics:
/// - `false` (HTTP/1.1 client): forward the client's `Sec-WebSocket-*` headers
///   to the upstream and return a `101 Switching Protocols` echoing the
///   upstream's `Sec-WebSocket-Accept`.
/// - `true` (HTTP/2 RFC 8441 client): the client sent no `Sec-WebSocket-Key`,
///   so synthesize one (and `Version: 13`) for the upstream handshake, and
///   return a `200 OK` (extended CONNECT success) with no `Sec-WebSocket-*`
///   headers.
///
/// `fwd_headers` are the client request's headers (name → values); only the
/// WebSocket-relevant ones are forwarded. `client_on_upgrade` is the client's
/// [`OnUpgrade`] captured by the listener before the request was consumed; it
/// resolves only after the returned response is written back to the client.
// Each parameter is a distinct, meaningful part of the upstream handshake;
// bundling them into a struct would only move the same fields around.
#[allow(clippy::too_many_arguments)]
pub async fn proxy_upgrade(
    host: String,
    port: u16,
    path: String,
    tls: bool,
    verify: bool,
    fwd_headers: &std::collections::HashMap<String, Vec<String>>,
    client_on_upgrade: OnUpgrade,
    client_is_h2: bool,
) -> Result<Response<Full<Bytes>>, WsError> {
    // 1. Raw TCP to the upstream (TLS-wrapped for `wss`), driven by a hyper
    //    client connection that allows upgrades. Both arms yield the same
    //    `SendRequest` type (generic over the body, not the IO), so the rest of
    //    the handshake below is written once.
    let tcp = TcpStream::connect((host.as_str(), port))
        .await
        .map_err(|e| WsError::Connect(format!("{}:{}", host, port), e.to_string()))?;

    let mut sender = if tls {
        let connector = crate::outbound::client_tls_connector(verify).map_err(WsError::Tls)?;
        let server_name = rustls::pki_types::ServerName::try_from(host.clone())
            .map_err(|e| WsError::Tls(format!("invalid server name '{}': {}", host, e)))?;
        let tls_stream = connector
            .connect(server_name, tcp)
            .await
            .map_err(|e| WsError::Tls(e.to_string()))?;
        let (sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(tls_stream))
            .await
            .map_err(|e| WsError::Handshake(e.to_string()))?;
        tokio::spawn(async move {
            if let Err(e) = conn.with_upgrades().await {
                debug!("upstream wss connection closed: {}", e);
            }
        });
        sender
    } else {
        let (sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(tcp))
            .await
            .map_err(|e| WsError::Handshake(e.to_string()))?;
        tokio::spawn(async move {
            if let Err(e) = conn.with_upgrades().await {
                debug!("upstream ws connection closed: {}", e);
            }
        });
        sender
    };

    // 2. Build the upstream handshake request (always HTTP/1.1), overriding
    //    Host with the target. Forward the client's WebSocket headers; for an
    //    h2 client, the key/version handshake headers don't exist on the wire,
    //    so synthesize them.
    let mut builder = Request::builder()
        .method(http::Method::GET)
        .uri(&path)
        .header(http::header::HOST, format!("{}:{}", host, port));
    for name in FORWARD_HEADERS {
        // An h2 extended-CONNECT client sends none of the h1 handshake headers
        // (`upgrade`/`connection`/`sec-websocket-key`/`-version`) on the wire —
        // we synthesize them below. Only `sec-websocket-protocol`/`-extensions`
        // may carry over.
        if client_is_h2
            && matches!(
                *name,
                "upgrade" | "connection" | "sec-websocket-key" | "sec-websocket-version"
            )
        {
            continue;
        }
        if let Some(values) = fwd_headers.get(*name) {
            for value in values {
                builder = builder.header(*name, value);
            }
        }
    }
    if client_is_h2 {
        builder = builder
            .header("upgrade", "websocket")
            .header("connection", "upgrade")
            .header("sec-websocket-key", synthesize_ws_key());
        if !fwd_headers.contains_key("sec-websocket-version") {
            builder = builder.header("sec-websocket-version", "13");
        }
    }
    let req = builder
        .body(Full::new(Bytes::new()))
        .map_err(|e| WsError::Handshake(e.to_string()))?;

    // 3. Send it and require a 101.
    let resp = sender
        .send_request(req)
        .await
        .map_err(|e| WsError::Handshake(e.to_string()))?;
    if resp.status() != http::StatusCode::SWITCHING_PROTOCOLS {
        return Err(WsError::UpstreamRejected(resp.status().as_u16()));
    }

    // 4. Build the client-facing response, then take the upstream's upgraded
    //    stream. An h1 client gets a `101` echoing the upstream handshake
    //    headers; an h2 (RFC 8441) client gets a `200` — hyper strips
    //    hop-by-hop headers and completing the extended CONNECT needs a 2xx
    //    with an empty body.
    let client_response = if client_is_h2 {
        let mut b = Response::builder().status(http::StatusCode::OK);
        // A negotiated subprotocol is a normal header in h2; forward it.
        if let Some(value) = resp.headers().get("sec-websocket-protocol") {
            b = b.header("sec-websocket-protocol", value);
        }
        b
    } else {
        let mut b = Response::builder().status(http::StatusCode::SWITCHING_PROTOCOLS);
        for name in ECHO_HEADERS {
            if let Some(value) = resp.headers().get(*name) {
                b = b.header(*name, value);
            }
        }
        b
    };

    let upstream_upgraded = hyper::upgrade::on(resp)
        .await
        .map_err(|e| WsError::Upgrade(e.to_string()))?;

    let client_response = client_response
        .body(Full::new(Bytes::new()))
        .map_err(|e| WsError::Handshake(e.to_string()))?;

    // 5. Relay once the client side upgrades (after our 101 is written back).
    tokio::spawn(async move {
        match client_on_upgrade.await {
            Ok(client_upgraded) => {
                let mut client_io = TokioIo::new(client_upgraded);
                let mut upstream_io = TokioIo::new(upstream_upgraded);
                if let Err(e) =
                    tokio::io::copy_bidirectional(&mut client_io, &mut upstream_io).await
                {
                    debug!("websocket relay closed: {}", e);
                }
            }
            Err(e) => warn!("client websocket upgrade failed: {}", e),
        }
    });

    Ok(client_response)
}

/// A `502 Bad Gateway` JSON response, returned when the upstream WebSocket
/// handshake could not be completed.
pub fn bad_gateway_502() -> Response<Full<Bytes>> {
    Response::builder()
        .status(http::StatusCode::BAD_GATEWAY)
        .header(http::header::CONTENT_TYPE, "application/json")
        .body(Full::new(Bytes::from_static(
            br#"{"error":"bad_gateway","message":"upstream websocket handshake failed"}"#,
        )))
        .unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.append(
                http::HeaderName::from_bytes(k.as_bytes()).unwrap(),
                http::HeaderValue::from_str(v).unwrap(),
            );
        }
        h
    }

    #[test]
    fn test_detect_plain_upgrade() {
        assert!(is_websocket_upgrade(&headers(&[
            ("connection", "Upgrade"),
            ("upgrade", "websocket"),
        ])));
    }

    #[test]
    fn test_detect_multi_token_connection() {
        // Browsers commonly send "keep-alive, Upgrade".
        assert!(is_websocket_upgrade(&headers(&[
            ("connection", "keep-alive, Upgrade"),
            ("upgrade", "WebSocket"),
        ])));
    }

    #[test]
    fn test_detect_requires_both_headers() {
        assert!(!is_websocket_upgrade(&headers(&[(
            "connection",
            "Upgrade"
        )])));
        assert!(!is_websocket_upgrade(&headers(&[("upgrade", "websocket")])));
        assert!(!is_websocket_upgrade(&headers(&[])));
    }

    #[test]
    fn test_detect_rejects_non_websocket_upgrade() {
        // h2c upgrade is not a WebSocket.
        assert!(!is_websocket_upgrade(&headers(&[
            ("connection", "Upgrade"),
            ("upgrade", "h2c"),
        ])));
    }

    #[test]
    fn test_bad_gateway_502_shape() {
        let resp = bad_gateway_502();
        assert_eq!(resp.status(), 502);
        assert_eq!(
            resp.headers().get("content-type").unwrap(),
            "application/json"
        );
    }

    fn req_with(method: http::Method, protocol: Option<&'static str>) -> http::Request<()> {
        let mut req = http::Request::builder().method(method).body(()).unwrap();
        if let Some(p) = protocol {
            req.extensions_mut()
                .insert(hyper::ext::Protocol::from_static(p));
        }
        req
    }

    #[test]
    fn test_detect_h2_extended_connect() {
        let req = req_with(http::Method::CONNECT, Some("websocket"));
        assert!(is_h2_websocket_connect(req.method(), req.extensions()));
    }

    #[test]
    fn test_detect_h2_rejects_non_connect_and_non_websocket() {
        // GET with the extension is not an extended CONNECT.
        let get = req_with(http::Method::GET, Some("websocket"));
        assert!(!is_h2_websocket_connect(get.method(), get.extensions()));
        // CONNECT without a :protocol is a classic tunnel, not a WebSocket.
        let plain = req_with(http::Method::CONNECT, None);
        assert!(!is_h2_websocket_connect(plain.method(), plain.extensions()));
        // CONNECT with a different :protocol.
        let h2c = req_with(http::Method::CONNECT, Some("h2c"));
        assert!(!is_h2_websocket_connect(h2c.method(), h2c.extensions()));
    }

    #[test]
    fn test_synthesize_ws_key_is_16_bytes() {
        use base64::{engine::general_purpose::STANDARD, Engine};
        let key = synthesize_ws_key();
        assert_eq!(STANDARD.decode(key).unwrap().len(), 16);
    }
}
