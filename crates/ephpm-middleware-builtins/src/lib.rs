//! The ten in-tree official ePHPm middleware modules as plain Rust library
//! code.
//!
//! Each module here is an ordinary [`ephpm_middleware::Middleware`]
//! implementation with **no C ABI exports** — that is what lets
//! `ephpm-server` link all of them into one binary and run them in-process
//! through the static builtin registry (`library = "jwt"` works even in a
//! custom fully static build, where `dlopen` does not exist).
//!
//! The modules, by phase:
//!
//! - **Request phase** ([`ephpm_middleware::Middleware`]): [`jwt`], [`cors`],
//!   [`ratelimit`], [`security_headers`], [`api_key`], [`ip_allowlist`],
//!   [`maintenance_mode`], [`redirect`].
//! - **Request + response phase** (also
//!   [`ephpm_middleware::ResponseMiddleware`], registered in the server via
//!   [`ephpm_middleware::builtin::BuiltinModule::init_response`]):
//!   [`request_id`], [`header_transform`].
//!
//! The sibling `ephpm-middleware-<name>` crates in the `ephpm/middleware`
//! (examples) repository are thin cdylib shells: they re-export these types
//! and add the `declare!` C ABI glue, producing the loadable
//! `.so`/`.dylib`/`.dll` artifacts for the dynamic (dlopen) lane. The shells
//! cannot be merged into one binary — many copies of the same
//! `ephpm_middleware_*` export symbols collide at link time — which is exactly
//! why the implementations live here instead.

pub mod api_key;
pub mod cors;
pub mod header_transform;
pub mod ip_allowlist;
pub mod jwt;
pub mod maintenance_mode;
pub mod ratelimit;
pub mod redirect;
pub mod request_id;
pub mod security_headers;
