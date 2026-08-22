//! The in-tree ePHPm middleware modules as plain Rust library code.
//!
//! Each module here is an ordinary [`ephpm_middleware::Middleware`]
//! implementation with **no C ABI exports** — that is what lets
//! `ephpm-server` link all of them into one binary and run them in-process
//! through the static builtin registry (`library = "jwt"` works even in a
//! custom fully static build, where `dlopen` does not exist).
//!
//! The sibling
//! `ephpm-middleware-{jwt,cors,ratelimit,security-headers,session-cookie}`
//! crates are thin cdylib shells: they re-export these types and add the
//! `declare!` C ABI glue, producing the loadable `.so`/`.dylib`/`.dll`
//! artifacts for the dynamic (dlopen) lane. The shells cannot be merged into
//! one binary — several copies of the same `ephpm_middleware_*` export
//! symbols collide at link time — which is exactly why the implementations
//! live here instead.
//!
//! [`hs256`] is not a middleware: it is the token-verification core that
//! [`jwt`] (API bearer tokens) and [`session_cookie`] (browser sessions)
//! both call, so the two gates can differ in policy and failure behaviour
//! without ever differing in crypto.

pub mod cors;
pub mod hs256;
pub mod jwt;
pub mod ratelimit;
pub mod security_headers;
pub mod session_cookie;
