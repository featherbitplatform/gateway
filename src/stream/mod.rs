//! L4 (TCP/UDP) stream proxying — a data path independent of the HTTP engine.
//!
//! Each configured [`StreamListenerConfig`] binds a port at startup and relays
//! raw bytes to an upstream pool selected by the shared [`Balancer`]. There is
//! no `Context`, no node-graph, and no per-request matching: a listener maps a
//! port straight to a backend pool. TCP uses `copy_bidirectional`; UDP tracks
//! per-client sessions (see [`tcp`] and [`udp`]).

pub(crate) mod sni;
mod tcp;
mod udp;

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::watch;
use tracing::{info, warn};

use crate::balancer::{Balancer, Strategy};
use crate::config::{StreamListenerConfig, StreamProtocol, TimeoutConfig};
use crate::stream::sni::SniRouter;

/// Binds every configured stream listener and spawns its accept/receive loop.
///
/// Binding is fail-fast: the first bind error (or invalid config) is returned
/// so startup can abort with a clear message. On success every listener is
/// bound and running in its own detached task, and this returns immediately.
/// Each loop stops accepting when `shutdown_rx` flips to `true` (in-flight
/// relays/sessions are then dropped at process exit).
pub async fn start_all(
    streams: &[StreamListenerConfig],
    timeouts: &TimeoutConfig,
    shutdown_rx: watch::Receiver<bool>,
) -> Result<(), String> {
    let connect_timeout = Duration::from_secs(timeouts.connection_seconds);
    let idle = Duration::from_secs(timeouts.idle_seconds);

    for cfg in streams {
        let strategy = match &cfg.upstream.load_balancing {
            Some(s) => Strategy::parse(s)?,
            None => Strategy::default(),
        };
        let balancer = Arc::new(Balancer::new(cfg.upstream.targets.clone(), strategy)?);

        let addr = match cfg.protocol {
            StreamProtocol::Tcp => {
                // Build one pool per SNI route; `balancer` is the default.
                let mut routes = Vec::with_capacity(cfg.sni_routes.len());
                for route in &cfg.sni_routes {
                    let route_strategy = match &route.upstream.load_balancing {
                        Some(s) => Strategy::parse(s)?,
                        None => Strategy::default(),
                    };
                    let pool = Arc::new(Balancer::new(
                        route.upstream.targets.clone(),
                        route_strategy,
                    )?);
                    routes.push((route.server_name.clone(), pool));
                }
                let router = Arc::new(SniRouter::new(routes, balancer));
                tcp::spawn(cfg, router, connect_timeout, shutdown_rx.clone())
                    .await
                    .map_err(|e| {
                        format!("failed to bind tcp stream {}:{}: {}", cfg.bind, cfg.port, e)
                    })?
            }
            StreamProtocol::Udp => {
                if !cfg.sni_routes.is_empty() {
                    warn!(
                        "sni_routes set on UDP stream {}:{} — ignored (TCP only)",
                        cfg.bind, cfg.port
                    );
                }
                udp::spawn(cfg, balancer, idle, shutdown_rx.clone())
                    .await
                    .map_err(|e| {
                        format!("failed to bind udp stream {}:{}: {}", cfg.bind, cfg.port, e)
                    })?
            }
        };

        let proto = match cfg.protocol {
            StreamProtocol::Tcp => "tcp",
            StreamProtocol::Udp => "udp",
        };
        info!("Stream ({}) listening on {}", proto, addr);
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::balancer::Target;
    use crate::config::{StreamProtocol, StreamUpstreamConfig};
    use std::net::SocketAddr;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream, UdpSocket};

    fn stream_cfg(protocol: StreamProtocol, target: SocketAddr) -> StreamListenerConfig {
        StreamListenerConfig {
            protocol,
            bind: "127.0.0.1".to_string(),
            port: 0,
            upstream: StreamUpstreamConfig {
                targets: vec![Target {
                    host: target.ip().to_string(),
                    port: target.port(),
                }],
                load_balancing: None,
            },
            sni_routes: Vec::new(),
        }
    }

    fn balancer_for(cfg: &StreamListenerConfig) -> Arc<Balancer> {
        Arc::new(Balancer::new(cfg.upstream.targets.clone(), Strategy::RoundRobin).unwrap())
    }

    /// A router with no SNI routes (default pool only), for the plain TCP tests.
    fn router_for(cfg: &StreamListenerConfig) -> Arc<SniRouter> {
        Arc::new(SniRouter::new(Vec::new(), balancer_for(cfg)))
    }

    /// A TCP echo server; returns its bound address.
    async fn tcp_echo() -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                tokio::spawn(async move {
                    let (mut r, mut w) = sock.split();
                    let _ = tokio::io::copy(&mut r, &mut w).await;
                });
            }
        });
        addr
    }

    /// A UDP echo server; returns its bound address.
    async fn udp_echo() -> SocketAddr {
        let sock = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let addr = sock.local_addr().unwrap();
        tokio::spawn(async move {
            let mut buf = [0u8; 2048];
            while let Ok((n, peer)) = sock.recv_from(&mut buf).await {
                let _ = sock.send_to(&buf[..n], peer).await;
            }
        });
        addr
    }

    #[tokio::test]
    async fn test_tcp_proxy_round_trip() {
        let echo = tcp_echo().await;
        let cfg = stream_cfg(StreamProtocol::Tcp, echo);
        let (_tx, rx) = watch::channel(false);
        let proxy = tcp::spawn(&cfg, router_for(&cfg), Duration::from_secs(5), rx)
            .await
            .unwrap();

        let mut client = TcpStream::connect(proxy).await.unwrap();
        client.write_all(b"hello").await.unwrap();
        let mut buf = [0u8; 5];
        client.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"hello");

        // A second frame confirms the relay stays open.
        client.write_all(b"world").await.unwrap();
        client.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"world");
    }

    #[tokio::test]
    async fn test_udp_proxy_round_trip_and_session_reuse() {
        let echo = udp_echo().await;
        let cfg = stream_cfg(StreamProtocol::Udp, echo);
        let (_tx, rx) = watch::channel(false);
        let proxy = udp::spawn(&cfg, balancer_for(&cfg), Duration::from_secs(2), rx)
            .await
            .unwrap();

        let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        client.send_to(b"ping", proxy).await.unwrap();
        let mut buf = [0u8; 16];
        let (n, _) = tokio::time::timeout(Duration::from_secs(1), client.recv_from(&mut buf))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(&buf[..n], b"ping");

        // A second datagram from the same client reuses the session.
        client.send_to(b"pong", proxy).await.unwrap();
        let (n, _) = tokio::time::timeout(Duration::from_secs(1), client.recv_from(&mut buf))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(&buf[..n], b"pong");
    }

    /// A backend that reads its first bytes (the replayed ClientHello — proving
    /// passthrough) then writes back a one-byte id and echoes.
    async fn id_backend(id: u8) -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                tokio::spawn(async move {
                    let mut buf = [0u8; 512];
                    if sock.read(&mut buf).await.unwrap_or(0) > 0 {
                        let _ = sock.write_all(&[id]).await;
                    }
                });
            }
        });
        addr
    }

    /// Builds a minimal, well-formed TLS ClientHello record carrying `sni`.
    fn client_hello(sni: &str) -> Vec<u8> {
        let name = sni.as_bytes();
        let mut sni_body = Vec::new();
        sni_body.extend_from_slice(&((1 + 2 + name.len()) as u16).to_be_bytes());
        sni_body.push(0x00);
        sni_body.extend_from_slice(&(name.len() as u16).to_be_bytes());
        sni_body.extend_from_slice(name);
        let mut ext = Vec::new();
        ext.extend_from_slice(&0x0000u16.to_be_bytes());
        ext.extend_from_slice(&(sni_body.len() as u16).to_be_bytes());
        ext.extend_from_slice(&sni_body);
        let mut body = Vec::new();
        body.extend_from_slice(&[0x03, 0x03]);
        body.extend_from_slice(&[0u8; 32]);
        body.push(0x00);
        body.extend_from_slice(&2u16.to_be_bytes());
        body.extend_from_slice(&[0x00, 0x2f]);
        body.push(0x01);
        body.push(0x00);
        body.extend_from_slice(&(ext.len() as u16).to_be_bytes());
        body.extend_from_slice(&ext);
        let mut hs = vec![0x01];
        hs.extend_from_slice(&(body.len() as u32).to_be_bytes()[1..]);
        hs.extend_from_slice(&body);
        let mut rec = vec![0x16, 0x03, 0x01];
        rec.extend_from_slice(&(hs.len() as u16).to_be_bytes());
        rec.extend_from_slice(&hs);
        rec
    }

    async fn probe(proxy: SocketAddr, first: &[u8]) -> u8 {
        let mut c = TcpStream::connect(proxy).await.unwrap();
        c.write_all(first).await.unwrap();
        let mut id = [0u8; 1];
        c.read_exact(&mut id).await.unwrap();
        id[0]
    }

    #[tokio::test]
    async fn test_sni_routing_selects_backend_and_replays() {
        use crate::config::SniRoute;

        let backend_a = id_backend(b'A').await;
        let backend_b = id_backend(b'B').await;

        // Default -> B; a.example.com -> A.
        let mut cfg = stream_cfg(StreamProtocol::Tcp, backend_b);
        cfg.sni_routes = vec![SniRoute {
            server_name: "a.example.com".to_string(),
            upstream: StreamUpstreamConfig {
                targets: vec![Target {
                    host: backend_a.ip().to_string(),
                    port: backend_a.port(),
                }],
                load_balancing: None,
            },
        }];

        // Build the router exactly as `start_all` does.
        let default = balancer_for(&cfg);
        let routes = cfg
            .sni_routes
            .iter()
            .map(|r| {
                (
                    r.server_name.clone(),
                    Arc::new(
                        Balancer::new(r.upstream.targets.clone(), Strategy::RoundRobin).unwrap(),
                    ),
                )
            })
            .collect();
        let router = Arc::new(SniRouter::new(routes, default));

        let (_tx, rx) = watch::channel(false);
        let proxy = tcp::spawn(&cfg, router, Duration::from_secs(2), rx)
            .await
            .unwrap();

        // Matched SNI -> backend A (also proves the ClientHello was replayed,
        // since A only responds after reading its first bytes).
        assert_eq!(probe(proxy, &client_hello("a.example.com")).await, b'A');
        // Unmatched SNI -> default backend B.
        assert_eq!(probe(proxy, &client_hello("other.com")).await, b'B');
        // Non-TLS garbage -> default backend B (NotPresent, no wait).
        assert_eq!(probe(proxy, b"not a tls hello").await, b'B');
    }
}
