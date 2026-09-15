//! The out-of-band streaming response body.
//!
//! A streaming body cannot live on `Context` as an ordinary field: `Context`
//! is `Serialize`/`Deserialize` because it marshals into Lua scripts and into
//! debug trace snapshots. The handle is therefore `#[serde(skip)]`, the same
//! way the WebSocket upgrade handle is carried out-of-band by the listener.

use bytes::Bytes;
use http_body_util::combinators::BoxBody;

/// An upstream response body being relayed to the client unbuffered, plus any
/// guards whose lifetime must match the stream rather than the node that
/// produced it (balancer in-flight counters, `limit-conn` permits).
pub struct ResponseStream {
    // Read back out by `into_parts` in Task 6, once the listener relays a
    // stream instead of buffering it; unread on any path exercised today.
    #[allow(dead_code)]
    body: BoxBody<Bytes, hyper::Error>,
    guards: Vec<Box<dyn Send + 'static>>,
}

// SAFETY: `ResponseStream` never hands out a `&`-reference to `body` or
// `guards` — the only ways to reach them are by value (`into_parts`, which
// consumes `self`) or through `&mut self` (`hold`). Two threads can therefore
// never observe the same inner data through a shared `&ResponseStream`
// simultaneously, so implementing `Sync` introduces no possibility of a data
// race even though the erased trait objects it holds (`BoxBody`,
// `Box<dyn Send>`) are `Send`-only, not `Sync` (the same justification used
// by the `sync_wrapper` crate). This is needed so `Context` — which nests
// this behind `Option<ResponseStream>` — stays `Sync`: several existing
// plugins (session-backed auth: `authz-casdoor`, `cas-auth`, `dingtalk-auth`,
// `feishu-auth`, `openid-connect`) hold `&Context` across an `.await`, which
// requires `Context: Sync` for their `execute` future to remain `Send`.
unsafe impl Sync for ResponseStream {}

// Task 1 only lands the data model; nothing constructs or consumes a
// `ResponseStream` yet (a node starts populating `response.stream` in Task
// 2, and the listener starts calling `into_parts` in Task 6), so these are
// legitimately unused for now rather than dead in the usual sense.
#[allow(dead_code)]
impl ResponseStream {
    pub fn new(body: BoxBody<Bytes, hyper::Error>) -> Self {
        Self {
            body,
            guards: Vec::new(),
        }
    }

    /// Attaches a guard released when the stream is consumed or dropped.
    pub fn hold(&mut self, guard: Box<dyn Send + 'static>) {
        self.guards.push(guard);
    }

    /// Takes the body for transmission. Guards travel with the returned body's
    /// owner, so the caller must keep `self` alive until the body is finished.
    pub fn into_parts(self) -> (BoxBody<Bytes, hyper::Error>, Vec<Box<dyn Send + 'static>>) {
        (self.body, self.guards)
    }
}

// `Box<dyn Send>` has no Debug; the struct is only ever shown as a marker.
impl std::fmt::Debug for ResponseStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResponseStream")
            .field("guards", &self.guards.len())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use http_body_util::{BodyExt, Full};

    fn boxed(text: &str) -> BoxBody<Bytes, hyper::Error> {
        Full::new(Bytes::from(text.to_owned()))
            .map_err(|never| match never {})
            .boxed()
    }

    /// The stream handle must carry arbitrary guards (balancer in-flight,
    /// limit-conn) so they release when the stream ends, not when the node
    /// that created it returns.
    #[tokio::test]
    async fn test_stream_holds_guards_until_dropped() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        struct Guard(Arc<AtomicUsize>);
        impl Drop for Guard {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }

        let dropped = Arc::new(AtomicUsize::new(0));
        let mut stream = ResponseStream::new(boxed("data: hello\n\n"));
        stream.hold(Box::new(Guard(dropped.clone())));

        assert_eq!(
            dropped.load(Ordering::SeqCst),
            0,
            "guard released too early"
        );
        drop(stream);
        assert_eq!(
            dropped.load(Ordering::SeqCst),
            1,
            "guard not released on drop"
        );
    }
}
