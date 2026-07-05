//! `cors` — loadable cdylib shell around the shared implementation in
//! [`ephpm_middleware_builtins::cors`].
//!
//! The middleware itself (CORS preflight handling and response headers, docs
//! and tests included) lives in `ephpm-middleware-builtins`, where it is
//! also compiled into every ePHPm binary as the builtin `cors` registry
//! entry — no cdylib needed there. This crate only adds the C ABI exports
//! (`declare!`) so the same module can be dlopened by dynamically linked
//! builds.

pub use ephpm_middleware_builtins::cors::Cors;

ephpm_middleware::declare!(Cors);
