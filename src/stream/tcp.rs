//! TCP stream proxy: accept a client, pick a load-balanced upstream (optionally
//! by TLS SNI), and relay bytes in both directions until either side closes.

use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;
use tracing::{debug, warn};

use crate::stream::sni::{extract_sni, SniResult, SniRouter};

/// Cap on how much of a ClientHello is buffered while looking for the SNI.
const MAX_CLIENT_HELLO: usize = 8 * 1024;

/// Binds the TCP listener (fail-fast) and spawns its accept loop, returning the
/// bound address (the OS-assigned port when `cfg.port == 0`). The loop stops
/// accepting when `shutdown_rx` flips to `true`. When `router` has SNI routes,
/// each connection's TLS ClientHello is peeked to pick a backend by hostname.
pub async fn spawn(
    cfg: &crate::config::StreamListenerConfig,
    router: Arc<SniRouter>,
    connect_timeout: Duration,
    mut shutdown_rx: watch::Receiver<bool>,
) -> io::Result<SocketAddr> {
    let ip = cfg
        .bind
        .parse()
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, format!("invalid bind: {}", e)))?;
    let addr = SocketAddr::new(ip, cfg.port);
    let listener = TcpListener::bind(addr).await?;
    let local = listener.local_addr()?;

    tokio::spawn(async move {
        loop {
            let (mut client, peer) = tokio::select! {
                accepted = listener.accept() => match accepted {
                    Ok(v) => v,
                    Err(e) => {
                        warn!("tcp stream accept error: {}", e);
                        continue;
                    }
                },
                _ = shutdown_rx.changed() => break,
            };
            let router = router.clone();
            tokio::spawn(async move {
                // Pick the backend pool. With SNI routes, peek the ClientHello
                // (consuming its bytes into `prebuffer`, which must be replayed
                // to the upstream to keep the passthrough handshake intact).
                let (balancer, prebuffer) = if router.has_sni_routes() {
                    let (sni, buf) = peek_sni(&mut client, connect_timeout).await;
                    (router.select(sni.as_deref()).clone(), buf)
                } else {
                    (router.select(None).clone(), Vec::new())
                };

                let idx = balancer.select(&peer.to_string());
                // Hold the in-flight guard for the connection's whole lifetime
                // so least-connections balancing reflects live streams.
                let _guard = balancer.owned_acquire(idx);
                let target = balancer.target(idx);
                let dest = (target.host.as_str(), target.port);

                match tokio::time::timeout(connect_timeout, TcpStream::connect(dest)).await {
                    Ok(Ok(mut upstream)) => {
                        // Replay the consumed ClientHello before relaying.
                        if !prebuffer.is_empty() {
                            if let Err(e) = upstream.write_all(&prebuffer).await {
                                warn!(
                                    "tcp stream replay to {}:{} failed: {}",
                                    target.host, target.port, e
                                );
                                return;
                            }
                        }
                        if let Err(e) =
                            tokio::io::copy_bidirectional(&mut client, &mut upstream).await
                        {
                            debug!("tcp stream relay closed: {}", e);
                        }
                    }
                    Ok(Err(e)) => {
                        warn!(
                            "tcp stream connect to {}:{} failed: {}",
                            target.host, target.port, e
                        )
                    }
                    Err(_) => warn!(
                        "tcp stream connect to {}:{} timed out",
                        target.host, target.port
                    ),
                }
            });
        }
    });

    Ok(local)
}

/// Reads (consuming) the start of the connection until the SNI is resolved,
/// EOF, the 8 KiB cap, or the timeout. Returns the hostname (if any) and the
/// bytes consumed so they can be replayed to the upstream. Any short / garbage
/// / timeout path yields `(None, buf)` so the caller uses the default pool.
async fn peek_sni(client: &mut TcpStream, timeout: Duration) -> (Option<String>, Vec<u8>) {
    let mut buf = Vec::with_capacity(1024);
    let deadline = tokio::time::Instant::now() + timeout;
    let mut tmp = [0u8; 4096];
    loop {
        match extract_sni(&buf) {
            SniResult::Found(host) => return (Some(host), buf),
            SniResult::NotPresent => return (None, buf),
            SniResult::Incomplete => {}
        }
        if buf.len() >= MAX_CLIENT_HELLO {
            return (None, buf);
        }
        match tokio::time::timeout_at(deadline, client.read(&mut tmp)).await {
            Ok(Ok(0)) => return (None, buf), // EOF
            Ok(Ok(n)) => {
                let take = n.min(MAX_CLIENT_HELLO - buf.len());
                buf.extend_from_slice(&tmp[..take]);
            }
            Ok(Err(_)) | Err(_) => return (None, buf), // read error or timeout
        }
    }
}
