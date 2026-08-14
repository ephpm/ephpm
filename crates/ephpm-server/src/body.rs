//! Unified response body type for the HTTP server.
//!
//! Provides [`ServerBody`], a type alias that supports both buffered responses
//! (small files, PHP output, error pages) and streamed responses (large files
//! served directly from disk without loading into memory).

use http_body_util::{BodyExt, Full, StreamBody};
use hyper::body::{Bytes, Frame};
use tokio_stream::StreamExt;
use tokio_util::io::ReaderStream;

/// The response body type used throughout the server.
///
/// This is a boxed body that unifies buffered (`Full<Bytes>`) and streamed
/// (`ReaderStream<File>`) responses behind a single type. The boxing cost
/// is negligible compared to network I/O.
pub type ServerBody = http_body_util::combinators::BoxBody<Bytes, std::io::Error>;

/// Wrap a fully buffered body for use as a [`ServerBody`].
///
/// This is the common path for small responses: error pages, PHP output,
/// cached files, and small static files.
pub fn buffered(body: Full<Bytes>) -> ServerBody {
    body.map_err(|never| match never {}).boxed()
}

/// Stream a file from disk as a [`ServerBody`].
///
/// Reads the file in 64 KiB chunks via [`ReaderStream`], avoiding loading
/// the entire file into memory. Used for files above the streaming threshold.
pub fn streamed(file: tokio::fs::File) -> ServerBody {
    let stream = ReaderStream::with_capacity(file, 64 * 1024);
    let framed = stream.map(|result| result.map(Frame::data));
    StreamBody::new(framed).boxed()
}

/// Stream response-body chunks from a channel as a [`ServerBody`] (worker-mode
/// streaming responses, Phase 3).
///
/// Each [`Bytes`] the worker produces via `send_response_stream` flows straight
/// to the client as a data frame, so bytes reach the client before PHP has
/// produced the whole body. The sender closing ends the body.
///
/// `aborted` is the streamed response's
/// [`StreamAbortFlag`](ephpm_php::worker_bridge::StreamAbortFlag). It is read
/// **after** the channel is exhausted, and decides what that exhaustion means:
///
/// - clear → the worker finished the body; end it normally (hyper writes the
///   terminating chunk / satisfies `Content-Length`);
/// - set → the worker died mid-body. Yield an `io::Error` as the final frame
///   so hyper abandons the response instead of completing it. The client sees a
///   failed transfer — the only honest outcome once a `200` status line has
///   already gone out and cannot be retracted.
///
/// The abort error is deliberately *not* produced in the same poll that sees
/// the channel close: the stream yields one `Poll::Pending` first, so hyper
/// flushes the response head it has buffered before the error tears the
/// connection down (issue #271). See `AbortAwareChunks::poll_next`.
#[must_use]
pub fn channel_body(
    rx: tokio::sync::mpsc::Receiver<Bytes>,
    aborted: ephpm_php::worker_bridge::StreamAbortFlag,
) -> ServerBody {
    StreamBody::new(AbortAwareChunks { rx, aborted, state: ChunkState::Streaming }).boxed()
}

/// Where an [`AbortAwareChunks`] stream is in its close sequence.
enum ChunkState {
    /// Forwarding chunks as the producer sends them.
    Streaming,
    /// The channel closed with the abort flag set. Yield `Poll::Pending`
    /// exactly once (self-waking) before the error frame, so hyper gets a
    /// chance to flush the response head it has buffered but not yet written
    /// to the socket. See [`AbortAwareChunks::poll_next`] — issue #271.
    FlushBeforeAbort,
    /// The terminal item has been produced; report end-of-stream from here on
    /// rather than erroring (or looping) on every subsequent poll.
    Finished,
}

/// Chunk stream behind [`channel_body`]: forwards every [`Bytes`] the producer
/// sends, then decides at end-of-channel whether that was a completed body or
/// an abandoned one.
struct AbortAwareChunks {
    rx: tokio::sync::mpsc::Receiver<Bytes>,
    aborted: ephpm_php::worker_bridge::StreamAbortFlag,
    /// Position in the close sequence (see [`ChunkState`]).
    state: ChunkState,
}

impl tokio_stream::Stream for AbortAwareChunks {
    type Item = Result<Frame<Bytes>, std::io::Error>;

    fn poll_next(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        use std::task::Poll;

        let this = self.get_mut();
        match this.state {
            ChunkState::Finished => Poll::Ready(None),
            // Second poll of the abort path. hyper has had its flush
            // opportunity, so break the framing now.
            ChunkState::FlushBeforeAbort => {
                this.state = ChunkState::Finished;
                Poll::Ready(Some(Err(std::io::Error::other(
                    "PHP worker died mid-response; body is incomplete",
                ))))
            }
            ChunkState::Streaming => match this.rx.poll_recv(cx) {
                Poll::Pending => Poll::Pending,
                Poll::Ready(Some(chunk)) => Poll::Ready(Some(Ok(Frame::data(chunk)))),
                Poll::Ready(None) => {
                    // The channel is closed. Read the flag only now: the
                    // producer sets it before dropping the sender, so if it was
                    // going to be set, it is set by the time we get here.
                    if this.aborted.load(std::sync::atomic::Ordering::SeqCst) {
                        // Do NOT return the error in this poll. hyper's h1
                        // dispatcher runs `poll_read` / `poll_write` /
                        // `poll_flush` in that order (`proto::h1::dispatch`,
                        // `Dispatcher::poll_loop`), and inside `poll_write` the
                        // body is polled as
                        // `let item = ready!(body.poll_frame(cx));` followed by
                        // `item.map_err(...)?`. An `Err` therefore propagates
                        // straight out of `poll_write` AND out of `poll_loop` —
                        // `poll_flush` never runs for that iteration. If the
                        // worker bailed before hyper's first body poll, the
                        // response head (and any chunk already buffered) is
                        // still sitting in the connection's write buffer, so it
                        // is dropped with the connection: the client sees a
                        // socket torn down with *no response head at all*,
                        // indistinguishable from a server crash. That is the
                        // #271 flake — `reqwest::get()` itself fails instead of
                        // returning a 200 with a broken body.
                        //
                        // `Poll::Pending` is the one answer that unwinds
                        // `poll_write` *without* an error, so `poll_loop`
                        // proceeds to `poll_flush` and puts the head on the
                        // wire. We wake immediately, so the very next poll
                        // delivers the error against an already-flushed head.
                        // The observable contract is then deterministic:
                        // 200 + headers, then a body that never terminates.
                        this.state = ChunkState::FlushBeforeAbort;
                        cx.waker().wake_by_ref();
                        Poll::Pending
                    } else {
                        this.state = ChunkState::Finished;
                        Poll::Ready(None)
                    }
                }
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use std::pin::Pin;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::task::{Context, Poll, Wake, Waker};

    use tokio_stream::Stream;

    use super::*;

    /// A waker that only counts how many times it was asked to re-poll.
    struct CountingWaker(AtomicUsize);

    impl Wake for CountingWaker {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }

        fn wake_by_ref(self: &Arc<Self>) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    fn chunks(
        rx: tokio::sync::mpsc::Receiver<Bytes>,
        aborted: ephpm_php::worker_bridge::StreamAbortFlag,
    ) -> AbortAwareChunks {
        AbortAwareChunks { rx, aborted, state: ChunkState::Streaming }
    }

    /// Regression test for #271: the abort error must not be produced in the
    /// same poll that observed the channel close. hyper skips `poll_flush` when
    /// the body errors, so an immediate error strands the response head in the
    /// connection's write buffer and the client never sees a status line at
    /// all. One `Poll::Pending` flush boundary (which self-wakes) gives hyper
    /// the flush it needs first.
    #[test]
    fn aborted_stream_yields_a_flush_boundary_before_the_error() {
        let (tx, rx) = tokio::sync::mpsc::channel::<Bytes>(4);
        let aborted: ephpm_php::worker_bridge::StreamAbortFlag = Arc::new(AtomicBool::new(false));

        // One chunk lands, then the worker bails: flag set, sender dropped —
        // the exact ordering `clear_in_flight_streams` produces.
        tx.try_send(Bytes::from_static(b"chunk1")).unwrap();
        aborted.store(true, Ordering::SeqCst);
        drop(tx);

        let mut stream = chunks(rx, aborted);
        let mut stream = Pin::new(&mut stream);

        let counter = Arc::new(CountingWaker(AtomicUsize::new(0)));
        let waker: Waker = counter.clone().into();
        let mut cx = Context::from_waker(&waker);

        // 1) the buffered chunk still comes out.
        let Poll::Ready(Some(Ok(frame))) = stream.as_mut().poll_next(&mut cx) else {
            panic!("expected the buffered data chunk");
        };
        assert_eq!(frame.into_data().ok(), Some(Bytes::from_static(b"chunk1")));

        // 2) end-of-channel + abort => a flush boundary, not the error yet.
        assert!(
            stream.as_mut().poll_next(&mut cx).is_pending(),
            "abort must yield a Pending flush boundary before the error"
        );
        assert_eq!(
            counter.0.load(Ordering::SeqCst),
            1,
            "the flush boundary must self-wake so the runtime re-polls"
        );

        // 3) now the error frame breaks the body framing.
        let Poll::Ready(Some(Err(err))) = stream.as_mut().poll_next(&mut cx) else {
            panic!("expected the abort error after the flush boundary");
        };
        assert_eq!(err.kind(), std::io::ErrorKind::Other);

        // 4) terminal from here on.
        assert!(matches!(stream.as_mut().poll_next(&mut cx), Poll::Ready(None)));
    }

    /// A stream the worker finished normally ends straight away: no flush
    /// boundary, no spurious wake, no error.
    #[test]
    fn clean_stream_ends_without_error_or_extra_poll() {
        let (tx, rx) = tokio::sync::mpsc::channel::<Bytes>(4);
        let aborted: ephpm_php::worker_bridge::StreamAbortFlag = Arc::new(AtomicBool::new(false));

        tx.try_send(Bytes::from_static(b"only")).unwrap();
        drop(tx); // clean EOF, abort flag left clear

        let mut stream = chunks(rx, aborted);
        let mut stream = Pin::new(&mut stream);

        let counter = Arc::new(CountingWaker(AtomicUsize::new(0)));
        let waker: Waker = counter.clone().into();
        let mut cx = Context::from_waker(&waker);

        assert!(matches!(stream.as_mut().poll_next(&mut cx), Poll::Ready(Some(Ok(_)))));
        assert!(matches!(stream.as_mut().poll_next(&mut cx), Poll::Ready(None)));
        assert_eq!(
            counter.0.load(Ordering::SeqCst),
            0,
            "a clean finish must not wake for an extra poll"
        );
    }

    /// An abort with nothing buffered still flushes first — this is the case
    /// where the worker died before producing a single chunk, and it is the one
    /// most likely to lose the head.
    #[test]
    fn abort_with_no_chunks_still_flushes_first() {
        let (tx, rx) = tokio::sync::mpsc::channel::<Bytes>(4);
        let aborted: ephpm_php::worker_bridge::StreamAbortFlag = Arc::new(AtomicBool::new(false));

        aborted.store(true, Ordering::SeqCst);
        drop(tx);

        let mut stream = chunks(rx, aborted);
        let mut stream = Pin::new(&mut stream);

        let counter = Arc::new(CountingWaker(AtomicUsize::new(0)));
        let waker: Waker = counter.clone().into();
        let mut cx = Context::from_waker(&waker);

        assert!(stream.as_mut().poll_next(&mut cx).is_pending());
        assert!(matches!(stream.as_mut().poll_next(&mut cx), Poll::Ready(Some(Err(_)))));
        assert!(matches!(stream.as_mut().poll_next(&mut cx), Poll::Ready(None)));
    }
}
