//! Body wrappers for streaming upstream responses.
//!
//! Both operate at the `http_body::Body` frame level so they compose with any
//! `BoxBody<Bytes, hyper::Error>` — the streaming body type used throughout
//! `src/outbound` and `src/context/stream.rs` — without re-buffering it.
//!
//! A note on the error type: `hyper::Error` cannot be constructed outside the
//! `hyper` crate. Every constructor on it (`new`, `new_io`, `new_canceled`,
//! ...) is `pub(super)` — verified against hyper 1.9.0's `src/error.rs` — so
//! nothing outside `hyper` itself can mint a fresh `hyper::Error` value; code
//! here can only forward one hyper already produced from a real connection.
//! That forecloses a literal "the idle body errors" implementation:
//! [`idle_timeout_body`] cannot manufacture an `Err(hyper::Error)` frame of
//! its own, so a silently-stalled body is reaped by ending the stream
//! (`Poll::Ready(None)`) instead. See that function's doc comment for the
//! consequence and why this is the honest alternative rather than an
//! `unsafe` workaround.

use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use bytes::Bytes;
use http_body::{Body, Frame, SizeHint};
use http_body_util::combinators::BoxBody;
use http_body_util::BodyExt;

/// Wraps `body` so it is reaped after `idle` elapses with no frame arriving.
/// The timer resets on every frame, so a steady stream survives indefinitely
/// while a silent one is bounded.
///
/// Reaping ends the stream (`Poll::Ready(None)`) rather than erroring it —
/// `hyper::Error` has no public constructor anywhere in the `hyper` crate, so
/// this wrapper cannot produce a fresh one of its own (see the module doc).
/// For a chunked HTTP/1.1 response this is written as a clean terminator
/// rather than an abruptly reset connection, so on the wire a client cannot
/// distinguish "reaped for going idle" from "upstream finished on its own".
/// The idle-reap should be logged at the point it fires (left to the caller)
/// so the operator still has visibility. A real upstream error — the wrapped
/// body itself yielding `Err(hyper::Error)`, e.g. a connection reset — is
/// forwarded unchanged; only the manufactured "no frame for `idle`" case is
/// affected.
pub fn idle_timeout_body(
    body: BoxBody<Bytes, hyper::Error>,
    idle: Duration,
) -> BoxBody<Bytes, hyper::Error> {
    IdleTimeoutBody {
        inner: body,
        idle,
        sleep: Box::pin(tokio::time::sleep(idle)),
    }
    .boxed()
}

struct IdleTimeoutBody {
    inner: BoxBody<Bytes, hyper::Error>,
    idle: Duration,
    sleep: Pin<Box<tokio::time::Sleep>>,
}

impl Body for IdleTimeoutBody {
    type Data = Bytes;
    type Error = hyper::Error;

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
            Poll::Ready(()) => Poll::Ready(None),
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
// Not yet called outside tests: the `upstream` node only ever attaches its
// guard via `ResponseStream::hold` today (see `plugins/native/upstream.rs`);
// the listener starts calling this to layer on `limit-conn`-style permits in
// the follow-up task that relays a stream instead of buffering it.
#[allow(dead_code)]
pub fn body_holding(
    body: BoxBody<Bytes, hyper::Error>,
    guards: Vec<Box<dyn Send + 'static>>,
) -> BoxBody<Bytes, hyper::Error> {
    BodyHolding {
        inner: body,
        guards: std::sync::Mutex::new(guards),
    }
    .boxed()
}

#[allow(dead_code)] // constructed only by `body_holding`, above
struct BodyHolding {
    inner: BoxBody<Bytes, hyper::Error>,
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
    type Error = hyper::Error;

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
    /// available after its own delay. Never errors — `hyper::Error` can't be
    /// constructed outside `hyper` (see the module doc), so a test body that
    /// needs `Error = hyper::Error` can only ever succeed or end cleanly.
    /// After the queue drains it either ends the stream (`stall = false`) or
    /// stays `Pending` forever (`stall = true`), simulating an upstream that
    /// goes silent without closing the connection.
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
        type Error = hyper::Error;

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
    /// further frame.
    #[tokio::test]
    async fn test_idle_timeout_reaps_a_silent_stream() {
        let body = ScriptedBody::new(vec![Duration::ZERO], true);
        let wrapped = idle_timeout_body(body.boxed(), Duration::from_millis(150));

        let collected = tokio::time::timeout(Duration::from_millis(800), wrapped.collect())
            .await
            .expect("a silent stream must be reaped, not hang past the idle bound")
            .expect("reaping ends the stream cleanly rather than erroring it");

        assert_eq!(
            collected.to_bytes().as_ref(),
            b"x",
            "only the one frame sent before the stream went silent"
        );
    }

    /// A body that keeps sending frames inside the idle bound must be left
    /// alone and allowed to finish on its own, however long that takes in
    /// total.
    #[tokio::test]
    async fn test_idle_timeout_lets_a_steady_stream_finish() {
        let delays = vec![Duration::from_millis(50); 5];
        let body = ScriptedBody::new(delays, false);
        let wrapped = idle_timeout_body(body.boxed(), Duration::from_millis(500));

        let collected = tokio::time::timeout(Duration::from_secs(2), wrapped.collect())
            .await
            .expect("a steady stream (gaps well under the idle bound) must not be reaped")
            .expect("no error expected");

        assert_eq!(collected.to_bytes().as_ref(), b"xxxxx");
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
}
