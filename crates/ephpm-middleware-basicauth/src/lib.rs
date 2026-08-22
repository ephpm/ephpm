//! `basic-auth` — loadable cdylib shell around the shared implementation in
//! [`ephpm_middleware_builtins::basicauth`].
//!
//! The middleware itself (HTTP Basic authentication with static or
//! KV-resolved per-site credentials, docs and tests included) lives in
//! `ephpm-middleware-builtins`, where it is also compiled into every ePHPm
//! binary as the builtin `basic-auth` registry entry — no cdylib needed
//! there. This crate only adds the C ABI exports (`declare!`) so the same
//! module can be dlopened by dynamically linked builds.
//!
//! This artifact is **larger** than the other middleware cdylibs because it
//! statically links `aws-lc-rs` for PBKDF2. In the ePHPm binary that library
//! is already present and shared; here it is a second copy. Prefer
//! `library = "basic-auth"` (the builtin lane) unless you specifically need a
//! loadable module.

pub use ephpm_middleware_builtins::basicauth::BasicAuth;

ephpm_middleware::declare!(BasicAuth);
