//! Minimal response-phase proof module: injects a configurable response header
//! (default `X-Resp-Phase: 1`) during the response phase.
//!
//! Deliberately trivial and free of the built-in compression's competition, so
//! the router integration tests can assert "the response phase ran on this
//! response" by a single header check — including on the **static-file** path,
//! which had no middleware at all before the response phase existed.
//!
//! Shipped as a `crate-type = ["cdylib"]` example so `cargo test -p
//! ephpm-server` always leaves it loadable on disk.

use ephpm_middleware::{Middleware, Request, Response, ResponseMiddleware, ResponseView};

/// Injects one response header in the response phase.
pub struct HeaderStamp {
    name: String,
    value: String,
}

impl Middleware for HeaderStamp {
    fn init(config: &serde_json::Value) -> Result<Self, String> {
        Ok(Self {
            name: config
                .get("header")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("X-Resp-Phase")
                .to_owned(),
            value: config
                .get("value")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("1")
                .to_owned(),
        })
    }

    // Request phase: no-op. The header is added in the response phase, so its
    // presence proves the response phase ran.
    fn invoke(&self, _req: &Request<'_>) -> Response {
        Response::cont()
    }

    fn describe() -> &'static str {
        "mw-response-header (ephpm-server proof module)"
    }
}

impl ResponseMiddleware for HeaderStamp {
    fn invoke_response(&self, _req: &Request<'_>, resp: &mut ResponseView<'_>) {
        resp.set_header(&self.name, &self.value);
    }
}

ephpm_middleware::declare!(HeaderStamp, response);
