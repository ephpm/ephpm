//! The four in-tree ePHPm middleware modules as plain Rust library code.
//!
//! Each module here is an ordinary [`ephpm_middleware::Middleware`]
//! implementation with **no C ABI exports** — that is what lets
//! `ephpm-server` link all of them into one binary and run them in-process
//! through the static builtin registry (`library = "jwt"` works even in a
//! custom fully static build, where `dlopen` does not exist).
//!
//! The sibling `ephpm-middleware-{jwt,cors,ratelimit,security-headers}`
//! crates are thin cdylib shells: they re-export these types and add the
//! `declare!` C ABI glue, producing the loadable `.so`/`.dylib`/`.dll`
//! artifacts for the dynamic (dlopen) lane. The shells cannot be merged into
//! one binary — four copies of the same `ephpm_middleware_*` export symbols
//! collide at link time — which is exactly why the implementations live
//! here instead.

pub mod cors;
pub mod jwt;
pub mod ratelimit;
pub mod security_headers;
