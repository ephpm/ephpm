//! Test fixture (not a shipped module): a hand-rolled middleware in the shape
//! of a module written **before** ABI minor 5 and the optional
//! `ephpm_middleware_config_keys` symbol — the backwards-compatibility case for
//! issues #469 and #480.
//!
//! It exports exactly the three mandatory symbols: no `describe`, no
//! `invoke_response`, and in particular **no `ephpm_middleware_config_keys`**,
//! so the host cannot know its config schema. Its `init` ignores the config
//! entirely, as a C module that never parsed it would. It reads only host-table
//! slots that existed at minor 3 (`request_vhost_id`), and gates on the ABI
//! major exactly like `declare!` does — never on an exact minor, which is what
//! lets a minor-3 module keep loading on a minor-5 host.
//!
//! The dlopen tests prove that such a module still mounts with arbitrary
//! `config` keys (no host-side check can apply to it) and still runs against
//! today's longer host table.
#![allow(unsafe_code)] // Speaks the C middleware ABI by hand; see module docs.

use std::cell::RefCell;
use std::os::raw::{c_char, c_int};
use std::sync::OnceLock;

use ephpm_middleware::abi;

/// The host table, stashed at `init` (a `usize` so the static is `Sync`).
static HOST: OnceLock<usize> = OnceLock::new();

thread_local! {
    /// Backing storage for the one response header handed to the host; valid
    /// until the next `invoke` on this thread, which outlives the host's copy.
    static OUT: RefCell<[abi::EphpmHeaderKv; 1]> = const {
        RefCell::new([abi::EphpmHeaderKv { name: std::ptr::null(), value: std::ptr::null() }])
    };
}

/// Accepts any major-1 host, whatever its minor, and any config.
#[unsafe(no_mangle)]
unsafe extern "C" fn ephpm_middleware_init(
    abi_version: u32,
    _config_json: *const c_char,
    host: *const abi::EphpmHostV1,
) -> c_int {
    if (abi_version >> 24) != 1 || host.is_null() {
        return -1;
    }
    let _ = HOST.set(host as usize);
    0
}

/// `CONTINUE`, appending `X-Legacy-Vhost: <site key>` — or `<none>` when the
/// (minor-3) `request_vhost_id` slot returns NULL, which this module, like
/// every correct C module, null-checks.
#[unsafe(no_mangle)]
unsafe extern "C" fn ephpm_middleware_invoke(
    request: *const abi::EphpmRequest,
    response_out: *mut abi::EphpmResponse,
) -> c_int {
    let Some(&host) = HOST.get() else {
        return -1;
    };
    if response_out.is_null() {
        return -1;
    }
    // SAFETY: the host table outlives the process (ABI contract) and was
    // checked non-null at init.
    let host = unsafe { &*(host as *const abi::EphpmHostV1) };
    // SAFETY: `request` is live for this call; the slot predates minor 3.
    let vhost = unsafe { (host.request_vhost_id)(request) };
    // A non-null vhost id is a NUL-terminated string that lives as long as the
    // request — long enough to hand straight back for the host to copy.
    let value = if vhost.is_null() { c"<none>".as_ptr() } else { vhost };
    OUT.with(|cell| {
        let mut out = cell.borrow_mut();
        out[0] = abi::EphpmHeaderKv { name: c"X-Legacy-Vhost".as_ptr(), value };
        // SAFETY: `response_out` is a valid, host-owned, zero-initialised
        // out-struct; `out` stays alive until the next invoke on this thread.
        unsafe {
            (*response_out).action = abi::ACTION_CONTINUE;
            (*response_out).response_headers = out.as_ptr();
            (*response_out).response_headers_len = 1;
        }
    });
    0
}

/// Nothing to release.
#[unsafe(no_mangle)]
unsafe extern "C" fn ephpm_middleware_shutdown() {}
