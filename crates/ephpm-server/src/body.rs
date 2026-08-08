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
#[must_use]
pub fn channel_body(
    rx: tokio::sync::mpsc::Receiver<Bytes>,
    aborted: ephpm_php::worker_bridge::StreamAbortFlag,
) -> ServerBody {
    StreamBody::new(AbortAwareChunks { rx, aborted, finished: false }).boxed()
}

/// Chunk stream behind [`channel_body`]: forwards every [`Bytes`] the producer
/// sends, then decides at end-of-channel whether that was a completed body or
/// an abandoned one.
struct AbortAwareChunks {
    rx: tokio::sync::mpsc::Receiver<Bytes>,
    aborted: ephpm_php::worker_bridge::StreamAbortFlag,
    /// Set once the terminal item has been produced, so the stream reports
    /// end-of-stream afterwards instead of erroring on every poll.
    finished: bool,
}

impl tokio_stream::Stream for AbortAwareChunks {
    type Item = Result<Frame<Bytes>, std::io::Error>;

    fn poll_next(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        use std::task::Poll;

        let this = self.get_mut();
        if this.finished {
            return Poll::Ready(None);
        }
        match this.rx.poll_recv(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Some(chunk)) => Poll::Ready(Some(Ok(Frame::data(chunk)))),
            Poll::Ready(None) => {
                // The channel is closed. Read the flag only now: the producer
                // sets it before dropping the sender, so if it was going to be
                // set, it is set by the time we get here.
                this.finished = true;
                if this.aborted.load(std::sync::atomic::Ordering::SeqCst) {
                    Poll::Ready(Some(Err(std::io::Error::other(
                        "PHP worker died mid-response; body is incomplete",
                    ))))
                } else {
                    Poll::Ready(None)
                }
            }
        }
    }
}
