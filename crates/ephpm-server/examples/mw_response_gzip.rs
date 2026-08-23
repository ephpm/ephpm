//! Proof module for the middleware **response phase**: a real gzip response
//! compressor built through the `declare!(Type, response)` macro.
//!
//! It demonstrates the two-phase model end to end — the request phase is a
//! no-op (`CONTINUE`), and the response phase transforms the *generated*
//! response: it gzips the body, sets `Content-Encoding: gzip`, and drops the
//! now-stale `Content-Length`/`ETag`. Because the host runs the response phase
//! on **static** responses too, mounting this module compresses static assets
//! (`.html`, `.css`, JSON) — something a request-phase-only module could never
//! do.
//!
//! It is shipped here as a `crate-type = ["cdylib"]` example (like the
//! `mw_probe_*` fixtures) so `cargo test -p ephpm-server` always leaves a
//! loadable library on disk for `tests/middleware_response_phase.rs`, and so
//! it can be dropped next to a real `ephpm` binary and driven with `curl`
//! (see the middleware docs).
//!
//! Deliberately conservative — it compresses only when the client sent
//! `Accept-Encoding: gzip`, the response is not already encoded, and the body
//! is non-empty — because the response phase is a transform, not a gate: it
//! must never turn a good response into a broken one.

use std::io::Write;

use ephpm_middleware::{Middleware, Request, Response, ResponseMiddleware, ResponseView};
use flate2::Compression;
use flate2::write::GzEncoder;

/// gzip response compressor.
pub struct Gzip {
    /// Bodies at least this many bytes are compressed (smaller ones are left
    /// alone — gzip framing can exceed the savings). Defaults to 32.
    min_size: usize,
}

impl Middleware for Gzip {
    fn init(config: &serde_json::Value) -> Result<Self, String> {
        let min_size = config
            .get("min_size")
            .and_then(serde_json::Value::as_u64)
            .map_or(32, |n| usize::try_from(n).unwrap_or(usize::MAX));
        Ok(Self { min_size })
    }

    // Request phase: nothing to gate — all the work is in the response phase.
    fn invoke(&self, _req: &Request<'_>) -> Response {
        Response::cont()
    }

    fn describe() -> &'static str {
        "mw-response-gzip (ephpm-server proof module)"
    }
}

impl ResponseMiddleware for Gzip {
    fn invoke_response(&self, req: &Request<'_>, resp: &mut ResponseView<'_>) {
        // Only when the client opted in.
        let accepts_gzip = req
            .header("Accept-Encoding")
            .is_some_and(|v| v.split(',').any(|enc| enc.trim().eq_ignore_ascii_case("gzip")));
        if !accepts_gzip {
            return;
        }
        // Never double-encode a response another layer already compressed.
        if resp.header("Content-Encoding").is_some() {
            return;
        }
        let body = resp.body();
        if body.len() < self.min_size {
            return;
        }

        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        if encoder.write_all(body).is_err() {
            return;
        }
        let Ok(compressed) = encoder.finish() else {
            return;
        };

        resp.set_body(compressed);
        resp.set_header("Content-Encoding", "gzip");
        resp.set_header("Vary", "Accept-Encoding");
        // The host recomputes Content-Length for the replaced body, but an
        // ETag computed over the identity body no longer matches the encoded
        // one — drop it rather than serve a wrong validator.
        resp.remove_header("ETag");
    }
}

ephpm_middleware::declare!(Gzip, response);
