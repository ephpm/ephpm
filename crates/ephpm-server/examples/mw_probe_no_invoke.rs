//! Test fixture (not a shipped module): a cdylib that exports `init` and
//! `shutdown` but **not** `ephpm_middleware_invoke`.
//!
//! Models the most likely authoring mistake — a module crate that builds
//! cleanly but forgot `declare!`, or that was built with the wrong
//! `crate-type` so only part of the ABI surfaced. The loader must name the
//! missing symbol at startup rather than dlopen it and discover the gap on the
//! first request.
#![allow(unsafe_code)] // Speaks the C middleware ABI by hand; see module docs.

use std::os::raw::{c_char, c_int};

use ephpm_middleware::abi;

/// Would accept the handshake — the point is that the loader never gets here,
/// because symbol resolution fails first.
#[unsafe(no_mangle)]
unsafe extern "C" fn ephpm_middleware_init(
    _abi_version: u32,
    _config_json: *const c_char,
    _host: *const abi::EphpmHostV1,
) -> c_int {
    0
}

/// Deliberately no `ephpm_middleware_invoke` export in this fixture.
#[unsafe(no_mangle)]
unsafe extern "C" fn ephpm_middleware_shutdown() {}
