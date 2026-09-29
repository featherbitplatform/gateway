//! Socket tuning shared by every listener and dialer that carries proxied traffic.

use tokio::net::TcpStream;

/// Applies the socket options every proxied TCP connection needs.
///
/// Today that is `TCP_NODELAY`. With Nagle's algorithm on, a response that
/// leaves as several small writes -- TLS records, HTTP/2 frames -- has its
/// second write held until the peer ACKs the first, and peers delay ACKs by up
/// to ~40 ms: a p99 latency tail on every TLS and HTTP/2 connection. Proxies
/// turn Nagle off for the same reason (nginx: `tcp_nodelay on`).
///
/// Failure is logged and otherwise ignored: the connection still works, only
/// slower.
pub fn tune_tcp(stream: &TcpStream) {
    if let Err(e) = stream.set_nodelay(true) {
        tracing::debug!("could not set TCP_NODELAY: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpListener;

    #[tokio::test]
    async fn test_tune_tcp_disables_nagle_on_an_accepted_connection() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let client = tokio::spawn(async move { TcpStream::connect(addr).await.unwrap() });
        let (accepted, _) = listener.accept().await.unwrap();
        let _client = client.await.unwrap();

        assert!(
            !accepted.nodelay().unwrap(),
            "precondition: Nagle is on by default"
        );
        tune_tcp(&accepted);
        assert!(accepted.nodelay().unwrap(), "TCP_NODELAY must be set");
    }
}
