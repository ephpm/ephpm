//! F5 — a **tenant** CLI run (`ephpm php --site`, including every per-site cron
//! child) enforces the `memory_limit` ceiling, so a tenant cron script cannot
//! `ini_set('memory_limit', '-1')` and exhaust host memory.
//!
//! This binary simulates that process shape: `enable_cli_mode()` +
//! `enable_tenant_mode()` + a `256M` ceiling, all before engine startup. The
//! companion `memory_limit_ceiling_bare_cli.rs` proves the plain operator CLI
//! (no tenant mode) stays unbounded — the two together pin the
//! `g_cli_mode && !g_tenant_mode` gate in the wrapper.
//!
//! Requires a real libphp link. Run with:
//! `cargo test -p ephpm-php --test memory_limit_ceiling_tenant_cli`.

#![cfg(all(test, php_linked))]

mod common_ceiling;

use ephpm_php::PhpRuntime;
use serial_test::serial;

const MIB: u64 = 1024 * 1024;

fn init() {
    common_ceiling::init_once(|| {
        PhpRuntime::enable_cli_mode();
        PhpRuntime::enable_tenant_mode();
        PhpRuntime::set_memory_limit_max(Some("256M")).expect("set ceiling");
    });
}

#[test]
#[serial]
fn tenant_cli_enforces_the_configured_ceiling() {
    init();
    // Unlike the plain operator CLI, a tenant run installs the ceiling: the
    // bounds are reported and active (the `256M` the operator configured).
    let bounds = PhpRuntime::memory_limit_bounds()
        .expect("a tenant CLI run must enforce the memory_limit ceiling (finding F5)");
    assert_eq!(
        bounds.ceiling_bytes,
        256 * MIB,
        "the ceiling must be the configured [php] memory_limit_max"
    );
}
