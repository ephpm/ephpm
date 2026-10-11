//! Test fixture (not a shipped module): a hand-rolled middleware that exports
//! the optional `ephpm_middleware_config_keys` symbol (issue #480) but answers
//! it **wrongly** — a JSON object instead of an array of key names.
//!
//! The host must refuse to mount it rather than guess: treating the answer as
//! "undeclared" would silently drop the check the module asked for, and
//! treating it as a declaration of whatever parsed would reject valid keys. A
//! module that answers the question wrongly is a bug, not a compatibility case.
//! Otherwise this is a fully working module, so the refusal cannot pass for
//! some other reason.
#![allow(unsafe_code)] // Speaks the C middleware ABI by hand; see module docs.

use std::os::raw::{c_char, c_int};

use ephpm_middleware::abi;

/// The malformed declaration.
#[unsafe(no_mangle)]
unsafe extern "C" fn ephpm_middleware_config_keys() -> *const c_char {
    c"{\"tag\":true}".as_ptr()
}

/// Would accept any major-1 host — the loader must never get this far.
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

/// `CONTINUE` with no edits.
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
