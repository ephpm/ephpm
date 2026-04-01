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
