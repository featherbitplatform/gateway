//! Data-plane listener: the hyper accept loop and per-request handler. It
//! serves plain HTTP or, when `system.tls` is set, TLS-terminated HTTPS; over
//! either transport it speaks HTTP/1.1 and (when `system.http2.enabled`)
//! HTTP/2, negotiated per connection. For each request it matches a route,
//! builds the [`Context`], executes the route's
//! [`CompiledGraph`](crate::graph::CompiledGraph), and writes `Context.response`
//! back to the client.

use std::net::SocketAddr;
use std::sync::Arc;

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::TokioIo;
use hyper_util::server::graceful::GracefulShutdown;
use tokio::net::TcpListener;
use tokio::sync::watch;
use tracing::{error, info, warn};

use crate::config::SystemConfig;
use crate::context::{Context, GatewayRequest, Protocol};
use crate::routing::matches_route;
use crate::server::{tls, websocket};
use crate::state::SharedState;

/// Binds the data-plane listener and serves requests until the process exits.
///
/// Binds to the address/port from `system.listener` (`config/system.yaml`),
/// then accepts connections in a loop, spawning one Tokio task per connection.
/// When `system.tls` is set, connections are TLS-terminated with a
/// [`TlsAcceptor`](tokio_rustls::TlsAcceptor); each connection then speaks
/// HTTP/1.1 or HTTP/2 (when `system.http2.enabled`) as negotiated. Routes and
/// compiled graphs are read from the shared `SharedState`, so hot-reloads take
/// effect for subsequent requests without a restart.
///
/// On a shutdown signal (`shutdown_rx` flips to `true`) the accept loop stops
/// and in-flight connections are drained via hyper's graceful shutdown, bounded
/// by `timeouts.shutdown_timeout_seconds`; then this returns `Ok(())`.
///
/// Returns an error only for a fail-fast startup problem: an unparseable bind
/// address, a failed bind, or (when TLS is configured) an unreadable cert/key
/// or bad `min_version`. Per-connection errors — including TLS handshake
/// failures — are logged and do not stop the server.
pub async fn start_server(
    system: &SystemConfig,
    state: Arc<SharedState>,
    mut shutdown_rx: watch::Receiver<bool>,
) -> Result<(), Box<dyn std::error::Error>> {
    let addr = SocketAddr::new(system.listener.bind.parse()?, system.listener.port);

    let http2_enabled = system.http2.enabled;
    // Fail-fast: a configured-but-broken TLS setup aborts startup here rather
    // than failing every handshake at runtime. The config is hot-reloadable —
    // a cert-file change swaps it in for new connections without a restart.
    let tls_config: Option<tls::SharedTlsConfig> = match &system.tls {
        Some(tls_cfg) => {
            let shared = tls::build_reloadable(tls_cfg, http2_enabled)?;
            tls::spawn_cert_watcher(tls_cfg.clone(), http2_enabled, shared.clone(), "data-plane");
            Some(shared)
        }
        None => None,
    };

    let tcp_listener = TcpListener::bind(addr).await?;
    info!(
        "Gateway listening on {} ({}, http2={})",
        addr,
        if tls_config.is_some() {
            "https"
        } else {
            "http"
        },
        http2_enabled,
    );

    let graceful = GracefulShutdown::new();
    loop {
        tokio::select! {
            accepted = tcp_listener.accept() => {
                let (stream, remote_addr) = accepted?;
                let state = state.clone();
                let tls_config = tls_config.clone();
                // Owned watcher moves into the task so TLS handshakes stay
                // concurrent while the connection is still drain-tracked.
                let watcher = graceful.watcher();

                tokio::spawn(async move {
                    // Build the service per branch so the TLS branch can capture
                    // the verified client-cert fingerprint (mTLS identity). Build
                    // the acceptor from the *current* config so cert reloads apply
                    // to new connections.
                    match tls_config.as_ref().map(tls::current_acceptor) {
                        Some(acc) => match acc.accept(stream).await {
                            Ok(tls_stream) => {
                                let client_id = tls::client_cert_identity(&tls_stream);
                                let service = service_fn(move |req: Request<Incoming>| {
                                    let state = state.clone();
                                    let client_id = client_id.clone();
                                    async move {
                                        handle_request(req, remote_addr, client_id, &state).await
                                    }
                                });
                                let conn = tls::build_connection(TokioIo::new(tls_stream), service, http2_enabled);
                                if let Err(err) = watcher.watch(conn).await {
                                    error!("Connection error: {}", err);
                                }
                            }
                            // A failed handshake affects only this connection.
                            Err(err) => warn!("TLS handshake failed from {}: {}", remote_addr, err),
                        },
                        None => {
                            let service = service_fn(move |req: Request<Incoming>| {
                                let state = state.clone();
                                async move { handle_request(req, remote_addr, None, &state).await }
                            });
                            let conn = tls::build_connection(TokioIo::new(stream), service, http2_enabled);
                            if let Err(err) = watcher.watch(conn).await {
                                error!("Connection error: {}", err);
                            }
                        }
                    }
                });
            }
            _ = shutdown_rx.changed() => break,
        }
    }

    drop(tcp_listener); // stop accepting new connections
    let drain = std::time::Duration::from_secs(system.timeouts.shutdown_timeout_seconds);
    info!("Draining in-flight connections (up to {:?})…", drain);
    tokio::select! {
        _ = graceful.shutdown() => info!("Graceful shutdown complete"),
        _ = tokio::time::sleep(drain) => warn!("Shutdown drain timed out after {:?}; forcing exit", drain),
    }
    Ok(())
}

/// Handles a single request end-to-end: buffers the body, finds the first
/// route whose match rule accepts the request (first match wins, in config
/// order), executes its compiled graph, and converts the resulting
/// `Context.response` into a hyper response (a status of 0 is treated as
/// 200). Unmatched requests get a JSON 404. The read lock on
/// `state.routes` is held only for matching and is released before graph
/// execution so long-running upstream calls never block a config reload.
/// Policy name recorded on a trace for a request that matched no route, so the
/// debug panel and its policy filter can distinguish unrouted traffic.
const NO_ROUTE_POLICY: &str = "(no route matched)";

async fn handle_request(
    mut req: Request<Incoming>,
    remote_addr: SocketAddr,
    client_cert: Option<tls::ClientCertIdentity>,
    state: &SharedState,
) -> Result<Response<Full<Bytes>>, hyper::Error> {
    // Detect a WebSocket upgrade and capture the client's upgrade handle BEFORE
    // the request is consumed by `into_parts()` — afterwards it's gone. Two
    // client transports upgrade to WebSocket: HTTP/1.1 (`Connection: Upgrade`)
    // and HTTP/2 (RFC 8441 extended CONNECT, `:method CONNECT` + `:protocol
    // websocket`). The handle resolves only once we return the upgrade response,
    // so it's carried out-of-band (never inside the serde-clean `Context`).
    let is_h1_ws = websocket::is_websocket_upgrade(req.headers());
    let is_h2_ws = websocket::is_h2_websocket_connect(req.method(), req.extensions());
    let is_ws = is_h1_ws || is_h2_ws;
    let client_is_h2 = is_h2_ws;
    let client_on_upgrade = is_ws.then(|| hyper::upgrade::on(&mut req));

    let (parts, body) = req.into_parts();

    let body_bytes = body
        .collect()
        .await
        .map(|c| c.to_bytes())
        .unwrap_or_else(|_| Bytes::new());

    let path = parts.uri.path();
    let method = parts.method.as_str();
    let host = parts
        .headers
        .get("host")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    let headers: Vec<(String, String)> = parts
        .headers
        .iter()
        .map(|(k, v)| (k.as_str().to_string(), v.to_str().unwrap_or("").to_string()))
        .collect();

    // Read lock on routes
    let routes = state.routes.read().await;
    let matched = routes
        .iter()
        .find(|(route, _)| matches_route(&route.match_rule, path, method, &headers, host));

    let (route_name, policy_name, graph) = match matched {
        Some((route, graph)) => (route.name.clone(), route.policy.clone(), graph.clone()),
        None => {
            drop(routes);
            // An unrouted request is still an incoming request. When tracing is
            // on, record a zero-step trace so the debug panel shows 404s too —
            // otherwise "I sent a request but see nothing" is a real gap for any
            // path that does not match a route.
            let mut trace_id = None;
            if state.debug.enabled {
                let mut ctx =
                    Context::new(GatewayRequest::from_hyper(&parts, body_bytes, remote_addr));
                if state.debug.should_trace(&ctx.request.headers) {
                    let opted_in = state.debug.header_opt_in(&ctx.request.headers);
                    ctx.response.status_code = 404;
                    let rec = crate::debug::TraceRecorder::new(
                        &ctx,
                        state.debug.capture_options(),
                        state.debug.max_steps,
                    );
                    let id = crate::debug::new_trace_id();
                    let trace = rec.finish(
                        id.clone(),
                        state.debug.next_seq(),
                        crate::debug::TraceSource::Request,
                        None,
                        NO_ROUTE_POLICY.to_string(),
                        &ctx,
                        std::time::Duration::ZERO,
                    );
                    state.debug.record(trace);
                    trace_id = opted_in.then_some(id);
                }
            }
            let mut builder = Response::builder()
                .status(404)
                .header("content-type", "application/json");
            if let Some(id) = trace_id {
                builder = builder.header("x-featherbit-trace-id", id);
            }
            return Ok(builder
                .body(Full::new(Bytes::from(
                    r#"{"error": "not_found", "message": "No route matched"}"#,
                )))
                .unwrap());
        }
    };
    drop(routes); // release read lock before executing the graph

    let gateway_request = GatewayRequest::from_hyper(&parts, body_bytes, remote_addr);
    let mut ctx = Context::new(gateway_request);
    if is_ws {
        // Signal the `upstream` node to resolve a target and emit a 101 instead
        // of doing a buffered round-trip. Note for h2 WebSockets (RFC 8441):
        // the method is `CONNECT` (a route `match` constraining method to GET
        // won't match), and the authority is in `:authority` rather than a
        // `Host` header, so host-based route matching may see an empty host.
        // Path-based matching is unaffected.
        ctx.request.protocol = Protocol::WebSocket;
    }
    // Stamp the request start (unix millis) so logger nodes can report latency
    // without a Context struct change. Reserved key, ignored by other plugins.
    let start_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    ctx.message.insert(
        "__request_start_ms".to_string(),
        serde_json::json!(start_ms),
    );
    // Expose the verified mTLS client identity to the graph — scripts/plugins/
    // loggers can pin, authorize on, or record it. Reserved keys.
    if let Some(id) = client_cert {
        ctx.message.insert(
            "__client_cert_fingerprint".to_string(),
            serde_json::json!(id.fingerprint),
        );
        if let Some(cn) = id.subject_cn {
            ctx.message.insert(
                "__client_cert_subject_cn".to_string(),
                serde_json::json!(cn),
            );
        }
        if !id.san_dns.is_empty() {
            ctx.message.insert(
                "__client_cert_san_dns".to_string(),
                serde_json::json!(id.san_dns),
            );
        }
    }

    // Debug mode: trace this request when it opts in (or when trace_all is on).
    // On an untraced request the whole cost is one HashMap lookup, and only
    // while debug is enabled at all.
    let started = std::time::Instant::now();
    let (result_ctx, trace_id) = if state.debug.should_trace(&ctx.request.headers) {
        // Only echo the id back when the caller explicitly opted in — a
        // trace_all-captured request did not ask, and its response should not
        // carry a debug header the client never requested.
        let opted_in = state.debug.header_opt_in(&ctx.request.headers);
        let recorder = crate::debug::TraceRecorder::new(
            &ctx,
            state.debug.capture_options(),
            state.debug.max_steps,
        );
        let (out_ctx, recorder) = graph.execute_traced(ctx, recorder).await;
        let id = crate::debug::new_trace_id();
        let trace = recorder.finish(
            id.clone(),
            state.debug.next_seq(),
            crate::debug::TraceSource::Request,
            Some(route_name.clone()),
            policy_name.clone(),
            &out_ctx,
            started.elapsed(),
        );
        state.debug.record(trace);
        (out_ctx, opted_in.then_some(id))
    } else {
        (graph.execute(ctx).await, None)
    };

    let status = if result_ctx.response.status_code == 0 {
        200
    } else {
        result_ctx.response.status_code
    };

    state
        .metrics
        .request_duration
        .with_label_values(&[&route_name])
        .observe(started.elapsed().as_secs_f64());
    state
        .metrics
        .request_count
        .with_label_values(&[
            route_name.as_str(),
            parts.method.as_str(),
            status.to_string().as_str(),
        ])
        .inc();
    for err in &result_ctx.errors {
        state
            .metrics
            .request_errors
            .with_label_values(&[&route_name, &err.code])
            .inc();
    }

    // WebSocket branch: the graph resolved an upstream and emitted a 101. Take
    // over the connection — open the upstream handshake and relay. If anything
    // earlier in the graph rejected the request (e.g. auth 401) it won't be a
    // 101, so we fall through to the normal buffered response below and the
    // captured upgrade handle is simply dropped.
    if is_ws && result_ctx.response.status_code == 101 {
        if let (Some(on_upgrade), Some(host), Some(port)) = (
            client_on_upgrade,
            result_ctx
                .message
                .get("__ws_upstream_host")
                .and_then(|v| v.as_str())
                .map(String::from),
            result_ctx
                .message
                .get("__ws_upstream_port")
                .and_then(|v| v.as_u64()),
        ) {
            let path = result_ctx
                .message
                .get("__ws_upstream_path")
                .and_then(|v| v.as_str())
                .unwrap_or("/")
                .to_string();
            let tls = result_ctx
                .message
                .get("__ws_upstream_tls")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let verify = result_ctx
                .message
                .get("__ws_upstream_verify")
                .and_then(|v| v.as_bool())
                .unwrap_or(true);
            return Ok(
                match websocket::proxy_upgrade(
                    host,
                    port as u16,
                    path,
                    tls,
                    verify,
                    &result_ctx.request.headers,
                    on_upgrade,
                    client_is_h2,
                )
                .await
                {
                    Ok(resp) => resp,
                    Err(e) => {
                        warn!("websocket proxy to upstream failed: {}", e);
                        websocket::bad_gateway_502()
                    }
                },
            );
        }
    }

    let mut response_builder = Response::builder().status(status);

    for (key, values) in &result_ctx.response.headers {
        for value in values {
            response_builder = response_builder.header(key.as_str(), value.as_str());
        }
    }

    // Tell the caller which trace to fetch. Only ever set for a request that
    // opted in, so it reveals nothing to anyone who did not already know.
    if let Some(id) = trace_id {
        response_builder = response_builder.header("x-featherbit-trace-id", id.as_str());
    }

    Ok(response_builder
        .body(Full::new(result_ctx.response.body))
        .unwrap())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config_store::FileConfigStore;
    use futures_util::{SinkExt, StreamExt};
    use hyper::service::service_fn;

    #[tokio::test]
    async fn test_graceful_shutdown_drains_in_flight() {
        use std::time::Duration;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, mut rx) = watch::channel(false);

        // Graceful-aware accept loop mirroring `start_server`, with a slow
        // handler so a request is in-flight when shutdown fires.
        let server = tokio::spawn(async move {
            let graceful = GracefulShutdown::new();
            loop {
                tokio::select! {
                    accepted = listener.accept() => {
                        let (stream, _) = accepted.unwrap();
                        let watcher = graceful.watcher();
                        tokio::spawn(async move {
                            let service = service_fn(|_req: Request<Incoming>| async {
                                tokio::time::sleep(Duration::from_millis(300)).await;
                                Ok::<_, hyper::Error>(Response::new(Full::new(Bytes::from_static(b"ok"))))
                            });
                            let conn = tls::build_connection(TokioIo::new(stream), service, false);
                            let _ = watcher.watch(conn).await;
                        });
                    }
                    _ = rx.changed() => break,
                }
            }
            drop(listener);
            tokio::select! {
                _ = graceful.shutdown() => {}
                _ = tokio::time::sleep(Duration::from_secs(5)) => {}
            }
        });

        let url = format!("http://127.0.0.1:{}/", addr.port());
        let req_fut = reqwest::Client::new().get(&url).send();

        // Fire shutdown ~50ms in, while the request is mid-flight.
        let killer = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            let _ = tx.send(true);
        });

        // The in-flight request must still complete (drained), not be cut.
        let resp = req_fut.await.unwrap();
        assert_eq!(resp.status(), 200);
        assert_eq!(resp.text().await.unwrap(), "ok");
        killer.await.unwrap();

        // The accept loop must exit and drain within the timeout.
        tokio::time::timeout(Duration::from_secs(5), server)
            .await
            .expect("server did not shut down")
            .unwrap();

        // After shutdown the listener is closed — new requests are refused.
        let after = reqwest::Client::new()
            .get(&url)
            .timeout(Duration::from_secs(1))
            .send()
            .await;
        assert!(
            after.is_err(),
            "new requests must be refused after shutdown"
        );
    }
    use tokio::net::TcpStream;
    use tokio_tungstenite::tungstenite::Message;

    /// A minimal WebSocket echo server; returns the address it bound to.
    async fn echo_ws_server() -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                tokio::spawn(async move {
                    let ws = match tokio_tungstenite::accept_async(stream).await {
                        Ok(ws) => ws,
                        Err(_) => return,
                    };
                    let (mut tx, mut rx) = ws.split();
                    while let Some(Ok(msg)) = rx.next().await {
                        if msg.is_close() {
                            break;
                        }
                        if (msg.is_text() || msg.is_binary()) && tx.send(msg).await.is_err() {
                            break;
                        }
                    }
                });
            }
        });
        addr
    }

    /// Builds `SharedState` with one route whose policy proxies `/ws` to the
    /// given upstream via `listener → upstream → client`.
    fn state_with_upstream(host: &str, port: u16) -> Arc<SharedState> {
        let gw_yaml = format!(
            r#"
routes:
  - name: ws
    match: {{ path: /ws }}
    policy: p
policies:
  - name: p
    nodes:
      - {{ id: listener, type: listener }}
      - {{ id: backend, type: upstream, config: {{ targets: [ {{ host: {host}, port: {port} }} ] }} }}
      - {{ id: client, type: client }}
    edges:
      - {{ from: listener.out, to: backend.in }}
      - {{ from: backend.success, to: client.in }}
"#
        );
        build_state(&gw_yaml)
    }

    /// Like [`state_with_upstream`] but marks the upstream `tls: true` with
    /// verification off (self-signed test cert) so the relay uses `wss://`.
    fn state_with_tls_upstream(host: &str, port: u16) -> Arc<SharedState> {
        let gw_yaml = format!(
            r#"
routes:
  - name: ws
    match: {{ path: /ws }}
    policy: p
policies:
  - name: p
    nodes:
      - {{ id: listener, type: listener }}
      - {{ id: backend, type: upstream, config: {{ targets: [ {{ host: {host}, port: {port} }} ], tls: true, ssl_verify: false }} }}
      - {{ id: client, type: client }}
    edges:
      - {{ from: listener.out, to: backend.in }}
      - {{ from: backend.success, to: client.in }}
"#
        );
        build_state(&gw_yaml)
    }

    fn build_state(gw_yaml: &str) -> Arc<SharedState> {
        let system = serde_yaml::from_str("{}").unwrap();
        let gateway = serde_yaml::from_str(gw_yaml).unwrap();
        Arc::new(
            SharedState::new(
                system,
                gateway,
                None,
                Arc::new(FileConfigStore::new("unused.yaml".into())),
            )
            .unwrap(),
        )
    }

    /// A TLS-terminated (`wss`) WebSocket echo server using a self-signed cert;
    /// returns the address it bound to.
    async fn tls_echo_ws_server() -> SocketAddr {
        use crate::config::TlsConfig;

        let certified = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
        let dir = std::env::temp_dir();
        let pid = std::process::id();
        let cert_path = dir.join(format!("fb_wss_{}.crt", pid));
        let key_path = dir.join(format!("fb_wss_{}.key", pid));
        std::fs::write(&cert_path, certified.cert.pem()).unwrap();
        std::fs::write(&key_path, certified.key_pair.serialize_pem()).unwrap();
        let tls_cfg = TlsConfig {
            cert_path: cert_path.to_string_lossy().into_owned(),
            key_path: key_path.to_string_lossy().into_owned(),
            min_version: "1.2".to_string(),
            client_ca_path: None,
            client_cert_required: true,
            sni_certs: Vec::new(),
        };
        // http2=false → ALPN advertises http/1.1 only, matching the WS handshake.
        let acceptor = tls::build_acceptor(&tls_cfg, false).unwrap();

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let acceptor = acceptor.clone();
                tokio::spawn(async move {
                    let tls_stream = match acceptor.accept(stream).await {
                        Ok(s) => s,
                        Err(_) => return,
                    };
                    let ws = match tokio_tungstenite::accept_async(tls_stream).await {
                        Ok(ws) => ws,
                        Err(_) => return,
                    };
                    let (mut tx, mut rx) = ws.split();
                    while let Some(Ok(msg)) = rx.next().await {
                        if msg.is_close() {
                            break;
                        }
                        if (msg.is_text() || msg.is_binary()) && tx.send(msg).await.is_err() {
                            break;
                        }
                    }
                });
            }
        });
        addr
    }

    /// Starts the data-plane accept loop on an ephemeral port (plaintext),
    /// returning its address. `http2` selects the auto builder (h1/h2 + h2
    /// extended CONNECT) vs. plain HTTP/1.1 with upgrades.
    async fn start_gateway(state: Arc<SharedState>, http2: bool) -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let (stream, peer) = listener.accept().await.unwrap();
                let state = state.clone();
                tokio::spawn(async move {
                    let service = service_fn(move |req| {
                        let state = state.clone();
                        async move { handle_request(req, peer, None, &state).await }
                    });
                    tls::serve_connection(TokioIo::new(stream), service, http2).await;
                });
            }
        });
        addr
    }

    #[tokio::test]
    async fn test_websocket_proxy_round_trip() {
        let echo = echo_ws_server().await;
        let gw = start_gateway(state_with_upstream("127.0.0.1", echo.port()), false).await;

        let url = format!("ws://127.0.0.1:{}/ws", gw.port());
        let (mut ws, resp) = tokio_tungstenite::connect_async(&url).await.unwrap();
        assert_eq!(resp.status(), 101);

        ws.send(Message::text("hello")).await.unwrap();
        let reply = ws.next().await.unwrap().unwrap();
        assert_eq!(reply.to_text().unwrap(), "hello");

        ws.send(Message::binary(vec![1u8, 2, 3])).await.unwrap();
        let reply = ws.next().await.unwrap().unwrap();
        assert_eq!(reply.into_data().to_vec(), vec![1u8, 2, 3]);

        ws.close(None).await.ok();
    }

    #[tokio::test]
    async fn test_websocket_proxy_tls_upstream_round_trip() {
        // Client connects plain ws:// to the gateway; the gateway relays to a
        // TLS-terminated (wss) upstream with verification off (self-signed).
        let echo = tls_echo_ws_server().await;
        let gw = start_gateway(state_with_tls_upstream("127.0.0.1", echo.port()), false).await;

        let url = format!("ws://127.0.0.1:{}/ws", gw.port());
        let (mut ws, resp) = tokio_tungstenite::connect_async(&url).await.unwrap();
        assert_eq!(resp.status(), 101);

        ws.send(Message::text("secure")).await.unwrap();
        let reply = ws.next().await.unwrap().unwrap();
        assert_eq!(reply.to_text().unwrap(), "secure");

        ws.send(Message::binary(vec![9u8, 8, 7])).await.unwrap();
        let reply = ws.next().await.unwrap().unwrap();
        assert_eq!(reply.into_data().to_vec(), vec![9u8, 8, 7]);

        ws.close(None).await.ok();
    }

    #[tokio::test]
    async fn test_websocket_proxy_h2_extended_connect() {
        use http_body_util::Empty;
        use tokio_tungstenite::tungstenite::protocol::Role;
        use tokio_tungstenite::WebSocketStream;

        // h1 WebSocket echo upstream; the gateway serves h2 (extended CONNECT)
        // to the client and bridges to this h1 upstream.
        let echo = echo_ws_server().await;
        let gw = start_gateway(state_with_upstream("127.0.0.1", echo.port()), true).await;

        // Plaintext h2c (prior-knowledge) client connection to the gateway.
        let tcp = TcpStream::connect(gw).await.unwrap();
        let (mut sender, conn) = hyper::client::conn::http2::handshake(
            hyper_util::rt::TokioExecutor::new(),
            TokioIo::new(tcp),
        )
        .await
        .unwrap();
        tokio::spawn(async move {
            let _ = conn.await;
        });

        // The client may only send extended CONNECT once the server's SETTINGS
        // (advertising it) have arrived. Wait for readiness, then give the
        // spawned connection task a brief moment to process the SETTINGS frame.
        sender.ready().await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        // Absolute-form URI so :scheme/:authority/:path are all present.
        let mut req = hyper::Request::builder()
            .method(hyper::Method::CONNECT)
            .uri(format!("http://127.0.0.1:{}/ws", gw.port()))
            .body(Empty::<Bytes>::new())
            .unwrap();
        req.extensions_mut()
            .insert(hyper::ext::Protocol::from_static("websocket"));

        let resp = sender.send_request(req).await.unwrap();
        // RFC 8441: a successful extended CONNECT is a 200, not a 101.
        assert_eq!(resp.status(), 200);

        // The tunnel IS the WebSocket data channel — wrap it directly (no
        // in-tunnel handshake), then exchange frames end-to-end to the upstream.
        let upgraded = hyper::upgrade::on(resp).await.unwrap();
        let mut ws =
            WebSocketStream::from_raw_socket(TokioIo::new(upgraded), Role::Client, None).await;

        ws.send(Message::text("h2-hello")).await.unwrap();
        let reply = ws.next().await.unwrap().unwrap();
        assert_eq!(reply.to_text().unwrap(), "h2-hello");

        ws.send(Message::binary(vec![4u8, 5, 6])).await.unwrap();
        let reply = ws.next().await.unwrap().unwrap();
        assert_eq!(reply.into_data().to_vec(), vec![4u8, 5, 6]);

        ws.close(None).await.ok();
    }

    #[tokio::test]
    async fn test_websocket_dead_upstream_502() {
        // Port 1 has nothing listening → the upstream handshake fails and the
        // gateway returns 502, so the client handshake never completes.
        let gw = start_gateway(state_with_upstream("127.0.0.1", 1), false).await;
        let url = format!("ws://127.0.0.1:{}/ws", gw.port());
        assert!(
            tokio_tungstenite::connect_async(&url).await.is_err(),
            "handshake must fail when the upstream is unreachable"
        );
    }
}
