//! Per-site PHP function capabilities — the request-time gate for functions the
//! multi-tenant hardening preset leaves registered *only because* at least one
//! virtual host was granted them.
//!
//! # Why this exists
//!
//! `disable_functions` is process-global and only takes effect at PHP's MINIT:
//! `zend_disable_functions` **removes** each named function from the internal
//! function table, once, and under ZTS that table is shared across every worker
//! thread. So a function cannot be disabled for one tenant and enabled for
//! another by any per-thread mechanism — the table has no tenant dimension.
//!
//! The workaround is to **not** globally disable a function any site was granted,
//! and instead gate it per request. [`crate::PhpRuntime::set_capability_gates`]
//! hands the C wrapper the list of such functions before `php_embed_init()`; the
//! wrapper swaps each one's internal handler for a generic gate
//! (`ephpm_capability_gate` in `ephpm_wrapper.c`) during module startup. On every
//! call that gate asks [`capability_allowed`] (exported to C as
//! `ephpm_capability_allowed`) whether *this request's site* holds the capability,
//! and either calls through to the real builtin or raises the exact warning a
//! disabled function raises.
//!
//! # The two pieces of state
//!
//! * The **site → capabilities map** ([`install`]) is built once at startup from
//!   the operator-owned per-site overrides and is **immutable** for the process
//!   lifetime. The set of *gated* functions is fixed at MINIT, so the grants that
//!   decide them are fixed too — changing a grant requires a restart, exactly
//!   like changing `disable_functions`. This is a feature, not a limitation: it
//!   keeps the shared-process attack surface from shifting under a running fleet.
//! * The **current site** ([`set_current_site`]) is a per-thread cell the router
//!   sets before each request runs, from the one canonical site key
//!   (`ResolvedSite.key`) — never re-derived from the `Host` header. It is the
//!   same discipline as [`crate::db_bridge::set_current_site`], but a dedicated
//!   cell because the database key is `None` outside per-site-database mode,
//!   whereas a capability grant must work in any multi-tenant deployment.
//!
//! # Fail closed, always
//!
//! Every path that cannot *prove* the current request's site holds the
//! capability denies it: no map installed, no current site, a host that matched
//! no vhost (`None`), a site absent from the map, or a name the site was not
//! granted. The C side only ever swaps a handler for a function that was granted
//! by some site, so a denied call behaves identically to one the hardening preset
//! disabled.
//!
//! # Security posture (honest)
//!
//! This is **cooperative-tenant isolation at the ePHPm layer**, not a kernel
//! boundary. The gate is a handler check inside one shared ZTS process; a
//! memory-safety break in PHP or an extension, or any way to reach the saved
//! original handler directly, bypasses it. It does not add a sandbox. What it
//! *does* guarantee is that a tenant with no grant is exactly as restricted as it
//! is today, and that granting one tenant `mail`/`fsockopen` never makes those
//! callable from another tenant's request.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::sync::OnceLock;

/// The process-wide, immutable site → granted-capabilities map.
///
/// Installed once at startup ([`install`]); `None` until then, which denies
/// every gated call. Keys are canonical site keys; values are canonical
/// (lower-case) capability/function names, matching the names the C gate passes
/// to [`capability_allowed`].
static CAPABILITY_MAP: OnceLock<HashMap<String, HashSet<String>>> = OnceLock::new();

thread_local! {
    /// This thread's current request site — the canonical site key, set by the
    /// router before PHP runs and cleared (`None`) for a host that matched no
    /// vhost. `None` denies every gated call on this thread.
    static CURRENT_SITE: RefCell<Option<Box<str>>> = const { RefCell::new(None) };
}

/// Install the immutable site → capabilities map. Call once at startup, before
/// serving. A second call is ignored (the map is frozen), so the set of grants
/// a running process honours never changes.
///
/// `map` keys are canonical site keys; each value is the set of canonical
/// capability names that site was granted. Only names that are actually *gated*
/// (left registered for per-site use) need appear — a name no site can reach via
/// a gate is harmless to include but does nothing.
// The one caller (the server startup path) builds a std-hasher map; generalizing
// over the hasher would only obscure this install-once API.
#[allow(clippy::implicit_hasher)]
pub fn install(map: HashMap<String, HashSet<String>>) {
    let _ = CAPABILITY_MAP.set(map);
}

/// Set (or clear, with `None`) this thread's current request site. The router
/// calls this before every PHP execution, from the canonical site key, and
/// clears it (`None`) for a host that matched no virtual host so a reused worker
/// thread never carries a previous request's identity into a gated call.
pub fn set_current_site(site_key: Option<&str>) {
    let _ = CURRENT_SITE.try_with(|s| {
        *s.borrow_mut() = site_key.map(Box::from);
    });
}

/// Whether the current request's site holds capability `name`.
///
/// The sole decision point for a gated call. Fails closed on every uncertainty:
/// no map, no current site, an unknown site, or a name the site lacks all return
/// `false`.
#[must_use]
pub fn is_allowed(name: &str) -> bool {
    let Some(map) = CAPABILITY_MAP.get() else {
        return false;
    };
    let lower = name.trim().to_ascii_lowercase();
    CURRENT_SITE
        .try_with(|s| {
            s.borrow()
                .as_deref()
                .and_then(|site| map.get(site))
                .is_some_and(|caps| caps.contains(&lower))
        })
        .unwrap_or(false)
}

/// Capability-check callback handed to the C gate via
/// `ephpm_set_capability_checker` (function-pointer convention, the same as
/// `ephpm_set_log_sink` — never linked by name).
///
/// Returns `1` when the current request's site holds the named capability, `0`
/// otherwise. `name` is the called function's name (canonical, lower-case) as a
/// NUL-terminated C string. A null pointer, non-UTF-8 bytes, or any panic denies
/// the call — this is the fail-closed boundary, and it must never unwind into C.
///
/// # Safety
///
/// `name` must be either null or a valid pointer to a NUL-terminated C string
/// that stays valid for the duration of the call. The C gate passes
/// `EX(func)->common.function_name`'s buffer, which is live for the call.
#[must_use]
pub unsafe extern "C" fn ephpm_capability_allowed(
    name: *const std::os::raw::c_char,
) -> std::os::raw::c_int {
    if name.is_null() {
        return 0;
    }
    // Catch any panic so an unwind never crosses the FFI boundary into PHP's C
    // frame; a panic denies the call, like every other uncertainty here.
    let allowed = std::panic::catch_unwind(|| {
        // SAFETY: the caller guarantees `name` is a valid NUL-terminated string
        // for the duration of the call (see the function's Safety section).
        let cstr = unsafe { std::ffi::CStr::from_ptr(name) };
        match cstr.to_str() {
            Ok(s) => is_allowed(s),
            Err(_) => false,
        }
    })
    .unwrap_or(false);
    std::os::raw::c_int::from(allowed)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `install` is process-global and `OnceLock` is write-once, so the map
    /// tests share one install and must not assume a specific other test's
    /// contents beyond what this one site guarantees.
    fn seed_map() -> &'static HashMap<String, HashSet<String>> {
        install({
            let mut m = HashMap::new();
            m.insert(
                "sendy.example".to_string(),
                ["mail".to_string(), "fsockopen".to_string()].into_iter().collect(),
            );
            m.insert("cake.example".to_string(), HashSet::new());
            m
        });
        CAPABILITY_MAP.get().expect("installed")
    }

    #[test]
    fn granted_site_allows_its_capabilities_and_nothing_else() {
        seed_map();
        set_current_site(Some("sendy.example"));
        assert!(is_allowed("mail"));
        assert!(is_allowed("fsockopen"));
        assert!(is_allowed("MAIL"), "the lookup is case-insensitive on the queried name");
        assert!(!is_allowed("pfsockopen"), "a capability the site was not granted is denied");
        set_current_site(None);
    }

    #[test]
    fn ungranted_site_denies_every_capability() {
        seed_map();
        set_current_site(Some("cake.example"));
        assert!(!is_allowed("mail"));
        assert!(!is_allowed("fsockopen"));
        set_current_site(None);
    }

    #[test]
    fn no_current_site_denies() {
        seed_map();
        set_current_site(None);
        assert!(!is_allowed("mail"), "an unmatched host (None) holds no capabilities");
    }

    #[test]
    fn unknown_site_denies() {
        seed_map();
        set_current_site(Some("stranger.example"));
        assert!(!is_allowed("mail"), "a site absent from the map holds no capabilities");
        set_current_site(None);
    }

    #[test]
    fn null_name_denies() {
        // SAFETY: passing a null pointer is the documented fail-closed path.
        assert_eq!(unsafe { ephpm_capability_allowed(std::ptr::null()) }, 0);
    }
}
