//! UDP stream proxy: relay datagrams between clients and a load-balanced
//! upstream, tracking one session per client address.
//!
//! UDP has no connections, so a single owning task demultiplexes the shared
//! listener socket by client address into a session map (no lock needed — the
//! map is touched only by that task). Each session owns an ephemeral socket
//! `connect`ed to the chosen upstream and a reader task that pumps upstream
//! replies back to the client. A session is torn down after `idle` with **no
//! traffic in either direction** (tracked by a shared `last_seen`), reaped by a
//! 1-second prune tick.

use std::collections::HashMap;
use std::io;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::net::UdpSocket;
use tokio::sync::watch;
use tokio::time::timeout;
use tracing::{debug, warn};

use crate::balancer::{Balancer, OwnedConnGuard};
use crate::config::StreamListenerConfig;

/// Max UDP payload plus headroom.
const BUF_SIZE: usize = 65_535;

/// One client's proxy session: the ephemeral upstream socket, shared activity
/// clock, liveness flag, and the in-flight guard held for the session's life.
struct Session {
    upstream: Arc<UdpSocket>,
    last_seen: Arc<AtomicU64>,
    alive: Arc<AtomicBool>,
    _guard: OwnedConnGuard,
}

fn now_ms(epoch: Instant) -> u64 {
    epoch.elapsed().as_millis() as u64
}

/// Binds the UDP listener (fail-fast) and spawns its receive loop, returning the
/// bound address (the OS-assigned port when `cfg.port == 0`).
pub async fn spawn(
    cfg: &StreamListenerConfig,
    balancer: Arc<Balancer>,
    idle: Duration,
    mut shutdown_rx: watch::Receiver<bool>,
) -> io::Result<SocketAddr> {
    let ip = cfg
        .bind
        .parse()
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, format!("invalid bind: {}", e)))?;
    let addr = SocketAddr::new(ip, cfg.port);
    let sock = Arc::new(UdpSocket::bind(addr).await?);
    let local = sock.local_addr()?;

    tokio::spawn(async move {
        let epoch = Instant::now();
        let mut sessions: HashMap<SocketAddr, Session> = HashMap::new();
        let mut buf = [0u8; BUF_SIZE];
        let mut prune = tokio::time::interval(Duration::from_secs(1));

        loop {
            tokio::select! {
                _ = shutdown_rx.changed() => break,
                _ = prune.tick() => {
                    sessions.retain(|_, s| s.alive.load(Ordering::Relaxed));
                }
                recv = sock.recv_from(&mut buf) => {
                    let (n, client) = match recv {
                        Ok(v) => v,
                        Err(e) => { warn!("udp stream recv error: {}", e); continue; }
                    };
                    let now = now_ms(epoch);

                    // Drop a dead session so it gets recreated below.
                    if sessions.get(&client).is_some_and(|s| !s.alive.load(Ordering::Relaxed)) {
                        sessions.remove(&client);
                    }
                    if let std::collections::hash_map::Entry::Vacant(e) = sessions.entry(client) {
                        match create_session(&balancer, &sock, client, idle, epoch).await {
                            Ok(session) => { e.insert(session); }
                            Err(e) => { warn!("udp stream session setup failed: {}", e); continue; }
                        }
                    }
                    if let Some(session) = sessions.get(&client) {
                        session.last_seen.store(now, Ordering::Relaxed);
                        if let Err(e) = session.upstream.send(&buf[..n]).await {
                            debug!("udp stream send to upstream failed: {}", e);
                        }
                    }
                }
            }
        }
    });

    Ok(local)
}

/// Creates a session for `client`: an ephemeral socket connected to a selected
/// upstream, plus a spawned reader task pumping replies back to the client.
async fn create_session(
    balancer: &Arc<Balancer>,
    listener: &Arc<UdpSocket>,
    client: SocketAddr,
    idle: Duration,
    epoch: Instant,
) -> io::Result<Session> {
    let idx = balancer.select(&client.to_string());
    let guard = balancer.owned_acquire(idx);
    let target = balancer.target(idx);

    let upstream = Arc::new(UdpSocket::bind(("0.0.0.0", 0)).await?);
    upstream
        .connect((target.host.as_str(), target.port))
        .await?;

    let last_seen = Arc::new(AtomicU64::new(now_ms(epoch)));
    let alive = Arc::new(AtomicBool::new(true));

    tokio::spawn(reader(
        listener.clone(),
        upstream.clone(),
        client,
        alive.clone(),
        last_seen.clone(),
        idle,
        epoch,
    ));

    Ok(Session {
        upstream,
        last_seen,
        alive,
        _guard: guard,
    })
}

/// Pumps upstream replies back to the client, exiting when the session has been
/// idle in **both** directions for `idle` (so an active client with a briefly
/// quiet upstream is not torn down).
async fn reader(
    listener: Arc<UdpSocket>,
    upstream: Arc<UdpSocket>,
    client: SocketAddr,
    alive: Arc<AtomicBool>,
    last_seen: Arc<AtomicU64>,
    idle: Duration,
    epoch: Instant,
) {
    let mut buf = [0u8; BUF_SIZE];
    let idle_ms = idle.as_millis() as u64;
    loop {
        match timeout(idle, upstream.recv(&mut buf)).await {
            Ok(Ok(n)) => {
                if let Err(e) = listener.send_to(&buf[..n], client).await {
                    debug!("udp stream send to client failed: {}", e);
                    break;
                }
                last_seen.store(now_ms(epoch), Ordering::Relaxed);
            }
            Ok(Err(e)) => {
                debug!("udp stream upstream recv error: {}", e);
                break;
            }
            Err(_) => {
                // recv idle elapsed — only give up if the client side is idle too.
                if now_ms(epoch).saturating_sub(last_seen.load(Ordering::Relaxed)) >= idle_ms {
                    break;
                }
            }
        }
    }
    alive.store(false, Ordering::Relaxed);
}
