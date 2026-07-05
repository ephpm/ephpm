//! `ratelimit` — loadable cdylib shell around the shared implementation in
//! [`ephpm_middleware_builtins::ratelimit`].
//!
//! The middleware itself (fixed-window per-client rate limiting via the
//! embedded KV store, docs and tests included) lives in
//! `ephpm-middleware-builtins`, where it is also compiled into every ePHPm
//! binary as the builtin `ratelimit` registry entry — no cdylib needed
//! there. This crate only adds the C ABI exports (`declare!`) so the same
//! module can be dlopened by dynamically linked builds.

pub use ephpm_middleware_builtins::ratelimit::RateLimit;

ephpm_middleware::declare!(RateLimit);
