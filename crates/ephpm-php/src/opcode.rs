//! Compile-only opcode scanning — the engine-backed static-analysis seam.
//!
//! [`scan_file`] compiles one PHP file with the **same Zend compiler that
//! would execute it** (via `ephpm_opcode_scan_file` in `ephpm_wrapper.c`),
//! walks the resulting `zend_op_array` — including nested op_arrays for
//! functions, methods, closures, and arrow functions — and reports every
//! statically-named call to a caller-supplied sink list, plus the `eval`
//! language construct. **Nothing is executed**: the wrapper never calls
//! `zend_execute`, and it compiles under `ZEND_COMPILE_WITHOUT_EXECUTION`
//! (the `opcache_compile_file()` model).
//!
//! Why this beats a text scan: by the time source has been compiled to
//! opcodes, comments are gone, string literals are constants (not code),
//! and the backtick operator has been lowered into a real `shell_exec`
//! call — so an opcode-level match is a genuine call site, never a token
//! that happened to appear in a comment or a string. That is what lets the
//! consuming analyzer report `Confidence::Confirmed` where the naive token
//! pass can only report `Suspected`.
//!
//! # Engine requirements and thread contract
//!
//! Scanning needs an initialized PHP runtime ([`crate::PhpRuntime::init`]).
//! The calling thread needs an active PHP request context; the C wrapper
//! *verifies* that (TSRM registration and `EG(active)`) instead of trusting
//! the caller, and [`scan_file`] lazily registers the current thread —
//! exactly as the request-execution paths do — when it lacks one (the
//! embed-init thread already has the SAPI's initial request and is used
//! as-is). Concurrent scans are serialized by an internal mutex: the
//! compiler mutates per-request engine state (function/class tables), so
//! two interleaved scans would corrupt each other.
//!
//! All setjmp/longjmp stays inside the C wrapper (`zend_try`/`zend_catch`
//! around the entire compile+walk), so this module's FFI call always
//! returns normally and no Rust destructor is ever skipped by a bailout.
//!
//! In stub mode (no `php_linked`), [`scan_file`] returns
//! [`OpcodeScanError::Unavailable`] so callers can degrade to a skip.

use std::path::Path;

/// One dangerous call site found in the compiled opcodes of a PHP file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpcodeHit {
    /// The matched sink name — one of the names passed to [`scan_file`]
    /// (e.g. `"system"`), or `"eval"` for the eval construct.
    pub sink: String,
    /// 1-based source line recorded on the matched opcode.
    pub line: u32,
}

/// Why a scan produced no result.
#[derive(Debug, thiserror::Error)]
pub enum OpcodeScanError {
    /// This binary was built without libphp (stub mode) — opcode scanning
    /// requires a PHP-linked build.
    #[error("opcode scanning requires a PHP-linked build (this binary links no libphp)")]
    Unavailable,
    /// The PHP runtime is not initialized on this process/thread — call
    /// [`crate::PhpRuntime::init`] first, and scan from the initializing
    /// thread.
    #[error("PHP engine context unavailable (initialize PhpRuntime and scan from its thread)")]
    NoEngine,
    /// The file did not compile (parse error, or unreadable). The message
    /// carries PHP's own diagnostic (e.g. `syntax error, unexpected ...`).
    #[error("PHP compile error: {0}")]
    Compile(String),
    /// A fatal error (zend bailout) occurred during compilation — e.g. a
    /// redeclaration inside the file. Caught by the wrapper's `zend_catch`;
    /// treat the scan run as terminal (engine state may be degraded).
    #[error("fatal PHP error during compilation: {0}")]
    Fatal(String),
    /// The path (or a sink name) contained an interior NUL byte and cannot
    /// cross the C boundary.
    #[error("invalid path or sink name (interior NUL byte): {0}")]
    InvalidInput(String),
}

/// Whether opcode scanning is available in this build (i.e. libphp is
/// linked). `false` in stub mode.
#[must_use]
pub fn available() -> bool {
    cfg!(php_linked)
}

/// Compile `path` to opcodes (no execution) and return every statically
/// named call site to one of `sinks`, plus `eval` constructs when `"eval"`
/// is in `sinks`. See the module docs for the model and thread contract.
///
/// # Errors
///
/// - [`OpcodeScanError::Unavailable`] in stub builds;
/// - [`OpcodeScanError::NoEngine`] when the runtime is not initialized or
///   the calling thread has no active PHP request context;
/// - [`OpcodeScanError::Compile`] on a parse error / unreadable file;
/// - [`OpcodeScanError::Fatal`] on a fatal compile-time error (bailout);
/// - [`OpcodeScanError::InvalidInput`] on NUL bytes in the inputs.
#[cfg(php_linked)]
pub fn scan_file(path: &Path, sinks: &[&str]) -> Result<Vec<OpcodeHit>, OpcodeScanError> {
    use std::ffi::{CStr, CString};
    use std::os::raw::{c_char, c_void};
    use std::sync::Mutex;

    /// Serializes scans: the Zend compiler mutates per-request engine state
    /// (function/class tables, compiler globals), which is single-threaded
    /// per context. See the module docs.
    static SCAN_LOCK: Mutex<()> = Mutex::new(());

    if !crate::PhpRuntime::is_ready() {
        return Err(OpcodeScanError::NoEngine);
    }

    let c_path = CString::new(path.to_string_lossy().into_owned())
        .map_err(|_| OpcodeScanError::InvalidInput(path.display().to_string()))?;
    let c_sinks: Vec<CString> = sinks
        .iter()
        .map(|s| CString::new(*s).map_err(|_| OpcodeScanError::InvalidInput((*s).to_owned())))
        .collect::<Result<_, _>>()?;
    let sink_ptrs: Vec<*const c_char> = c_sinks.iter().map(|s| s.as_ptr()).collect();

    /// Accumulates hits across callback invocations. Owned by the caller
    /// frame; the C wrapper only borrows it for the duration of the call.
    struct CbState {
        hits: Vec<OpcodeHit>,
    }

    /// The C→Rust trampoline. Must not unwind (extern "C") and must not
    /// call back into PHP — it runs inside the wrapper's `zend_try` region.
    unsafe extern "C" fn on_hit(ctx: *mut c_void, sink: *const c_char, lineno: u32) {
        // SAFETY: `ctx` is the `&mut CbState` passed to
        // `ephpm_opcode_scan_file` below, valid and exclusively borrowed
        // for the duration of that call; `sink` points at one of the
        // caller-owned NUL-terminated sink CStrings (the wrapper hands
        // back the caller's own pointer), alive for the whole call.
        let (state, sink) = unsafe { (&mut *ctx.cast::<CbState>(), CStr::from_ptr(sink)) };
        state.hits.push(OpcodeHit { sink: sink.to_string_lossy().into_owned(), line: lineno });
    }

    let mut state = CbState { hits: Vec::new() };
    let mut err_buf = [0u8; 1024];

    let guard = SCAN_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut do_scan = || {
        // SAFETY: all pointers outlive the call — `c_path`/`c_sinks`/
        // `sink_ptrs` are locals dropped after it, `state` and `err_buf`
        // live on the caller frame. The wrapper contains the entire compile
        // inside zend_try/zend_catch, so the call always returns normally
        // (no longjmp crosses this frame and no Rust destructor is
        // skipped). PhpRuntime is initialized (checked above) and the
        // wrapper itself verifies this thread's TSRM registration and
        // active request context before touching engine state (returning
        // -3 instead of trusting the caller).
        unsafe {
            crate::ffi::ephpm_opcode_scan_file(
                c_path.as_ptr(),
                sink_ptrs.as_ptr(),
                sink_ptrs.len(),
                on_hit,
                (&raw mut state).cast::<c_void>(),
                err_buf.as_mut_ptr().cast::<c_char>(),
                err_buf.len(),
            )
        }
    };
    let mut rc = do_scan();
    if rc == -3 {
        // This thread has no PHP context yet. The embed-init thread always
        // has one (php_embed_init's initial request), so this is some other
        // thread — register it exactly as the request paths do
        // (ts_resource + php_request_startup, retired on thread exit by
        // ThreadPhpGuard) and retry once.
        crate::PhpRuntime::ensure_thread_registered().map_err(|_| OpcodeScanError::NoEngine)?;
        rc = do_scan();
    }
    drop(guard);

    let err_msg = || {
        let end = err_buf.iter().position(|&b| b == 0).unwrap_or(err_buf.len());
        String::from_utf8_lossy(&err_buf[..end]).into_owned()
    };
    // Return codes: keep in sync with EPHPM_OPSCAN_* in ephpm_wrapper.c.
    match rc {
        0 => Ok(state.hits),
        -1 => Err(OpcodeScanError::Compile(err_msg())),
        -2 => Err(OpcodeScanError::Fatal(err_msg())),
        _ => Err(OpcodeScanError::NoEngine),
    }
}

/// Stub-mode variant: opcode scanning needs libphp.
///
/// # Errors
///
/// Always [`OpcodeScanError::Unavailable`].
#[cfg(not(php_linked))]
pub fn scan_file(_path: &Path, _sinks: &[&str]) -> Result<Vec<OpcodeHit>, OpcodeScanError> {
    Err(OpcodeScanError::Unavailable)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stub_mode_reports_unavailable() {
        // In a php_linked build this test is about the *other* invariant:
        // scanning without engine context on this thread must fail cleanly,
        // never crash. Both outcomes are the documented degradation path.
        if available() {
            // No PhpRuntime::init() here — must fail with NoEngine.
            if crate::PhpRuntime::is_ready() {
                // Another test initialized PHP in this process; the
                // stub-degradation assertion is meaningless — skip.
                return;
            }
            let err = scan_file(Path::new("nope.php"), &["eval"]).unwrap_err();
            assert!(matches!(err, OpcodeScanError::NoEngine), "{err}");
        } else {
            let err = scan_file(Path::new("nope.php"), &["eval"]).unwrap_err();
            assert!(matches!(err, OpcodeScanError::Unavailable), "{err}");
        }
    }
}
