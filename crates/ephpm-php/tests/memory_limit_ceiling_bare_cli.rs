//! F5 (the other side of the gate) — the **plain operator** CLI (`ephpm php`
//! with no `--site`) stays unbounded, exactly as php-cli is, even when a
//! `memory_limit_max` was configured. The ceiling is enforced only for server
//! requests and for a tenant run (`enable_tenant_mode`).
//!
//! This binary sets `enable_cli_mode()` WITHOUT `enable_tenant_mode()` and a
//! `256M` ceiling, then asserts no ceiling is installed — the
//! `g_cli_mode && !g_tenant_mode` skip is taken. Companion of
//! `memory_limit_ceiling_tenant_cli.rs`.
//!
//! Requires a real libphp link. Run with:
//! `cargo test -p ephpm-php --test memory_limit_ceiling_bare_cli`.

#![cfg(all(test, php_linked))]

mod common_ceiling;

use ephpm_php::PhpRuntime;
use serial_test::serial;

fn init() {
    common_ceiling::init_once(|| {
        PhpRuntime::enable_cli_mode();
        // Deliberately NOT enable_tenant_mode().
        PhpRuntime::set_memory_limit_max(Some("256M")).expect("set ceiling");
    });
}

#[test]
#[serial]
fn the_bare_operator_cli_stays_unbounded() {
    init();
    assert_eq!(
        PhpRuntime::memory_limit_bounds(),
        None,
        "the plain operator `ephpm php` must keep php-cli's unbounded memory semantics \
         (composer et al. need -1); the ceiling applies only to tenant runs"
    );
}
