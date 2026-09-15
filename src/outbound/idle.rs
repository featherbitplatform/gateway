//! Body wrappers for streaming upstream responses.
//!
//! Both operate at the `http_body::Body` frame level so they compose with any
//! `BoxBody<Bytes, BoxError>` — the streaming body type used throughout
//! `src/outbound` and `src/context/stream.rs` — without re-buffering it.
//! `BoxError` (`crate::outbound::BoxError`) is a boxed `std::error::Error`,
//! not `hyper::Error`: see that type alias's doc comment for why. In short,
//! `hyper::Error` has no public constructor anywhere in the `hyper` crate, so
//! a body wrapper built on it could never report its own failure. `BoxError`
//! has no such restriction, which is what lets [`idle_timeout_body`] below
//! actually error the stream on reap, rather than merely ending it.

use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use bytes::Bytes;
use http_body::{Body, Frame, SizeHint};
use http_body_util::combinators::BoxBody;
use http_body_util::BodyExt;

use super::BoxError;

/// The error [`idle_timeout_body`] reports when a stream is reaped: no frame
/// arrived for the configured idle bound.
#[derive(Debug)]
pub struct IdleTimeoutError {
    /// The idle bound that elapsed with no frame arriving.
    pub idle: Duration,
}

impl std::fmt::Display for IdleTimeoutError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "idle timeout: no response body frame for {:?}",
            self.idle
        )
    }
}

impl std::error::Error for IdleTimeoutError {}

/// Wraps `body` so it errors when no frame arrives for `idle`. The timer
/// resets on every frame, so a steady stream survives indefinitely while a
/// silent one is reaped — and reaped as a real failure
/// (`Poll::Ready(Some(Err(IdleTimeoutError)))`), not a clean end: ending
/// cleanly would write the chunked terminator (HTTP/1.1) or `END_STREAM`
/// (h2) exactly as a legitimately complete response would, making a
/// truncated body indistinguishable from a complete one on the wire — on
/// exactly the unbounded bodies (NDJSON exports, log tails, bulk proxied
/// data) this feature targets. A real upstream error — the wrapped body
/// itself yielding `Err`, e.g. a connection reset — is forwarded unchanged;
/// the reap only fires when nothing else has.
pub fn idle_timeout_body(
    body: BoxBody<Bytes, BoxError>,
    idle: Duration,
) -> BoxBody<Bytes, BoxError> {
    IdleTimeoutBody {
        inner: body,
        idle,
        sleep: Box::pin(tokio::time::sleep(idle)),
    }
    .boxed()
}

struct IdleTimeoutBody {
    inner: BoxBody<Bytes, BoxError>,
    idle: Duration,
    sleep: Pin<Box<tokio::time::Sleep>>,
}

impl Body for IdleTimeoutBody {
    type Data = Bytes;
    type Error = BoxError;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        let this = self.get_mut();

        match Pin::new(&mut this.inner).poll_frame(cx) {
            Poll::Ready(frame) => {
                // Any activity — a frame, the body's own clean end, or its
                // own error — pushes the deadline out. A body that has
                // already reported it's finished is never polled again, so
                // resetting here on the terminal poll is a harmless no-op.
                this.sleep
                    .as_mut()
                    .reset(tokio::time::Instant::now() + this.idle);
                return Poll::Ready(frame);
            }
            Poll::Pending => {}
        }

        match this.sleep.as_mut().poll(cx) {
            Poll::Ready(()) => {
                // The only place this fires: the caller (a hyper connection
                // writing this body to the client) sees only an ordinary
                // `Err` frame, not a log line, so this is the one place in
                // the process a silent-upstream reap is ever recorded.
                tracing::warn!(
                    idle_ms = this.idle.as_millis() as u64,
                    "streamed response body idle timeout: no frame for {:?}, reaping the stream",
                    this.idle
                );
                Poll::Ready(Some(Err(Box::new(IdleTimeoutError { idle: this.idle }))))
            }
            Poll::Pending => Poll::Pending,
        }
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
}

/// Wraps `body` so `guards` are dropped only when the body reports it has
/// finished (cleanly or with an error) or the returned body is itself
/// dropped — never merely because the node that started the stream returned.
/// This is what keeps balancer in-flight counters and `limit-conn` permits
/// held for the stream's real lifetime.
pub fn body_holding(
    body: BoxBody<Bytes, BoxError>,
    guards: Vec<Box<dyn Send + 'static>>,
) -> BoxBody<Bytes, BoxError> {
    BodyHolding {
        inner: body,
        guards: std::sync::Mutex::new(guards),
    }
    .boxed()
}

struct BodyHolding {
    inner: BoxBody<Bytes, BoxError>,
    // `Box<dyn Send>` guards are `Send`-only, not `Sync`, so a bare `Vec`
    // here would make `BodyHolding` (and, boxed, the `BoxBody` it returns)
    // lose `Sync` — `BoxBody`'s trait object requires `Send + Sync`.
    // `Mutex<T>` is `Sync` whenever `T: Send`, and `poll_frame` always has
    // exclusive access (`&mut self` via `get_mut`), so the lock never
    // contends. Same trick as `context::stream::ResponseStream`.
    //
    // Cleared as soon as the body reports it is finished (see `poll_frame`);
    // otherwise dropped along with `self` when `self` is dropped — ordinary
    // field-drop order gives us that half for free, no `Drop` impl needed.
    guards: std::sync::Mutex<Vec<Box<dyn Send + 'static>>>,
}

impl Body for BodyHolding {
    type Data = Bytes;
    type Error = BoxError;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        let this = self.get_mut();
        match Pin::new(&mut this.inner).poll_frame(cx) {
            Poll::Ready(None) => {
                this.guards.get_mut().unwrap().clear();
                Poll::Ready(None)
            }
            Poll::Ready(Some(Err(e))) => {
                this.guards.get_mut().unwrap().clear();
                Poll::Ready(Some(Err(e)))
            }
            other => other,
        }
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    /// A minimal hand-rolled body over a queue of scheduled data frames, each
    /// available after its own delay. Never errors on its own — only the
    /// wrappers under test (`idle_timeout_body`) synthesize errors; this body
    /// exists to produce plain successful frames on a schedule. After the
    /// queue drains it either ends the stream (`stall = false`) or stays
    /// `Pending` forever (`stall = true`), simulating an upstream that goes
    /// silent without closing the connection.
    struct ScriptedBody {
        items: VecDeque<Duration>,
        stall: bool,
        pending: Option<Pin<Box<tokio::time::Sleep>>>,
    }

    impl ScriptedBody {
        fn new(delays: Vec<Duration>, stall: bool) -> Self {
            Self {
                items: delays.into(),
                stall,
                pending: None,
            }
        }
    }

    impl Body for ScriptedBody {
        type Data = Bytes;
        type Error = BoxError;

        fn poll_frame(
            self: Pin<&mut Self>,
            cx: &mut Context<'_>,
        ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
            let this = self.get_mut();
            // A single pass suffices: either a pending timer is already
            // armed, or one is armed here and polled immediately after —
            // never a second iteration, so no `loop` is needed.
            if this.pending.is_none() {
                match this.items.pop_front() {
                    Some(delay) => this.pending = Some(Box::pin(tokio::time::sleep(delay))),
                    None => {
                        return if this.stall {
                            Poll::Pending
                        } else {
                            Poll::Ready(None)
                        };
                    }
                }
            }
            let sleep = this.pending.as_mut().unwrap();
            match sleep.as_mut().poll(cx) {
                Poll::Ready(()) => {
                    this.pending = None;
                    Poll::Ready(Some(Ok(Frame::data(Bytes::from_static(b"x")))))
                }
                Poll::Pending => Poll::Pending,
            }
        }
    }

    /// A body that yields one frame and then goes silent must be reaped —
    /// not left to hang forever — once the idle bound elapses with no
    /// further frame, and the reap must surface as a real error, not a
    /// clean end (see the module doc: a clean end is indistinguishable from
    /// a legitimately complete response on the wire).
    #[tokio::test]
    async fn test_idle_timeout_reaps_a_silent_stream_as_an_error() {
        let body = ScriptedBody::new(vec![Duration::ZERO], true);
        let mut wrapped = idle_timeout_body(body.boxed(), Duration::from_millis(150));

        // First frame arrives normally.
        let first = tokio::time::timeout(
            Duration::from_millis(500),
            std::future::poll_fn(|cx| Pin::new(&mut wrapped).poll_frame(cx)),
        )
        .await
        .expect("first frame must arrive promptly");
        assert!(matches!(first, Some(Ok(_))), "expected the first frame");

        // Then silence: the idle bound must reap the stream as an error, not
        // hang and not end cleanly.
        let second = tokio::time::timeout(
            Duration::from_millis(800),
            std::future::poll_fn(|cx| Pin::new(&mut wrapped).poll_frame(cx)),
        )
        .await
        .expect("a silent stream must be reaped, not hang past the idle bound");
        match second {
            Some(Err(e)) => {
                assert!(
                    e.downcast_ref::<IdleTimeoutError>().is_some(),
                    "expected an IdleTimeoutError, got: {e}"
                );
            }
            other => panic!("expected the reap to error the stream, got: {other:?}"),
        }
    }

    /// A body that keeps sending frames inside the idle bound must be left
    /// alone and allowed to finish on its own, however long that takes in
    /// total. 15 frames * 50ms = 750ms of total elapsed time against a
    /// 500ms idle bound: a naive total-deadline implementation (arms the
    /// timer once, never resets it) fails this around frame 10, so this is
    /// the case that actually exercises the reset — 5 frames * 50ms = 250ms
    /// stays under a 500ms bound even without ever resetting anything.
    #[tokio::test]
    async fn test_idle_timeout_lets_a_steady_stream_finish() {
        let delays = vec![Duration::from_millis(50); 15];
        let body = ScriptedBody::new(delays, false);
        let wrapped = idle_timeout_body(body.boxed(), Duration::from_millis(500));

        let collected = tokio::time::timeout(Duration::from_secs(3), wrapped.collect())
            .await
            .expect("a steady stream (every gap well under the idle bound) must not be reaped")
            .expect("no error expected");

        assert_eq!(collected.to_bytes().as_ref(), b"xxxxxxxxxxxxxxx");
    }

    struct DropGuard(std::sync::Arc<std::sync::atomic::AtomicUsize>);
    impl Drop for DropGuard {
        fn drop(&mut self) {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
    }

    /// The guard must not be dropped while the body still has frames to
    /// deliver, and must be dropped once the body reports it is finished.
    #[tokio::test]
    async fn test_body_holding_holds_guards_until_body_finishes() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        let dropped = Arc::new(AtomicUsize::new(0));
        let body = ScriptedBody::new(vec![Duration::ZERO, Duration::ZERO], false);
        let mut wrapped = body_holding(body.boxed(), vec![Box::new(DropGuard(dropped.clone()))]);

        // First frame: the body isn't finished yet, so the guard must survive.
        let _ = std::future::poll_fn(|cx| Pin::new(&mut wrapped).poll_frame(cx)).await;
        assert_eq!(
            dropped.load(Ordering::SeqCst),
            0,
            "guard released too early"
        );

        // Drain the rest of the body to completion.
        let _ = wrapped.collect().await.unwrap();
        assert_eq!(
            dropped.load(Ordering::SeqCst),
            1,
            "guard not released once the body finished"
        );
    }

    /// Dropping the wrapped body early (before it finishes) must also
    /// release the guard — the fallback half of the contract that doesn't
    /// depend on the body running to completion.
    #[tokio::test]
    async fn test_body_holding_drops_guards_when_body_dropped_early() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        let dropped = Arc::new(AtomicUsize::new(0));
        let body = ScriptedBody::new(vec![Duration::ZERO], true);
        let wrapped = body_holding(body.boxed(), vec![Box::new(DropGuard(dropped.clone()))]);

        assert_eq!(dropped.load(Ordering::SeqCst), 0);
        drop(wrapped);
        assert_eq!(
            dropped.load(Ordering::SeqCst),
            1,
            "guard not released when the body was dropped early"
        );
    }

    /// The guard must also release when the body ends with an error (e.g.
    /// the idle reap above), not only on a clean end — `body_holding` treats
    /// both as "finished".
    #[tokio::test]
    async fn test_body_holding_drops_guards_on_body_error() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        let dropped = Arc::new(AtomicUsize::new(0));
        let body = ScriptedBody::new(vec![Duration::ZERO], true);
        let idled = idle_timeout_body(body.boxed(), Duration::from_millis(50));
        let mut wrapped = body_holding(idled, vec![Box::new(DropGuard(dropped.clone()))]);

        // First frame.
        let _ = std::future::poll_fn(|cx| Pin::new(&mut wrapped).poll_frame(cx)).await;
        assert_eq!(dropped.load(Ordering::SeqCst), 0);

        // The idle timeout fires and errors the (inner) body; body_holding
        // must treat that as "finished" and release the guard.
        let second = tokio::time::timeout(
            Duration::from_millis(500),
            std::future::poll_fn(|cx| Pin::new(&mut wrapped).poll_frame(cx)),
        )
        .await
        .expect("idle reap must fire");
        assert!(matches!(second, Some(Err(_))));
        assert_eq!(
            dropped.load(Ordering::SeqCst),
            1,
            "guard not released when the body ended with an error"
        );
    }
}
