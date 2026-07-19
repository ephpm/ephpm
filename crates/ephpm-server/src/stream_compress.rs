//! Streaming brotli compression for worker-mode streamed responses.
//!
//! Wraps the `body_rx` channel of a streamed worker response
//! (`send_response_stream`) in a brotli encoder task: each chunk PHP
//! produces is fed through one long-lived encoder and flushed
//! (`BROTLI_OPERATION_FLUSH`, via [`brotli::CompressorWriter::flush`]) so
//! the compressed bytes emitted so far always decode to exactly the
//! plaintext chunks so far. The client can therefore decode every SSE
//! event the moment its frame arrives — no buffering until stream end —
//! while the encoder *window* persists across the whole stream.
//!
//! That persistent window is the point: Datastar-style SSE re-renders
//! send near-identical markup over and over, so after the first render
//! each subsequent event compresses against the previous ones and
//! shrinks to a small delta on the wire.
//!
//! # Zero cost when off
//!
//! This module is only entered when `[server.response]
//! compression_streaming` matches the response (see
//! `Router`/`build_streamed_worker_response`). With the default `"off"`,
//! no encoder, task, or channel is created — the response body is the
//! original channel, identical to previous releases.
//!
//! # Quality / window choice (fixed, not knobs)
//!
//! Quality **5**, `lgwin` **22** (4 MiB window):
//! - Flushing per event forfeits most of what higher qualities buy
//!   (they win by buffering more input before emitting), while their CPU
//!   cost per flush grows steeply. q5 keeps per-event encode cost in the
//!   microseconds for KB-scale events.
//! - The 4 MiB window is what makes event N compress against events
//!   N-1, N-2, … for any realistic SSE session; it matches the lgwin the
//!   buffered brotli path already uses (`router::brotli_compress`).
//!
//! The buffered-path `compression_level` knob is deliberately not reused
//! here: it defaults to 1 (tuned for one-shot whole-body gzip), which
//! would gut the cross-event ratio that motivates this feature.

use std::io::Write;

use hyper::body::Bytes;
use tokio::sync::mpsc;

/// Brotli quality for streamed responses. See module docs for rationale.
const STREAM_QUALITY: u32 = 5;
/// Brotli window (log2) for streamed responses: 4 MiB shared across the
/// stream's lifetime.
const STREAM_LGWIN: u32 = 22;
/// Encoder scratch buffer size (same as the buffered path).
const ENCODER_BUF: usize = 4096;

/// Wrap a streamed-response body channel in a brotli encoder task.
///
/// Returns a new receiver carrying the compressed frames. One brotli
/// flush is issued per input chunk, so chunk boundaries (one chunk = one
/// SSE event from PHP's `send_response_stream` pump) remain
/// independently decodable.
///
/// Lifecycle mirrors the uncompressed path:
/// - upstream close (PHP finished) → encoder is finished and the final
///   frame + close propagate downstream;
/// - downstream close (client gone) → the task drops the upstream
///   receiver, which is what stops the PHP `response_chunk` pump.
#[must_use]
pub fn brotli_stream_body(mut body_rx: mpsc::Receiver<Bytes>) -> mpsc::Receiver<Bytes> {
    // Same depth as the worker body channel: keeps the end-to-end
    // backpressure behavior (PHP → encoder → hyper) equivalent to the
    // uncompressed PHP → hyper pipeline.
    let (tx, compressed_rx) = mpsc::channel::<Bytes>(ephpm_php::worker_bridge::BODY_CHANNEL_DEPTH);

    tokio::spawn(async move {
        let mut encoder =
            brotli::CompressorWriter::new(Vec::new(), ENCODER_BUF, STREAM_QUALITY, STREAM_LGWIN);

        while let Some(chunk) = body_rx.recv().await {
            // Writing to a Vec sink cannot fail; treat an encoder error as
            // a fatal stream abort (client sees a truncated brotli stream
            // and the chunked body ends — same failure surface as an
            // aborted uncompressed stream).
            if encoder.write_all(&chunk).is_err() || encoder.flush().is_err() {
                tracing::warn!("streaming brotli encoder failed — aborting stream");
                return;
            }
            // After a flush every compressed byte for the input so far is
            // in the sink; steal it and forward one frame per input chunk.
            let frame = std::mem::take(encoder.get_mut());
            if !frame.is_empty() && tx.send(Bytes::from(frame)).await.is_err() {
                // Client disconnected. Dropping `body_rx` (and the encoder)
                // closes the worker's channel, stopping the PHP pump.
                return;
            }
        }

        // Upstream finished cleanly — emit the brotli end-of-stream frame.
        let tail = encoder.into_inner();
        if !tail.is_empty() {
            let _ = tx.send(Bytes::from(tail)).await;
        }
    });

    compressed_rx
}

#[cfg(test)]
mod tests {
    use brotli::enc::StandardAlloc;
    use brotli::{BrotliDecompressStream, BrotliResult, BrotliState};

    use super::*;

    /// Decode `input` (a prefix of a brotli stream) with a streaming
    /// decoder, returning everything decodable from it. Panics on corrupt
    /// input; `NeedsMoreInput` (an unfinished stream) is fine — that is
    /// exactly the mid-stream situation a flushed prefix represents.
    fn decode_prefix(input: &[u8]) -> Vec<u8> {
        let mut state = BrotliState::new(
            StandardAlloc::default(),
            StandardAlloc::default(),
            StandardAlloc::default(),
        );
        let mut output = Vec::new();
        let mut buf = vec![0u8; 16 * 1024];
        let mut available_in = input.len();
        let mut input_offset = 0;
        loop {
            let mut available_out = buf.len();
            let mut output_offset = 0;
            let mut written = 0;
            let res = BrotliDecompressStream(
                &mut available_in,
                &mut input_offset,
                input,
                &mut available_out,
                &mut output_offset,
                &mut buf,
                &mut written,
                &mut state,
            );
            output.extend_from_slice(&buf[..output_offset]);
            match res {
                BrotliResult::ResultSuccess | BrotliResult::NeedsMoreInput => {
                    if available_in == 0 && output_offset == 0 {
                        return output;
                    }
                }
                BrotliResult::NeedsMoreOutput => {}
                BrotliResult::ResultFailure => panic!("corrupt brotli prefix"),
            }
        }
    }

    /// Each event must be decodable as soon as its compressed frame
    /// arrives — the per-chunk-flush guarantee this module exists for.
    #[tokio::test]
    async fn chunks_decode_incrementally() {
        let (tx, rx) = mpsc::channel::<Bytes>(8);
        let mut out = brotli_stream_body(rx);

        let events = [
            "event: patch\ndata: elements <div id=\"grid\">aaaa</div>\n\n",
            "event: patch\ndata: elements <div id=\"grid\">aaab</div>\n\n",
            ": keepalive\n\n",
            "event: patch\ndata: elements <div id=\"grid\">aaac</div>\n\n",
        ];

        let mut wire = Vec::new();
        let mut plain = Vec::new();
        for event in events {
            tx.send(Bytes::from_static(event.as_bytes())).await.unwrap();
            let frame = out.recv().await.expect("one compressed frame per event");
            wire.extend_from_slice(&frame);
            plain.extend_from_slice(event.as_bytes());
            // The wire bytes so far — an UNFINISHED brotli stream — must
            // already decode to the full plaintext so far.
            assert_eq!(
                decode_prefix(&wire),
                plain,
                "event not decodable at its own frame boundary"
            );
        }

        // Clean end of stream: sender drop finishes the encoder.
        drop(tx);
        while let Some(frame) = out.recv().await {
            wire.extend_from_slice(&frame);
        }
        assert_eq!(decode_prefix(&wire), plain, "final stream must round-trip");
    }

    /// The persistent window is the headline: repeated similar events
    /// must compress far better than the first one.
    #[tokio::test]
    async fn window_persists_across_events() {
        let (tx, rx) = mpsc::channel::<Bytes>(8);
        let mut out = brotli_stream_body(rx);

        // A pixelboard-style ~5 KB re-render with one cell changed per event.
        let render = |n: usize| {
            use std::fmt::Write as _;
            let mut cells = String::from("event: patch\ndata: elements <div id=\"grid\">");
            for i in 0..144 {
                let color = if i == n { "#22c55e" } else { "#27272a" };
                let _ =
                    write!(cells, "<button id=\"c-{i}\" style=\"background:{color}\"></button>");
            }
            cells.push_str("</div>\n\n");
            cells
        };

        tx.send(Bytes::from(render(0))).await.unwrap();
        let first = out.recv().await.unwrap().len();

        let mut later = Vec::new();
        for n in 1..20 {
            tx.send(Bytes::from(render(n))).await.unwrap();
            later.push(out.recv().await.unwrap().len());
        }
        let avg_later = later.iter().sum::<usize>() / later.len();

        assert!(
            avg_later * 5 < first,
            "window sharing should shrink repeat events ≥5x: first={first}B, avg_later={avg_later}B"
        );
    }

    /// Client disconnect (downstream receiver dropped) must close the
    /// upstream channel so the PHP pump stops.
    #[tokio::test]
    async fn downstream_drop_closes_upstream() {
        let (tx, rx) = mpsc::channel::<Bytes>(8);
        let out = brotli_stream_body(rx);
        drop(out);

        // The encoder task exits after its next recv/send cycle; the
        // sender then observes a closed channel.
        tx.send(Bytes::from_static(b"data: x\n\n")).await.ok();
        tokio::time::timeout(std::time::Duration::from_secs(5), tx.closed())
            .await
            .expect("upstream must close after downstream drop");
    }

    /// Empty input stream: no frames, clean close (brotli's empty-stream
    /// terminator is still emitted so the client sees valid brotli).
    #[tokio::test]
    async fn empty_stream_closes_cleanly() {
        let (tx, rx) = mpsc::channel::<Bytes>(8);
        let mut out = brotli_stream_body(rx);
        drop(tx);

        let mut wire = Vec::new();
        while let Some(frame) = out.recv().await {
            wire.extend_from_slice(&frame);
        }
        assert_eq!(decode_prefix(&wire), b"", "empty stream must decode to empty");
    }
}
