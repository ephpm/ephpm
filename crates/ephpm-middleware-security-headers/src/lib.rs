//! `security-headers` — loadable cdylib shell around the shared
//! implementation in [`ephpm_middleware_builtins::security_headers`].
//!
//! The middleware itself (standard security response headers, docs and tests
//! included) lives in `ephpm-middleware-builtins`, where it is also compiled
//! into every ePHPm binary as the builtin `security-headers` registry
//! entry — no cdylib needed there. This crate only adds the C ABI exports
//! (`declare!`) so the same module can be dlopened by dynamically linked
//! builds. It is also the guinea pig for `ephpm-server`'s dlopen round-trip
//! test.

pub use ephpm_middleware_builtins::security_headers::SecurityHeaders;

ephpm_middleware::declare!(SecurityHeaders);
