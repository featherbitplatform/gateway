//! Data-plane listener: the hyper accept loop and per-request handler. It
//! serves plain HTTP or, when `system.tls` is set, TLS-terminated HTTPS; over
//! either transport it speaks HTTP/1.1 and (when `system.http2.enabled`)
//! HTTP/2, negotiated per connection. For each request it matches a route,
//! builds the [`Context`], executes the route's
//! [`CompiledGraph`](crate::graph::CompiledGraph), and writes `Context.response`
//! back to the client.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

use bytes::Bytes;
use http_body_util::combinators::BoxBody;
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
use crate::context::stream::ResponseStream;
use crate::context::{Context, GatewayRequest, Protocol};
use crate::outbound::BoxError;
use crate::routing::matches_route;
use crate::server::{tls, websocket};
use crate::state::SharedState;

/// Boxes a fixed `Full<Bytes>` body into the widened response body type used
/// throughout this module, so a buffered response composes with a streamed
/// one behind a single `Response<BoxBody<Bytes, BoxError>>` return type.
/// `Full`'s error is `Infallible`; the conversion can never actually error.
fn boxed_full(body: Bytes) -> BoxBody<Bytes, BoxError> {
    Full::new(body).map_err(|never| match never {}).boxed()
}

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
            // ACME: seed managed certs (stored or placeholder) before the config
            // is built, so the listener is up — and validatable — immediately.
            let hooks = match &system.acme {
                Some(acme_cfg) if !tls_cfg.managed_domains().is_empty() => {
                    let rt =
                        crate::acme::start(acme_cfg, tls_cfg, &state.resources, &state.metrics)
                            .await?;
                    state.acme.store(Some(rt.clone()));
                    Some(rt.hooks())
                }
                _ => None,
            };
            let shared = tls::build_reloadable(tls_cfg, http2_enabled, hooks.as_ref())?;
            tls::spawn_cert_watcher(
                tls_cfg.clone(),
                http2_enabled,
                shared.clone(),
                "data-plane",
                hooks,
            );
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
                                if tls::negotiated_acme_challenge(&tls_stream) {
                                    tracing::debug!("acme-tls/1 validation handshake from {}; closing", remote_addr);
                                    return;
                                }
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
) -> Result<Response<BoxBody<Bytes, BoxError>>, hyper::Error> {
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
                .body(boxed_full(Bytes::from(
                    r#"{"error": "not_found", "message": "No route matched"}"#,
                )))
                .unwrap());
        }
    };
    drop(routes); // release read lock before executing the graph

    let gateway_request = GatewayRequest::from_hyper(&parts, body_bytes, remote_addr);
    let mut ctx = Context::new(gateway_request);
    // Attribute this execution to its matched route/policy so plugins (e.g.
    // session metadata) can read them without a Context struct change.
    // Precedent: the `__client_cert_*` internal vars below. Set before the
    // graph runs so it is visible to every node, including this WebSocket
    // upgrade path, which shares this same Context-building seam.
    ctx.message.insert(
        "__route".to_string(),
        serde_json::Value::String(route_name.clone()),
    );
    ctx.message.insert(
        "__policy".to_string(),
        serde_json::Value::String(policy_name.clone()),
    );
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
            // Resolve the upstream TLS identity registered at policy compile.
            // A missing entry can only mean the key was hand-injected — fall
            // back to the plain connector (verification still applies).
            let ws_upstream_tls_key = result_ctx
                .message
                .get("__ws_upstream_tls_key")
                .and_then(|v| v.as_u64());
            let tls_identity = ws_upstream_tls_key.and_then(|key| {
                let identity = crate::outbound::tls::UpstreamTls::lookup(key);
                if identity.is_none() {
                    warn!(
                        "websocket upstream tls identity key {} not found; \
                         proceeding without client cert, backend may reject the handshake",
                        key
                    );
                }
                identity
            });
            let resp = match websocket::proxy_upgrade(
                host,
                port as u16,
                path,
                tls,
                verify,
                tls_identity,
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
            };
            // `proxy_upgrade`/`bad_gateway_502` return `Response<Full<Bytes>>`
            // (a 101 switching-protocols response has no body to stream); box
            // it into the widened body type this handler returns everywhere
            // else.
            return Ok(resp.map(|b| b.map_err(|never| match never {}).boxed()));
        }
    }

    Ok(build_response(
        status,
        &result_ctx.response.headers,
        trace_id.as_deref(),
        result_ctx.response.body,
        result_ctx.response.stream,
    ))
}

/// Builds the final client-facing response from the policy-graph result.
///
/// Header names/values here may be templated from request data (e.g. a
/// `request-id` `header_name` rendered from a missing header can yield an
/// empty name, or a templated value can carry a raw newline). `http`'s
/// `Response::builder()` defers name/value validation until `.body()` is
/// called, so an unvalidated pair would surface as a panic on an otherwise
/// normal request — unauthenticated and request-triggerable. Validate each
/// pair up front and drop only the offending one; failing the whole response
/// over one bad header would itself be a client-triggerable denial of
/// service. Once every header is pre-validated, `.body()` can only fail from
/// a builder-internal error (e.g. an earlier `.status()` rejection), so the
/// residual case falls back to a bare 500 rather than panicking the
/// connection.
fn build_response(
    status: u16,
    headers: &HashMap<String, Vec<String>>,
    trace_id: Option<&str>,
    body: Bytes,
    stream: Option<ResponseStream>,
) -> Response<BoxBody<Bytes, BoxError>> {
    let mut response_builder = Response::builder().status(status);

    for (key, values) in headers {
        let header_name = match hyper::header::HeaderName::try_from(key.as_str()) {
            Ok(name) => name,
            Err(e) => {
                warn!(
                    "dropping response header with invalid name {:?}: {}",
                    key, e
                );
                continue;
            }
        };
        for value in values {
            let header_value = match hyper::header::HeaderValue::try_from(value.as_str()) {
                Ok(v) => v,
                Err(e) => {
                    warn!(
                        "dropping response header {:?} with invalid value: {}",
                        key, e
                    );
                    continue;
                }
            };
            response_builder = response_builder.header(header_name.clone(), header_value);
        }
    }

    // Tell the caller which trace to fetch. Only ever set for a request that
    // opted in, so it reveals nothing to anyone who did not already know.
    if let Some(id) = trace_id {
        response_builder = response_builder.header("x-featherbit-trace-id", id);
    }

    // The invariant documented on `GatewayResponse.stream` is "when `stream`
    // is set, `body` is empty" — every producer of a stream (the `upstream`
    // node) and every writer of a generated error body
    // (`ErrorHandlerPlugin::execute`, the engine's no-handler 500 fallback,
    // the engine's `NODE_NOT_FOUND` fallback) is responsible for upholding
    // it. If it's violated, prefer the buffered `body` and drop the stream:
    // a policy's `error_handler` can be an arbitrary node id with no type
    // constraint, so a future or third-party node can write a generated body
    // without knowing to clear `response.stream`. Failing safe here means a
    // missed call site degrades to "the error page is sent, plus a warning"
    // instead of "the error page silently vanishes and a half-finished
    // stream goes out instead." Surface the anomaly loudly either way: warn
    // in every build (so a release build at least logs it instead of
    // silently swallowing the mismatch), and hard-fail in dev/test builds
    // where the bug should be caught before it ships.
    let out_body = if !body.is_empty() {
        if stream.is_some() {
            warn!(
                "response.stream and a non-empty response.body ({} bytes) were both \
                 set; the stream is being discarded in favor of the buffered body — \
                 this indicates a node wrote a generated body without clearing \
                 response.stream",
                body.len()
            );
        }
        debug_assert!(
            stream.is_none(),
            "response.stream and a non-empty response.body ({} bytes) must never both \
             be set — see GatewayResponse.stream's documented invariant",
            body.len()
        );
        boxed_full(body)
    } else {
        match stream {
            Some(s) => {
                let (body, guards) = s.into_parts();
                // Guards must outlive the body; attach them to it so they drop
                // when the response finishes streaming or the client disconnects.
                crate::outbound::idle::body_holding(body, guards)
            }
            None => boxed_full(body),
        }
    };

    response_builder.body(out_body).unwrap_or_else(|e| {
        error!("failed to build response: {}", e);
        Response::builder()
            .status(500)
            .body(boxed_full(Bytes::from_static(b"internal server error")))
            .expect("static 500 response must build")
    })
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
        std::fs::write(&key_path, certified.signing_key.serialize_pem()).unwrap();
        let tls_cfg = TlsConfig {
            cert_path: Some(cert_path.to_string_lossy().into_owned()),
            key_path: Some(key_path.to_string_lossy().into_owned()),
            min_version: "1.2".to_string(),
            client_ca_path: None,
            client_cert_required: true,
            sni_certs: Vec::new(),
            acme: None,
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

    #[tokio::test]
    async fn test_context_carries_route_and_policy_names() {
        // Every request's Context must carry its matched route/policy names
        // (`__route`/`__policy`) before the graph executes, so plugins (and
        // later, session metadata — see server-side-sessions Task 6+) can
        // read them. Assert via a plain HTTP route whose `echo` node renders
        // both message keys into response headers with `{{message...}}`
        // templates (precedent: the `__client_cert_*` internal vars).
        let gw_yaml = r#"
routes:
  - name: r1
    match: { path: /attribution }
    policy: p1
policies:
  - name: p1
    nodes:
      - { id: listener, type: listener }
      - { id: e, type: echo, config: { body: "ok", headers: { x-test-route: "{{message.__route}}", x-test-policy: "{{message.__policy}}" } } }
      - { id: client, type: client }
    edges:
      - { from: listener.out, to: e.in }
      - { from: e.success, to: client.in }
"#;
        let gw = start_gateway(build_state(gw_yaml), false).await;
        let url = format!("http://127.0.0.1:{}/attribution", gw.port());
        let resp = reqwest::Client::new().get(&url).send().await.unwrap();
        assert_eq!(
            resp.headers()
                .get("x-test-route")
                .map(|v| v.to_str().unwrap()),
            Some("r1")
        );
        assert_eq!(
            resp.headers()
                .get("x-test-policy")
                .map(|v| v.to_str().unwrap()),
            Some("p1")
        );
    }

    #[test]
    fn test_build_response_drops_invalid_headers_without_panicking() {
        // An empty header name (e.g. a templated `header_name` rendered from a
        // missing request header) and a value carrying a raw newline (header
        // splitting) are both rejected by `http`'s builder only once `.body()`
        // is called — `build_response` must validate eagerly, skip just the
        // offending pairs, and still deliver the response instead of
        // panicking the connection.
        let mut headers = HashMap::new();
        headers.insert("".to_string(), vec!["oops".to_string()]);
        headers.insert("x-bad-value".to_string(), vec!["line1\nline2".to_string()]);
        headers.insert("x-good".to_string(), vec!["fine".to_string()]);

        let resp = build_response(200, &headers, None, Bytes::from_static(b"ok"), None);

        assert_eq!(resp.status(), 200);
        assert!(
            !resp.headers().contains_key("x-bad-value"),
            "header with an invalid (newline-containing) value must be dropped"
        );
        assert_eq!(
            resp.headers().get("x-good").map(|v| v.to_str().unwrap()),
            Some("fine"),
            "well-formed headers must still be delivered"
        );
        // An empty name can never appear as a valid `HeaderName`, so there is
        // nothing to look up by key — the assertion above (only the good
        // header survives) already covers it, together with the fact this
        // call did not panic.
    }

    /// A buffered response must keep its exact `content-length` and bytes —
    /// widening the body type must be invisible to every non-streaming route.
    #[tokio::test]
    async fn test_buffered_response_is_unchanged() {
        let mut headers = HashMap::new();
        headers.insert("content-type".to_string(), vec!["text/plain".to_string()]);

        let resp = build_response(200, &headers, None, Bytes::from("hello"), None);

        assert_eq!(resp.status(), 200);
        let collected = resp.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(collected, Bytes::from("hello"));
    }

    /// A streamed response carries the stream's bytes through unaltered.
    ///
    /// This deliberately does NOT assert anything about `content-length`:
    /// `build_response` never sets that header itself (this test's `headers`
    /// map is empty, and the same absence would hold for a buffered body with
    /// an empty header map too, so it would prove nothing about streaming
    /// specifically) — and a streamed relay of a known-length upstream body
    /// can legitimately carry one anyway, since `BodyHolding`/`IdleTimeoutBody`
    /// both delegate `size_hint` to the wrapped body. The real guarantee is
    /// that the stream's bytes — not `body` — are what the client receives.
    #[tokio::test]
    async fn test_streamed_response_carries_stream_body() {
        let body = Full::new(Bytes::from("data: one\n\n"))
            .map_err(|never| match never {})
            .boxed();

        let resp = build_response(
            200,
            &HashMap::new(),
            None,
            Bytes::new(),
            Some(crate::context::stream::ResponseStream::new(body)),
        );

        let collected = resp.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(collected, Bytes::from("data: one\n\n"));
    }

    /// End-to-end confirmation that widening the body type did not change
    /// what a real (non-streaming) client receives on the wire: hyper still
    /// computes an exact `content-length` for a `Full`-backed body boxed into
    /// `BoxBody`, and the bytes are exactly the configured body.
    #[tokio::test]
    async fn test_buffered_route_is_byte_identical_on_the_wire() {
        let gw_yaml = r#"
routes:
  - name: r1
    match: { path: /plain }
    policy: p1
policies:
  - name: p1
    nodes:
      - { id: listener, type: listener }
      - { id: e, type: echo, config: { body: "hello world" } }
      - { id: client, type: client }
    edges:
      - { from: listener.out, to: e.in }
      - { from: e.success, to: client.in }
"#;
        let gw = start_gateway(build_state(gw_yaml), false).await;
        let url = format!("http://127.0.0.1:{}/plain", gw.port());
        let resp = reqwest::Client::new().get(&url).send().await.unwrap();
        assert_eq!(
            resp.headers()
                .get("content-length")
                .map(|v| v.to_str().unwrap()),
            Some("11"),
            "content-length must still be computed exactly for a buffered body"
        );
        let body = resp.text().await.unwrap();
        assert_eq!(body, "hello world");
    }

    /// `build_response` logs when both a stream and a non-empty buffered
    /// `body` are set — that combination must never reach this function in
    /// the first place (a node writing a generated error body without
    /// clearing `response.stream`), so it is self-policing at the choke
    /// point: it warns (so a release build at least logs the anomaly instead
    /// of silently swallowing it) and `debug_assert!`s (so a dev/test build
    /// fails loudly rather than shipping the bug). The body wins over the
    /// stream when this happens — see
    /// `test_build_response_prefers_body_over_stale_stream` for that
    /// precedence itself.
    ///
    /// Split into two tests because the two halves live in different
    /// profiles: `warn!` is unconditional and must stay covered in
    /// `cargo test --release` too, while `debug_assert!` compiles out
    /// entirely in release, so a single test asserting the panic would fail
    /// `--release` for a reason that has nothing to do with a real
    /// regression — see `test_build_response_panics_when_body_and_stream_both_set`
    /// below, which is gated to only exist where the assert does.
    #[test]
    fn test_build_response_warns_when_body_and_stream_both_set() {
        use std::panic::AssertUnwindSafe;

        let boxed = Full::new(Bytes::from_static(b"stream-bytes"))
            .map_err(|never| match never {})
            .boxed();
        let stream = Some(crate::context::stream::ResponseStream::new(boxed));
        let leftover_body = Bytes::from_static(b"leftover body");

        let (_guard, logs) = crate::test_log::capture_warnings();
        // `warn!` runs before `debug_assert!` inside `build_response`, so the
        // log is already captured regardless of whether this call goes on to
        // panic (debug/test builds) or return normally (release builds) —
        // `catch_unwind` here only tolerates whichever of those happens.
        let _ = std::panic::catch_unwind(AssertUnwindSafe(|| {
            build_response(200, &HashMap::new(), None, leftover_body, stream)
        }));

        let out = logs.contents();
        assert!(out.contains("WARN"), "expected a WARN line, got: {out:?}");
        assert!(
            out.contains("13"),
            "warning should name the buffered body's length (13 bytes), got: {out:?}"
        );
    }

    /// The `debug_assert!` half of the same guard — only compiled where the
    /// assert itself is. Ungated, this failed `cargo test --release`
    /// (`debug_assert!` is a no-op there, so `build_response` would simply
    /// return instead of panicking) even though nothing was actually wrong.
    #[cfg(debug_assertions)]
    #[test]
    fn test_build_response_panics_when_body_and_stream_both_set() {
        use std::panic::AssertUnwindSafe;

        let boxed = Full::new(Bytes::from_static(b"stream-bytes"))
            .map_err(|never| match never {})
            .boxed();
        let stream = Some(crate::context::stream::ResponseStream::new(boxed));
        let leftover_body = Bytes::from_static(b"leftover body");

        let (_guard, _logs) = crate::test_log::capture_warnings();
        let result = std::panic::catch_unwind(AssertUnwindSafe(|| {
            build_response(200, &HashMap::new(), None, leftover_body, stream)
        }));

        assert!(
            result.is_err(),
            "debug_assert! must fire when response.stream and a non-empty response.body are both set"
        );
    }

    /// The precedence itself: when both a generated `body` and a stale
    /// `response.stream` are set, the client must receive the generated
    /// body, not the stream. This is what makes a missed stream-clear call
    /// site fail safe (an error page still reaches the client, alongside the
    /// warning) instead of silently vanishing behind a half-finished
    /// upstream stream.
    ///
    /// Gated the same way as `test_build_response_panics_when_body_and_stream_both_set`
    /// above and for the same reason, just the opposite half: in a dev/test
    /// build `debug_assert!` panics before `build_response` can return
    /// anything to inspect, so the only build where this function's return
    /// value is observable for this input is release, where the assert
    /// compiles out.
    #[cfg(not(debug_assertions))]
    #[tokio::test]
    async fn test_build_response_prefers_body_over_stale_stream() {
        let boxed = Full::new(Bytes::from_static(b"stale-stream-bytes"))
            .map_err(|never| match never {})
            .boxed();
        let stream = Some(crate::context::stream::ResponseStream::new(boxed));
        let generated_body = Bytes::from_static(b"generated error body");

        let (_guard, _logs) = crate::test_log::capture_warnings();
        let resp = build_response(500, &HashMap::new(), None, generated_body.clone(), stream);

        let collected = resp.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(
            collected, generated_body,
            "the generated body must win over a stale stream, not be silently discarded"
        );
    }

    // ------------------------------------------------------------------
    // Streaming responses: the end-to-end proof, plus two wire-level
    // properties nothing above has ever observed (real TCP framing, and
    // what upstream's verbatim header copy actually puts on the wire).
    // ------------------------------------------------------------------

    /// Builds a full gateway config from a bare `{nodes, edges}` policy value
    /// (the shape every test in this section writes) and starts the
    /// data-plane accept loop on an ephemeral port. The single route always
    /// matches `/stream`, since every caller only ever needs one.
    async fn spawn_gateway_with_policy(policy: serde_json::Value) -> u16 {
        let gateway_cfg = serde_json::json!({
            "routes": [
                { "name": "r1", "match": { "path": "/stream" }, "policy": "p1" }
            ],
            "policies": [
                {
                    "name": "p1",
                    "nodes": policy["nodes"],
                    "edges": policy["edges"],
                }
            ]
        });
        // JSON is valid YAML, so this reuses `build_state`'s YAML parser
        // unchanged rather than needing a second config-loading path.
        let gw_yaml = serde_json::to_string(&gateway_cfg).expect("policy JSON must serialize");
        let state = build_state(&gw_yaml);
        start_gateway(state, false).await.port()
    }

    /// The position of the first occurrence of `needle` in `haystack`, at
    /// byte granularity — `str::find` would require the whole buffer to be
    /// valid UTF-8, which raw chunk-framed bytes are not guaranteed to be
    /// this test only needs the header block located before decoding it.
    fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
        haystack
            .windows(needle.len())
            .position(|window| window == needle)
    }

    /// Opens a plain (non-pooling, non-buffering) TCP connection to the
    /// gateway and issues a bare HTTP/1.1 GET. Deliberately not `reqwest`:
    /// every property this section checks — an event arriving mid-stream,
    /// exact chunk framing, exact header text — requires seeing bytes as
    /// they land on the socket, not after a client library has reassembled
    /// them into a `Response`.
    async fn raw_get(port: u16, path: &str) -> TcpStream {
        use tokio::io::AsyncWriteExt;
        let mut stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        stream
            .write_all(
                format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n")
                    .as_bytes(),
            )
            .await
            .unwrap();
        stream
    }

    /// Reads raw bytes from `stream` until a complete SSE event (a line
    /// followed by a blank line) has arrived, and returns just that line —
    /// e.g. `"data: first"`. Reading is byte-by-byte off the socket as it
    /// arrives; nothing buffers the whole response first.
    async fn read_first_sse_event(port: u16, path: &str) -> String {
        use tokio::io::AsyncReadExt;
        let mut stream = raw_get(port, path).await;
        let mut acc = Vec::new();
        let mut buf = [0u8; 4096];
        loop {
            let n = stream.read(&mut buf).await.unwrap();
            assert!(n > 0, "connection closed before an SSE event arrived");
            acc.extend_from_slice(&buf[..n]);
            let text = String::from_utf8_lossy(&acc);
            if let Some(idx) = text.find("data: first") {
                let line = text[idx..].lines().next().unwrap();
                return line.to_string();
            }
        }
    }

    /// The whole point of the feature: an SSE event must reach the client
    /// before the upstream closes the response. Today the gateway buffers, so
    /// the client sees nothing until the server hangs up — this test fails on
    /// any buffering implementation, which is exactly what makes it worth having.
    #[tokio::test]
    async fn test_sse_event_arrives_before_upstream_closes() {
        // Upstream: sends one event, waits 10s, then closes.
        let upstream = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let up_port = upstream.local_addr().unwrap().port();
        tokio::spawn(async move {
            if let Ok((mut s, _)) = upstream.accept().await {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                let mut buf = [0u8; 4096];
                let _ = s.read(&mut buf).await;
                let _ = s
                    .write_all(
                        b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\
                      transfer-encoding: chunked\r\n\r\nd\r\ndata: first\n\n\r\n",
                    )
                    .await;
                tokio::time::sleep(std::time::Duration::from_secs(10)).await;
            }
        });

        // Gateway: listener -> upstream -> header-only response-rewrite -> client.
        let gw_port = spawn_gateway_with_policy(serde_json::json!({
            "nodes": [
                { "id": "listener", "type": "listener", "config": {} },
                { "id": "up", "type": "upstream",
                  "config": { "targets": [{ "host": "127.0.0.1", "port": up_port }] } },
                { "id": "hdr", "type": "response-rewrite",
                  "config": { "headers": { "set": { "x-gw": "1" } } } },
                { "id": "client", "type": "client", "config": {} }
            ],
            "edges": [
                { "from": "listener.out", "to": "up.in" },
                { "from": "up.success", "to": "hdr.in" },
                { "from": "hdr.success", "to": "client.in" }
            ]
        }))
        .await;

        // Read the first event with a 3s bound — well inside the upstream's 10s hold.
        let got = tokio::time::timeout(
            std::time::Duration::from_secs(3),
            read_first_sse_event(gw_port, "/stream"),
        )
        .await
        .expect("first event must arrive while the upstream is still open");

        assert_eq!(got, "data: first");
    }

    /// A stream that goes silent — no more frames, and the upstream never
    /// closes either — must be reaped after `stream_idle_timeout_ms` rather
    /// than held open indefinitely. The idle bound is set well below the
    /// upstream's 10s hold, so only the reap (never upstream EOF) can be
    /// what ends the connection within this test's bound.
    #[tokio::test]
    async fn test_stream_idle_timeout_reaps_a_silent_stream() {
        let upstream = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let up_port = upstream.local_addr().unwrap().port();
        tokio::spawn(async move {
            if let Ok((mut s, _)) = upstream.accept().await {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                let mut buf = [0u8; 4096];
                let _ = s.read(&mut buf).await;
                let _ = s
                    .write_all(
                        b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\
                      transfer-encoding: chunked\r\n\r\nd\r\ndata: only\n\n\r\n",
                    )
                    .await;
                // Go silent. The connection is held open far past the idle
                // bound below, so the client must see it close on its own.
                tokio::time::sleep(std::time::Duration::from_secs(10)).await;
            }
        });

        let gw_port = spawn_gateway_with_policy(serde_json::json!({
            "nodes": [
                { "id": "listener", "type": "listener", "config": {} },
                { "id": "up", "type": "upstream",
                  "config": { "targets": [{ "host": "127.0.0.1", "port": up_port }],
                              "stream_idle_timeout_ms": 300 } },
                { "id": "client", "type": "client", "config": {} }
            ],
            "edges": [
                { "from": "listener.out", "to": "up.in" },
                { "from": "up.success", "to": "client.in" }
            ]
        }))
        .await;

        use tokio::io::AsyncReadExt;
        let mut stream = raw_get(gw_port, "/stream").await;

        let outcome = tokio::time::timeout(std::time::Duration::from_secs(3), async {
            let mut buf = [0u8; 4096];
            let mut saw_data = false;
            loop {
                match stream.read(&mut buf).await {
                    Ok(0) => return saw_data,
                    Ok(_) => saw_data = true,
                    Err(_) => return saw_data,
                }
            }
        })
        .await
        .expect("the idle-timed-out stream must close well before the upstream's own 10s hold");

        assert!(
            outcome,
            "the one event sent before the upstream went silent must still have arrived"
        );
    }

    /// A stream sending an event every 200ms must survive well past a
    /// deliberately short `timeout_ms` — proof that, once a response is
    /// streaming, the whole-call deadline no longer covers the body (it
    /// bounded only connect + request + headers).
    #[tokio::test]
    async fn test_streamed_response_survives_past_short_timeout_ms() {
        let upstream = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let up_port = upstream.local_addr().unwrap().port();
        tokio::spawn(async move {
            if let Ok((mut s, _)) = upstream.accept().await {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                let mut buf = [0u8; 4096];
                let _ = s.read(&mut buf).await;
                let _ = s
                    .write_all(
                        b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\
                          transfer-encoding: chunked\r\n\r\n",
                    )
                    .await;
                for i in 0..5u32 {
                    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                    let payload = format!("data: evt{i}\n\n");
                    let chunk = format!("{:x}\r\n{}\r\n", payload.len(), payload);
                    if s.write_all(chunk.as_bytes()).await.is_err() {
                        break;
                    }
                }
                let _ = s.write_all(b"0\r\n\r\n").await;
            }
        });

        let gw_port = spawn_gateway_with_policy(serde_json::json!({
            "nodes": [
                { "id": "listener", "type": "listener", "config": {} },
                { "id": "up", "type": "upstream",
                  "config": { "targets": [{ "host": "127.0.0.1", "port": up_port }],
                              "timeout_ms": 50 } },
                { "id": "client", "type": "client", "config": {} }
            ],
            "edges": [
                { "from": "listener.out", "to": "up.in" },
                { "from": "up.success", "to": "client.in" }
            ]
        }))
        .await;

        let start = std::time::Instant::now();
        let body = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            use tokio::io::AsyncReadExt;
            let mut stream = raw_get(gw_port, "/stream").await;
            let mut acc = Vec::new();
            let mut buf = [0u8; 4096];
            loop {
                let n = stream.read(&mut buf).await.unwrap();
                if n == 0 {
                    break;
                }
                acc.extend_from_slice(&buf[..n]);
            }
            acc
        })
        .await
        .expect("the streamed body must complete despite a 50ms timeout_ms");
        let elapsed = start.elapsed();

        let text = String::from_utf8_lossy(&body);
        for i in 0..5u32 {
            assert!(
                text.contains(&format!("data: evt{i}")),
                "expected event {i} in the body, got: {text:?}"
            );
        }
        assert!(
            elapsed >= std::time::Duration::from_millis(900),
            "body took only {elapsed:?} — the 5x200ms upstream cadence should dominate, \
             not a 50ms request deadline that (pre-streaming) would have killed this early"
        );
    }

    /// Wire-level proof that a streamed response is genuinely chunk-framed
    /// and carries no `content-length` — every test above this one collects
    /// the body in-process (via `reqwest` or a raw socket read to EOF), so
    /// none of them has ever looked at what actually goes out on the wire.
    /// The upstream here deliberately sends neither `content-length` nor
    /// `transfer-encoding` (a close-delimited body of unknown length), so
    /// this isolates the gateway's *own* framing choice from the separate
    /// header-passthrough question the next test pins.
    #[tokio::test]
    async fn test_streamed_response_is_chunked_with_no_content_length_on_the_wire() {
        let upstream = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let up_port = upstream.local_addr().unwrap().port();
        tokio::spawn(async move {
            if let Ok((mut s, _)) = upstream.accept().await {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                let mut buf = [0u8; 4096];
                let _ = s.read(&mut buf).await;
                let _ = s
                    .write_all(
                        b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\r\n\
                          data: first\n\n",
                    )
                    .await;
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            }
        });

        let gw_port = spawn_gateway_with_policy(serde_json::json!({
            "nodes": [
                { "id": "listener", "type": "listener", "config": {} },
                { "id": "up", "type": "upstream",
                  "config": { "targets": [{ "host": "127.0.0.1", "port": up_port }] } },
                { "id": "client", "type": "client", "config": {} }
            ],
            "edges": [
                { "from": "listener.out", "to": "up.in" },
                { "from": "up.success", "to": "client.in" }
            ]
        }))
        .await;

        use tokio::io::AsyncReadExt;
        let mut stream = raw_get(gw_port, "/stream").await;

        let mut acc = Vec::new();
        let mut buf = [0u8; 4096];
        let header_end = loop {
            let n = stream.read(&mut buf).await.unwrap();
            assert!(n > 0, "connection closed before headers arrived");
            acc.extend_from_slice(&buf[..n]);
            if let Some(pos) = find_subslice(&acc, b"\r\n\r\n") {
                break pos + 4;
            }
        };

        let header_text = String::from_utf8_lossy(&acc[..header_end]).to_lowercase();
        assert!(
            header_text.contains("transfer-encoding: chunked"),
            "a streamed response of unknown length must be chunk-framed on the wire; \
             got headers:\n{header_text}"
        );
        assert!(
            !header_text.contains("content-length:"),
            "a chunked response must not also carry content-length; got headers:\n{header_text}"
        );

        // Read on into the body and confirm real chunk framing: a hex
        // chunk-size line, CRLF, then the chunk's own bytes.
        while acc.len() < header_end + 4 {
            let n = stream.read(&mut buf).await.unwrap();
            assert!(n > 0, "connection closed before any chunk arrived");
            acc.extend_from_slice(&buf[..n]);
        }
        let body_text = String::from_utf8_lossy(&acc[header_end..]);
        let size_line = body_text.lines().next().unwrap();
        assert!(
            u64::from_str_radix(size_line.trim(), 16).is_ok(),
            "the first body line on the wire must be a valid hex chunk-size, got {size_line:?}"
        );
    }

    /// The symmetric case to the test above: when the upstream declares a
    /// `content-length`, that header is copied through unchanged and hyper's
    /// H1 encoder trusts it — the response stays length-delimited, not
    /// chunked. Streaming is decided purely by graph shape (§5 of the design
    /// doc), not by upstream framing, so an ordinary known-length response
    /// behind header-only nodes still streams frame-by-frame as it arrives;
    /// it just doesn't get chunked on the wire the way an unknown-length one
    /// does. Pinned here because the docs previously claimed, incorrectly,
    /// that a streamed response never carries `content-length`.
    #[tokio::test]
    async fn test_streamed_response_with_known_content_length_is_length_delimited_on_the_wire() {
        let payload = b"data: first\n\n";
        let upstream = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let up_port = upstream.local_addr().unwrap().port();
        tokio::spawn(async move {
            if let Ok((mut s, _)) = upstream.accept().await {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                let mut buf = [0u8; 4096];
                let _ = s.read(&mut buf).await;
                let header = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\n\r\n",
                    payload.len()
                );
                let _ = s.write_all(header.as_bytes()).await;
                let _ = s.write_all(payload).await;
                // Hold the connection open past the body: with an exact
                // content-length, the client must stop reading at that many
                // bytes on its own, not because the connection closed.
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            }
        });

        let gw_port = spawn_gateway_with_policy(serde_json::json!({
            "nodes": [
                { "id": "listener", "type": "listener", "config": {} },
                { "id": "up", "type": "upstream",
                  "config": { "targets": [{ "host": "127.0.0.1", "port": up_port }] } },
                { "id": "client", "type": "client", "config": {} }
            ],
            "edges": [
                { "from": "listener.out", "to": "up.in" },
                { "from": "up.success", "to": "client.in" }
            ]
        }))
        .await;

        use tokio::io::AsyncReadExt;
        let mut stream = raw_get(gw_port, "/stream").await;

        let mut acc = Vec::new();
        let mut buf = [0u8; 4096];
        let header_end = loop {
            let n = stream.read(&mut buf).await.unwrap();
            assert!(n > 0, "connection closed before headers arrived");
            acc.extend_from_slice(&buf[..n]);
            if let Some(pos) = find_subslice(&acc, b"\r\n\r\n") {
                break pos + 4;
            }
        };

        let header_text = String::from_utf8_lossy(&acc[..header_end]).to_lowercase();
        assert!(
            header_text.contains(&format!("content-length: {}", payload.len())),
            "the upstream's content-length must be passed through unchanged; got headers:\n{header_text}"
        );
        assert!(
            !header_text.contains("transfer-encoding:"),
            "a length-delimited response must not also be chunked; got headers:\n{header_text}"
        );

        // Read exactly `payload.len()` more bytes — bounded, so this hangs
        // (and the test times out) if the gateway is chunking after all.
        let want = header_end + payload.len();
        let body = tokio::time::timeout(std::time::Duration::from_secs(3), async {
            while acc.len() < want {
                let n = stream.read(&mut buf).await.unwrap();
                assert!(n > 0, "connection closed before the full body arrived");
                acc.extend_from_slice(&buf[..n]);
            }
            acc[header_end..want].to_vec()
        })
        .await
        .expect("the length-delimited body must arrive without waiting on connection close");

        assert_eq!(
            body, payload,
            "the raw bytes on the wire must be the payload itself, with no chunk-size line \
             or other framing wrapped around it"
        );
    }

    /// Pins what the client actually receives when the upstream's own
    /// `transfer-encoding: chunked` header is copied through verbatim by
    /// `upstream.rs`'s streaming branch (`ctx.response.headers = resp.headers;`,
    /// around line 391) — no code anywhere strips hop-by-hop framing headers
    /// before they reach `build_response`. This was inert while every
    /// response was buffered (hyper always recomputed framing for a `Full`
    /// body regardless of what headers happened to be set); a streaming
    /// relay makes it newly observable, since the gateway's own outbound
    /// body is now unbounded too and hyper again wants to pick the framing.
    ///
    /// Finding (see the task report for the full writeup): this turns out to
    /// be harmless, not a bug. Hyper's outbound `Incoming` body already
    /// de-chunks the upstream's bytes (`resp.body` carries the plain
    /// payload, not raw chunk envelopes), and when hyper's H1 server codec
    /// then writes that unbounded `BoxBody` out to the client, it sees the
    /// copied-through `transfer-encoding: chunked` header, agrees with it
    /// (no `content-length`, HTTP/1.1, unknown size), performs the actual
    /// chunk-encoding itself, and does not add a second header. Captured on
    /// the wire for exactly this fixture:
    /// `"...transfer-encoding: chunked\r\n...\r\n\r\nD\r\ndata: first\n\n\r\n0\r\n\r\n"`
    /// — one header, and a genuinely valid chunk frame (`D` = 13 = the byte
    /// length of `"data: first\n\n"`) followed by the terminal `0\r\n\r\n`.
    /// Nothing to fix here; a "fix" (stripping hop-by-hop headers before
    /// `build_response`) would also touch the buffered path's byte-identical
    /// guarantee and is out of scope for this task regardless.
    #[tokio::test]
    async fn test_upstream_transfer_encoding_header_is_copied_through_verbatim() {
        let upstream = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let up_port = upstream.local_addr().unwrap().port();
        tokio::spawn(async move {
            if let Ok((mut s, _)) = upstream.accept().await {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                let mut buf = [0u8; 4096];
                let _ = s.read(&mut buf).await;
                let _ = s
                    .write_all(
                        b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\
                      transfer-encoding: chunked\r\n\r\nd\r\ndata: first\n\n\r\n0\r\n\r\n",
                    )
                    .await;
            }
        });

        let gw_port = spawn_gateway_with_policy(serde_json::json!({
            "nodes": [
                { "id": "listener", "type": "listener", "config": {} },
                { "id": "up", "type": "upstream",
                  "config": { "targets": [{ "host": "127.0.0.1", "port": up_port }] } },
                { "id": "client", "type": "client", "config": {} }
            ],
            "edges": [
                { "from": "listener.out", "to": "up.in" },
                { "from": "up.success", "to": "client.in" }
            ]
        }))
        .await;

        use tokio::io::AsyncReadExt;
        let mut stream = raw_get(gw_port, "/stream").await;
        let mut acc = Vec::new();
        let mut buf = [0u8; 4096];
        loop {
            match tokio::time::timeout(std::time::Duration::from_secs(3), stream.read(&mut buf))
                .await
            {
                Ok(Ok(0)) | Err(_) => break,
                Ok(Ok(n)) => acc.extend_from_slice(&buf[..n]),
                Ok(Err(_)) => break,
            }
        }

        let header_end = find_subslice(&acc, b"\r\n\r\n")
            .map(|pos| pos + 4)
            .expect("response must have a complete header block");
        let header_text = String::from_utf8_lossy(&acc[..header_end]).to_lowercase();

        // Pin the actual, current behavior: the upstream's header is copied
        // through exactly once (upstream.rs does not itself duplicate it —
        // hyper, seeing transfer-encoding already present, does not add a
        // second one), so the count is 1, not 0 (stripped) or 2 (doubled).
        let te_count = header_text.matches("transfer-encoding:").count();
        assert_eq!(
            te_count, 1,
            "expected the upstream's transfer-encoding header to be copied through exactly \
             once; got {te_count} occurrences in:\n{header_text}"
        );
        assert!(
            header_text.contains("transfer-encoding: chunked"),
            "got headers:\n{header_text}"
        );

        // And pin that the body is not merely *labeled* chunked but is
        // actually, validly chunk-framed: a hex size line for the payload's
        // exact byte length, the payload itself, and the terminal 0-chunk —
        // not, say, the plain unframed bytes a naive pass-through of an
        // already-dechunked `Incoming` body might have produced.
        let body = &acc[header_end..];
        let payload = b"data: first\n\n";
        let expected = format!("{:X}\r\n", payload.len());
        assert!(
            body.starts_with(expected.as_bytes()),
            "expected the chunk-size line {:?}, got {:?}",
            expected,
            String::from_utf8_lossy(&body[..body.len().min(16)])
        );
        assert!(
            body.ends_with(b"0\r\n\r\n"),
            "expected the response to end with the terminal chunk, got {:?}",
            String::from_utf8_lossy(body)
        );
        assert!(
            find_subslice(body, payload).is_some(),
            "expected the upstream's payload bytes intact in the body, got {:?}",
            String::from_utf8_lossy(body)
        );
    }
}
