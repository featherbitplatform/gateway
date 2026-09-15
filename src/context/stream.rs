//! The out-of-band streaming response body.
//!
//! A streaming body cannot live on `Context` as an ordinary field: `Context`
//! is `Serialize`/`Deserialize` because it marshals into Lua scripts and into
//! debug trace snapshots. The handle is therefore `#[serde(skip)]`, the same
//! way the WebSocket upgrade handle is carried out-of-band by the listener.

use bytes::Bytes;
use http_body_util::combinators::BoxBody;

use crate::outbound::BoxError;

/// An upstream response body being relayed to the client unbuffered, plus any
/// guards whose lifetime must match the stream rather than the node that
/// produced it (balancer in-flight counters, `limit-conn` permits).
pub struct ResponseStream {
    // Read back out by `into_parts` in Task 6, once the listener relays a
    // stream instead of buffering it; unread on any path exercised today.
    // `BoxBody` is already `Send + Sync` (see `http_body_util`'s
    // `combinators::BoxBody`, as opposed to the `Send`-only
    // `UnsyncBoxBody`), so it needs no help to keep `ResponseStream: Sync`.
    #[allow(dead_code)]
    body: BoxBody<Bytes, BoxError>,
    // `Box<dyn Send + 'static>` guards are `Send`-only, not `Sync`, so a bare
    // `Vec` here would make `ResponseStream` — and, nested inside
    // `GatewayResponse`, `Context` — lose `Sync`. Several existing plugins
    // hold `&Context` across an `.await`, which requires `Context: Sync` for
    // their `execute` future to stay `Send`. `Mutex<T>` is `Sync` whenever
    // `T: Send`, restoring `Sync` without `unsafe`. Neither `hold` nor
    // `into_parts` below actually contends the lock: both already have
    // exclusive access (`&mut self` / `self`), so this costs nothing at
    // runtime.
    guards: std::sync::Mutex<Vec<Box<dyn Send + 'static>>>,
}

// Task 1 only lands the data model; nothing constructs or consumes a
// `ResponseStream` yet (a node starts populating `response.stream` in Task
// 2, and the listener starts calling `into_parts` in Task 6), so these are
// legitimately unused for now rather than dead in the usual sense.
#[allow(dead_code)]
impl ResponseStream {
    pub fn new(body: BoxBody<Bytes, BoxError>) -> Self {
        Self {
            body,
            guards: std::sync::Mutex::new(Vec::new()),
        }
    }

    /// Attaches a guard released when the stream is consumed or dropped.
    pub fn hold(&mut self, guard: Box<dyn Send + 'static>) {
        // `&mut self` already guarantees exclusive access; this never blocks.
        self.guards.get_mut().unwrap().push(guard);
    }

    /// Takes the body for transmission. The guards travel with the returned
    /// `Vec`, not with `self` (which is consumed here) — the caller must keep
    /// that `Vec` alive until the returned body has finished streaming to the
    /// client; dropping it early releases the guards early.
    pub fn into_parts(self) -> (BoxBody<Bytes, BoxError>, Vec<Box<dyn Send + 'static>>) {
        // `self` is owned here, so nothing else can hold the lock; never blocks.
        (self.body, self.guards.into_inner().unwrap())
    }
}

// `Box<dyn Send>` has no Debug; the struct is only ever shown as a marker.
impl std::fmt::Debug for ResponseStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResponseStream")
            .field("guards", &self.guards.lock().map(|g| g.len()).unwrap_or(0))
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use http_body_util::{BodyExt, Full};

    fn boxed(text: &str) -> BoxBody<Bytes, BoxError> {
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

    /// `into_parts` is the accessor Task 6 depends on: it must hand the
    /// guards over alongside the body rather than dropping them, and the
    /// guards' lifetime must be tied to the returned `Vec`, not to the body
    /// or to `self` (which no longer exists once `into_parts` returns).
    #[tokio::test]
    async fn test_into_parts_hands_guards_to_the_caller_with_the_body() {
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

        let (body, guards) = stream.into_parts();

        drop(body);
        assert_eq!(
            dropped.load(Ordering::SeqCst),
            0,
            "guard released when the body was dropped, before the guard vec was"
        );

        drop(guards);
        assert_eq!(
            dropped.load(Ordering::SeqCst),
            1,
            "guard not released when the returned guard vec was dropped"
        );
    }

    /// `Context` gained a nested `Send`-only member (`ResponseStream`'s
    /// `BoxBody` and guards) in this change; pin down that it stays both
    /// `Send` (required for `Plugin::execute`'s boxed future) and `Sync`
    /// (required because several plugins hold `&Context` across an
    /// `.await`), so a future change can't silently regress either.
    #[test]
    fn test_context_stays_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<crate::context::Context>();
    }
}
