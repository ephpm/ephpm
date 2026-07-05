//! `jwt` — loadable cdylib shell around the shared implementation in
//! [`ephpm_middleware_builtins::jwt`].
//!
//! The middleware itself (HS256 JWT bearer-token validation, docs and tests
//! included) lives in `ephpm-middleware-builtins`, where it is also compiled
//! into every ePHPm binary as the builtin `jwt` registry entry — no cdylib
//! needed there. This crate only adds the C ABI exports (`declare!`) so the
//! same module can be dlopened by dynamically linked builds and serves as a
//! reference for out-of-tree modules.

pub use ephpm_middleware_builtins::jwt::Jwt;

ephpm_middleware::declare!(Jwt);
