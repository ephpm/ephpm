//! `session-cookie` — loadable cdylib shell around the shared implementation
//! in [`ephpm_middleware_builtins::session_cookie`].
//!
//! The middleware itself (signed session-cookie validation with
//! redirect-to-login, docs and tests included) lives in
//! `ephpm-middleware-builtins`, where it is also compiled into every ePHPm
//! binary as the builtin `session-cookie` registry entry — no cdylib needed
//! there. This crate only adds the C ABI exports (`declare!`) so the same
//! module can be dlopened by dynamically linked builds.

pub use ephpm_middleware_builtins::session_cookie::SessionCookie;

ephpm_middleware::declare!(SessionCookie);
