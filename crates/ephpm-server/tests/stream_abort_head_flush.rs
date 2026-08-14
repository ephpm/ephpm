//! Regression test for issue #271: an aborted streamed worker response must
//! still put its status line and headers on the wire.
//!
//! Background. `send_response_stream` delivers status + headers to the router
//! the moment PHP calls it, then feeds body chunks over a channel. If the
//! worker dies mid-body, `clear_in_flight_streams` sets the
//! [`StreamAbortFlag`](ephpm_php::worker_bridge::StreamAbortFlag) and drops the
//! sender, and the body stream turns that into an `io::Error` so hyper
//! abandons the response instead of writing the terminating chunk. The client
//! is supposed to observe `200 OK` + headers followed by a transfer that never
//! completes.
//!
//! The bug: hyper's h1 dispatcher (`proto::h1::dispatch::Dispatcher::poll_loop`)
//! runs `poll_read` / `poll_write` / `poll_flush` in that order, and inside
//! `poll_write` the response body is polled as
//! `let item = ready!(body.poll_frame(cx));` immediately followed by
//! `item.map_err(...)?`. A body error therefore propagates out of `poll_write`
//! *and* out of `poll_loop`, so `poll_flush` never runs for that iteration and
//! `poll_catch` goes straight to `Dispatched::Shutdown`. When the worker loses
//! its race with the connection task — i.e. it has already aborted by the time
//! hyper first polls the body — the response head is still sitting in the
//! connection's write buffer and is discarded with the connection. The client
//! then sees a socket that closed without a single response byte, which is
//! indistinguishable from a server crash, and `reqwest::get()` itself fails.
//!
//! This test pins down the ordering that used to lose the head: the abort flag
//! is set and the channel closed **before** the response is even handed to
//! hyper, so hyper's very first body poll sees the abort. No scheduler luck is
//! involved, so the test is deterministic where the E2E flake was not.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use ephpm_server::body::channel_body;
use http_body_util::BodyExt;
use hyper::service::service_fn;
use hyper::{Response, StatusCode};
use hyper_util::rt::TokioIo;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// Serve exactly one HTTP/1.1 connection whose response is an already-aborted
/// streamed worker body, and return everything the client managed to read.
///
/// `pre_chunk` mirrors the E2E fixture: the worker emitted one body chunk
/// before it died. `false` covers the harsher case where it died first.
async fn read_aborted_response(pre_chunk: bool) -> Vec<u8> {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind loopback");
    let addr = listener.local_addr().expect("local addr");

    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("accept");
        let service = service_fn(move |_req| async move {
            let (tx, rx) = tokio::sync::mpsc::channel::<hyper::body::Bytes>(4);
            if pre_chunk {
                tx.try_send(hyper::body::Bytes::from_static(b"STREAM-CHUNK-BEFORE-BAILOUT\n"))
                    .expect("buffer the pre-bailout chunk");
            }

            // The exact ordering `clear_in_flight_streams()` produces, applied
            // BEFORE hyper ever polls the body: flag first, then drop the
            // sender. This is the worker-wins-the-race case from #271.
            let aborted: ephpm_php::worker_bridge::StreamAbortFlag =
                Arc::new(AtomicBool::new(false));
            aborted.store(true, Ordering::SeqCst);
            drop(tx);

            Ok::<_, std::convert::Infallible>(
                Response::builder()
                    .status(StatusCode::OK)
                    .header("content-type", "text/plain; charset=utf-8")
                    .header("x-ephpm-marker", "stream-abort")
                    .body(channel_body(rx, aborted))
                    .expect("build streamed response"),
            )
        });

        // The error is expected: the body deliberately fails so hyper abandons
        // the response. What matters is what reached the socket first.
        let _ = hyper::server::conn::http1::Builder::new()
            .serve_connection(TokioIo::new(stream), service)
            .await;
    });

    let mut client = TcpStream::connect(addr).await.expect("connect");
    client
        .write_all(b"GET /s?__stream_bailout=1 HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n")
        .await
        .expect("send request");

    let mut received = Vec::new();
    // Reads until the server tears the connection down, which it always does
    // here — the body errors either way, so this cannot hang.
    client.read_to_end(&mut received).await.expect("read response");

    server.await.expect("server task");
    received
}

/// The head must reach the client even though the worker aborted before hyper
/// polled the body — and the body must still be left unterminated.
#[tokio::test]
async fn aborted_stream_still_delivers_the_response_head() {
    let received = read_aborted_response(true).await;
    let text = String::from_utf8_lossy(&received).into_owned();

    assert!(
        !received.is_empty(),
        "the client received NOTHING — the response head was discarded with the \
         connection, which is indistinguishable from a server crash (issue #271)"
    );
    assert!(
        text.starts_with("HTTP/1.1 200 OK\r\n"),
        "expected a 200 status line before the abort, got: {text:?}"
    );
    assert!(
        text.to_ascii_lowercase().contains("x-ephpm-marker: stream-abort"),
        "the response headers must be on the wire too, got: {text:?}"
    );
    assert!(
        text.to_ascii_lowercase().contains("transfer-encoding: chunked"),
        "a streamed worker response is chunked, got: {text:?}"
    );
    assert!(
        text.contains("STREAM-CHUNK-BEFORE-BAILOUT"),
        "the chunk the worker produced before dying should have been flushed too, got: {text:?}"
    );

    // The whole point: the transfer must NOT look complete. Under chunked
    // encoding that means the terminating zero-length chunk never appears.
    assert!(
        !received.ends_with(b"0\r\n\r\n"),
        "the body was terminated cleanly — a client cannot tell this from a \
         finished download: {text:?}"
    );
}

/// Same guarantee when the worker died without producing a single chunk: this
/// is the case with the least buffered data, so it is the one most likely to
/// lose the head.
#[tokio::test]
async fn aborted_stream_delivers_the_head_even_with_no_chunks() {
    let received = read_aborted_response(false).await;
    let text = String::from_utf8_lossy(&received).into_owned();

    assert!(
        text.starts_with("HTTP/1.1 200 OK\r\n"),
        "expected a 200 status line even with no body chunk, got: {text:?}"
    );
    assert!(
        !received.ends_with(b"0\r\n\r\n"),
        "the empty body must not be terminated cleanly: {text:?}"
    );
}

/// The control: a streamed response the worker finished normally must still
/// complete cleanly, terminating chunk and all.
#[tokio::test]
async fn clean_stream_still_completes_normally() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind loopback");
    let addr = listener.local_addr().expect("local addr");

    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("accept");
        let service = service_fn(move |_req| async move {
            let (tx, rx) = tokio::sync::mpsc::channel::<hyper::body::Bytes>(4);
            tx.try_send(hyper::body::Bytes::from_static(b"all-good\n")).expect("buffer chunk");
            let aborted: ephpm_php::worker_bridge::StreamAbortFlag =
                Arc::new(AtomicBool::new(false));
            drop(tx); // clean end-of-body, flag left clear

            Ok::<_, std::convert::Infallible>(
                Response::builder()
                    .status(StatusCode::OK)
                    .body(channel_body(rx, aborted))
                    .expect("build streamed response"),
            )
        });
        let _ = hyper::server::conn::http1::Builder::new()
            .serve_connection(TokioIo::new(stream), service)
            .await;
    });

    let mut client = TcpStream::connect(addr).await.expect("connect");
    client
        .write_all(b"GET /s HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n")
        .await
        .expect("send request");
    let mut received = Vec::new();
    client.read_to_end(&mut received).await.expect("read response");
    server.await.expect("server task");

    let text = String::from_utf8_lossy(&received).into_owned();
    assert!(text.starts_with("HTTP/1.1 200 OK\r\n"), "got: {text:?}");
    assert!(text.contains("all-good"), "got: {text:?}");
    assert!(
        received.ends_with(b"0\r\n\r\n"),
        "a completed stream must end with the terminating chunk: {text:?}"
    );
}

/// Sanity check on the body plumbing itself: collecting an aborted body must
/// surface an error rather than a successful (truncated) read.
#[tokio::test]
async fn aborted_body_collect_fails() {
    let (tx, rx) = tokio::sync::mpsc::channel::<hyper::body::Bytes>(4);
    tx.try_send(hyper::body::Bytes::from_static(b"partial")).expect("buffer chunk");
    let aborted: ephpm_php::worker_bridge::StreamAbortFlag = Arc::new(AtomicBool::new(false));
    aborted.store(true, Ordering::SeqCst);
    drop(tx);

    // `Collected<Bytes>` is not `Debug`, so match rather than `expect_err`.
    match channel_body(rx, aborted).collect().await {
        Ok(collected) => {
            panic!("an aborted body collected successfully ({} bytes)", collected.to_bytes().len())
        }
        Err(err) => assert!(
            err.to_string().contains("worker died mid-response"),
            "unexpected body error: {err}"
        ),
    }
}
