//! In-process ("builtin") middleware — the static-registry execution lane.
//!
//! A fully static musl release binary cannot `dlopen()` anything, so the
//! shared-library lane cannot be the only way to run middleware.
//! [`BuiltinModule`] adapts any [`Middleware`] implementation **compiled into
//! the host binary** so the chain can call it directly: same trait, same
//! [`Request`] view, same process-wide host table — the request accessors and
//! KV callbacks don't care whether their caller is a dlopened module or the
//! same binary. There is no C ABI round-trip and no dlopen anywhere on this
//! path.
//!
//! Semantics mirror the [`declare!`](crate::declare) glue exactly: an `init`
//! error or panic aborts server startup (fail-fast), a panicking `invoke`
//! fails closed as a 500, and a panicking `shutdown` is swallowed.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;

use crate::host::{RequestCtx, host_table};
use crate::{Middleware, Response, abi};

/// One middleware compiled into the host binary, adapted behind object-safe
/// closures so a chain can hold heterogeneous modules.
///
/// Built from any `T:`[`Middleware`] via [`BuiltinModule::init`] — the same
/// trait a cdylib module implements; only the transport differs.
pub struct BuiltinModule {
    run: Box<dyn Fn(&RequestCtx) -> Response + Send + Sync>,
    stop: Box<dyn Fn() + Send + Sync>,
    description: &'static str,
}

impl std::fmt::Debug for BuiltinModule {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BuiltinModule").field("describe", &self.description).finish_non_exhaustive()
    }
}

/// Verdict of one builtin module invocation, in owned host types — the same
/// three actions as the C [`abi::EphpmResponse`], minus the FFI marshaling.
pub enum Verdict {
    /// Keep walking the chain; append `response_headers` to the eventual
    /// client response.
    Continue {
        /// Headers appended to the eventual client response.
        response_headers: Vec<(String, String)>,
    },
    /// Accumulate request overrides, then keep walking the chain.
    Rewrite {
        /// Replacement request path (`None` = keep the current one).
        path: Option<String>,
        /// Request-header overrides handed to PHP.
        header_overrides: Vec<(String, String)>,
        /// Headers appended to the eventual client response.
        response_headers: Vec<(String, String)>,
    },
    /// Short-circuit: return this response to the client; PHP never runs.
    Respond {
        /// HTTP status code chosen by the module.
        status: u16,
        /// Response body bytes.
        body: Vec<u8>,
        /// Extra response headers set by the module.
        headers: Vec<(String, String)>,
    },
}

impl From<Response> for Verdict {
    fn from(r: Response) -> Self {
        match r.action {
            abi::ACTION_RESPOND => {
                Self::Respond { status: r.status, body: r.body, headers: r.headers }
            }
            abi::ACTION_REWRITE => Self::Rewrite {
                path: r.rewrite_path,
                header_overrides: r.headers,
                response_headers: r.response_headers,
            },
            // `Response` can only be built via cont()/respond()/rewrite(),
            // so everything else is CONTINUE.
            _ => Self::Continue { response_headers: r.response_headers },
        }
    }
}

impl BuiltinModule {
    /// Construct `T` from the mount's `config` (JSON; pass `Null` when the
    /// mount has none) — the in-process equivalent of the `declare!` glue's
    /// `ephpm_middleware_init`, including panic containment around the
    /// module's own `init`.
    ///
    /// # Errors
    ///
    /// Returns the module's `init` error message, or a synthetic one when
    /// `init` panicked. Either way the caller must fail startup (fail-fast,
    /// like a dlopened module whose `init` returns non-zero).
    pub fn init<T: Middleware>(config: &serde_json::Value) -> Result<Self, String> {
        let instance = catch_unwind(AssertUnwindSafe(|| T::init(config)))
            .map_err(|_| "middleware init panicked".to_owned())??;
        let instance = Arc::new(instance);
        let run = {
            let instance = Arc::clone(&instance);
            Box::new(move |ctx: &RequestCtx| {
                // SAFETY: `ctx` is live for the duration of this call and
                // `host_table()` is 'static — the same invariants the dlopen
                // lane provides, just without crossing a library boundary.
                let req = unsafe { crate::Request::from_raw(ctx.as_abi(), host_table()) };
                instance.invoke(&req)
            })
        };
        let stop = Box::new(move || instance.shutdown());
        let description = match T::describe() {
            "" => std::any::type_name::<T>(),
            d => d,
        };
        Ok(Self { run, stop, description })
    }

    /// Run the module for one request. A panic fails closed as a 500 —
    /// identical to the `declare!` glue's containment.
    #[must_use]
    pub fn invoke(&self, ctx: &RequestCtx) -> Verdict {
        match catch_unwind(AssertUnwindSafe(|| (self.run)(ctx))) {
            Ok(response) => response.into(),
            // Fail-closed: a broken auth middleware must not fail-open.
            Err(_) => Response::respond(500, "middleware panic").into(),
        }
    }

    /// Call the module's `shutdown`; panics are swallowed (as in `declare!`).
    pub fn shutdown(&self) {
        let _ = catch_unwind(AssertUnwindSafe(|| (self.stop)()));
    }

    /// Name/version string for logs: [`Middleware::describe`], falling back
    /// to the implementing type's name when it returns `""`.
    #[must_use]
    pub fn describe(&self) -> &'static str {
        self.description
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Request;

    /// Test middleware: echoes behavior chosen by its config.
    struct Probe {
        mode: String,
    }

    impl Middleware for Probe {
        fn init(config: &serde_json::Value) -> Result<Self, String> {
            match config.get("mode").and_then(serde_json::Value::as_str) {
                Some("init-panic") => panic!("boom in init"),
                Some("init-err") => Err("probe refused".to_owned()),
                Some(mode) => Ok(Self { mode: mode.to_owned() }),
                None => Err("`mode` is required".to_owned()),
            }
        }

        fn invoke(&self, req: &Request<'_>) -> Response {
            match self.mode.as_str() {
                "panic" => panic!("boom in invoke"),
                "respond" => Response::respond(403, "denied").header("X-Probe", "1"),
                "rewrite" => Response::rewrite()
                    .path("/rewritten.php")
                    .header("X-Req", "override")
                    .response_header("X-Resp", "appended"),
                // Continue, proving the request view works in-process.
                _ => Response::cont().response_header("X-Method", req.method()),
            }
        }

        fn describe() -> &'static str {
            "probe/1.0"
        }
    }

    fn ctx() -> RequestCtx {
        RequestCtx::new("GET", "/x.php", "a=1", "203.0.113.5", "t.example", &[])
    }

    fn probe(mode: &str) -> BuiltinModule {
        BuiltinModule::init::<Probe>(&serde_json::json!({ "mode": mode })).expect("init")
    }

    #[test]
    fn init_error_and_panic_fail_fast() {
        let err = BuiltinModule::init::<Probe>(&serde_json::json!({ "mode": "init-err" }))
            .expect_err("init must fail");
        assert_eq!(err, "probe refused");
        let err = BuiltinModule::init::<Probe>(&serde_json::json!({ "mode": "init-panic" }))
            .expect_err("init panic must fail");
        assert_eq!(err, "middleware init panicked");
    }

    #[test]
    fn continue_verdict_reads_the_request_in_process() {
        let module = probe("continue");
        assert_eq!(module.describe(), "probe/1.0");
        match module.invoke(&ctx()) {
            Verdict::Continue { response_headers } => {
                assert_eq!(response_headers, vec![("X-Method".to_owned(), "GET".to_owned())]);
            }
            _ => panic!("expected CONTINUE"),
        }
    }

    #[test]
    fn respond_verdict_maps_status_body_headers() {
        match probe("respond").invoke(&ctx()) {
            Verdict::Respond { status, body, headers } => {
                assert_eq!(status, 403);
                assert_eq!(body, b"denied");
                assert_eq!(headers, vec![("X-Probe".to_owned(), "1".to_owned())]);
            }
            _ => panic!("expected RESPOND"),
        }
    }

    #[test]
    fn rewrite_verdict_maps_path_and_both_header_lists() {
        match probe("rewrite").invoke(&ctx()) {
            Verdict::Rewrite { path, header_overrides, response_headers } => {
                assert_eq!(path.as_deref(), Some("/rewritten.php"));
                assert_eq!(header_overrides, vec![("X-Req".to_owned(), "override".to_owned())]);
                assert_eq!(response_headers, vec![("X-Resp".to_owned(), "appended".to_owned())]);
            }
            _ => panic!("expected REWRITE"),
        }
    }

    #[test]
    fn invoke_panic_fails_closed_as_500() {
        match probe("panic").invoke(&ctx()) {
            Verdict::Respond { status, body, .. } => {
                assert_eq!(status, 500);
                assert_eq!(body, b"middleware panic");
            }
            _ => panic!("a panicking invoke must fail closed"),
        }
    }
}
