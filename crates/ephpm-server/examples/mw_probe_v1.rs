//! Test fixture (not a shipped module): a genuine ABI-v1 middleware cdylib,
//! built through the real [`ephpm_middleware::declare!`] macro.
//!
//! This exists as a `crate-type = ["cdylib"]` **example** rather than a
//! workspace crate so that an ordinary `cargo test -p ephpm-server` always
//! leaves a loadable `.so`/`.dylib`/`.dll` on disk. The dlopen lane's tests
//! (`tests/middleware_dlopen.rs`) must never be able to silently skip for want
//! of an artifact — which is exactly how that lane went untested: the four
//! shipped `ephpm-middleware-*` cdylibs are not built by `cargo test
//! --workspace`, only by an explicit `cargo build -p <crate>`.
//!
//! `invoke` echoes the request back through response headers, so the test can
//! prove every host-table accessor survived the FFI round trip rather than
//! just proving that `dlopen` returned a handle.

use ephpm_middleware::{Middleware, Request, Response};

/// Fixture module: echoes request context into response headers, and
/// short-circuits one magic path so the `RESPOND` marshaling is covered too.
pub struct Probe {
    /// Value of the mount's `config.tag`, echoed back as `X-Probe-Tag`.
    /// Required, so that a config-driven `init` failure is also exercisable.
    tag: String,
}

impl Middleware for Probe {
    fn init(config: &serde_json::Value) -> Result<Self, String> {
        let tag = config
            .get("tag")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| "probe fixture requires a string `tag` in its config".to_owned())?;
        Ok(Self { tag: tag.to_owned() })
    }

    fn invoke(&self, req: &Request<'_>) -> Response {
        if req.path() == "/__probe/deny" {
            return Response::respond(418, "probe denied").header("X-Probe-Action", "respond");
        }
        Response::cont()
            .response_header("X-Probe-Tag", self.tag.clone())
            .response_header("X-Probe-Method", req.method())
            .response_header("X-Probe-Path", req.path())
            .response_header("X-Probe-Query", req.query())
            .response_header("X-Probe-Remote-Ip", req.remote_ip())
            .response_header("X-Probe-Vhost", req.vhost_id())
            .response_header("X-Probe-Accept", req.header("Accept").unwrap_or("<none>"))
            // Minor-2 request accessors, echoed for the dlopen round-trip test.
            .response_header("X-Probe-Scheme", req.scheme())
            .response_header("X-Probe-Secure", if req.is_secure() { "1" } else { "0" })
            .response_header("X-Probe-Host", req.http_host())
            .response_header("X-Probe-Body", String::from_utf8_lossy(req.body()).into_owned())
    }

    fn describe() -> &'static str {
        "mw-probe-v1 (ephpm-server test fixture)"
    }
}

ephpm_middleware::declare!(Probe);
