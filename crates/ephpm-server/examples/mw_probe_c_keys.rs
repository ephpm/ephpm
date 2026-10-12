//! Test fixture (not a shipped module): a hand-rolled middleware — the shape of
//! a module written in C, Zig or anything else that does not use the Rust
//! authoring kit — that **declares** its config keys through the optional
//! `ephpm_middleware_config_keys` symbol (issue #480).
//!
//! Its `init` deliberately does no key checking of its own, as a C module that
//! reads two keys and ignores the rest would. So when a mount with a typo is
//! refused, the refusal can only have come from the **host**, which is the
//! point: before #480 such a module had no way to get typo protection at all.
#![allow(unsafe_code)] // Speaks the C middleware ABI by hand; see module docs.

use std::os::raw::{c_char, c_int};

use ephpm_middleware::abi;

/// The declaration: a static, NUL-terminated JSON array of strings.
#[unsafe(no_mangle)]
unsafe extern "C" fn ephpm_middleware_config_keys() -> *const c_char {
    c"[\"mode\",\"tag\"]".as_ptr()
}

/// Accepts any major-1 host and any config — no key checking here.
#[unsafe(no_mangle)]
unsafe extern "C" fn ephpm_middleware_init(
    abi_version: u32,
    _config_json: *const c_char,
    host: *const abi::EphpmHostV1,
) -> c_int {
    if (abi_version >> 24) != 1 || host.is_null() {
        return -1;
    }
    0
}

/// `CONTINUE` with no edits (the host zero-initialises `response_out`).
#[unsafe(no_mangle)]
unsafe extern "C" fn ephpm_middleware_invoke(
    _request: *const abi::EphpmRequest,
    _response_out: *mut abi::EphpmResponse,
) -> c_int {
    0
}

/// Nothing to release.
#[unsafe(no_mangle)]
unsafe extern "C" fn ephpm_middleware_shutdown() {}
