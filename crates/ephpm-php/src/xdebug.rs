//! Startup snapshot of the Xdebug settings that decide whether a request
//! starts a step-debugging session.
//!
//! Worker mode's debug lane (`ephpm-server`'s `debug_lane`) has to decide in
//! the router — before any PHP runs for the request — whether Xdebug's own
//! RINIT check would open a DBGp session for it. That check depends on three
//! settings: `xdebug.mode` (overridable by the `XDEBUG_MODE` environment
//! variable), `xdebug.start_with_request`, and `xdebug.trigger_value` (the
//! optional shared secret a trigger's value must match).
//!
//! Ini values are per-thread under ZTS, so they are read exactly once, on the
//! thread that ran `php_embed_init()` (where the executor globals hold the
//! startup values), and frozen here. The router never touches PHP state.
//! Per-request ini changes (`ini_set`, per-vhost overrides) are not reflected —
//! the debug lane mirrors the server-wide startup configuration.

use std::sync::OnceLock;

/// The raw Xdebug settings as resolved at PHP startup. Interpretation (mode
/// parsing, trigger matching) lives with the consumer, so it can be unit
/// tested without PHP.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct XdebugIni {
    /// The `xdebug.mode` ini value (Xdebug's default is `develop`).
    pub mode: String,
    /// The `XDEBUG_MODE` environment variable at startup, if set. Xdebug
    /// prefers it over `xdebug.mode` when it parses as a valid mode list.
    pub mode_env: Option<String>,
    /// The `xdebug.start_with_request` ini value (`default`, `yes`, `no`,
    /// `trigger`).
    pub start_with_request: String,
    /// The `xdebug.trigger_value` ini value — empty when no shared secret is
    /// configured, otherwise one value or a comma-separated list.
    pub trigger_value: String,
    /// The `xdebug.discover_client_host` ini value (`"1"` when on). With it
    /// on, Xdebug connects back to whichever client sent the trigger — so
    /// anyone who can trigger a session can attach a debugger.
    pub discover_client_host: String,
}

/// `None` inside the cell = Xdebug was not loaded at startup.
static SNAPSHOT: OnceLock<Option<XdebugIni>> = OnceLock::new();

/// The Xdebug settings captured at startup, or `None` when the xdebug
/// extension is not loaded (always `None` in stub mode).
#[must_use]
pub fn settings() -> Option<XdebugIni> {
    SNAPSHOT.get().and_then(Clone::clone)
}

/// Capture the snapshot. Must run on the thread that called
/// `php_embed_init()`, right after it — see the module docs.
#[cfg(php_linked)]
pub(crate) fn capture() {
    use std::ffi::{CStr, CString};

    // SAFETY: the literal is a static NUL-terminated string; the C helper
    // only reads the process-global module_registry, which php_embed_init()
    // has finished populating.
    let loaded = unsafe { crate::ffi::ephpm_extension_loaded(c"xdebug".as_ptr()) != 0 };
    if !loaded {
        let _ = SNAPSHOT.set(None);
        return;
    }

    let ini = |key: &str| -> String {
        let Ok(c_key) = CString::new(key) else {
            return String::new();
        };
        // SAFETY: called on the php_embed_init() thread, whose TSRM context
        // and executor globals (EG(ini_directives)) are live. The C helper is
        // a plain hash lookup (no bailout path). The returned pointer borrows
        // PHP-owned memory and is copied into a Rust String before any other
        // PHP call; NULL means the directive is not registered.
        unsafe {
            let ptr = crate::ffi::ephpm_ini_string(c_key.as_ptr());
            if ptr.is_null() {
                String::new()
            } else {
                CStr::from_ptr(ptr).to_string_lossy().into_owned()
            }
        }
    };

    let snapshot = XdebugIni {
        mode: ini("xdebug.mode"),
        mode_env: std::env::var("XDEBUG_MODE").ok(),
        start_with_request: ini("xdebug.start_with_request"),
        trigger_value: ini("xdebug.trigger_value"),
        discover_client_host: ini("xdebug.discover_client_host"),
    };
    tracing::debug!(?snapshot, "xdebug extension loaded — captured trigger settings");
    let _ = SNAPSHOT.set(Some(snapshot));
}
