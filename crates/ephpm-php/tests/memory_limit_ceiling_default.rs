//! With no `[php] memory_limit_max` the ceiling is the `memory_limit` PHP
//! started with (`128M` here): scripts can lower it and raise it back, but not
//! above where the operator put it.
//!
//! Requires a real libphp link. Run with:
//! `cargo test -p ephpm-php --test memory_limit_ceiling_default`.

#![cfg(all(test, php_linked))]

mod common_ceiling;

use common_ceiling::{run, text};
use ephpm_php::{MemoryLimitBounds, PhpRuntime};
use serial_test::serial;

const MIB: u64 = 1024 * 1024;

#[test]
#[serial]
fn the_default_ceiling_is_the_startup_limit() {
    common_ceiling::init_once(|| {});
    assert_eq!(
        PhpRuntime::memory_limit_bounds(),
        Some(MemoryLimitBounds { startup_bytes: 128 * MIB, ceiling_bytes: 128 * MIB })
    );
    let body = "<?php $r = [var_export(ini_set('memory_limit', '-1'), true), \
                ini_get('memory_limit'), \
                var_export(ini_set('memory_limit', '256M'), true), ini_get('memory_limit'), \
                var_export(ini_set('memory_limit', '32M'), true), ini_get('memory_limit'), \
                var_export(ini_set('memory_limit', '128M'), true), ini_get('memory_limit')]; \
                echo implode('|', $r);";
    let out = text(&run("default.php", body.to_string(), &[]).expect("probe"));
    assert_eq!(out, "false|128M|false|128M|'128M'|32M|'32M'|128M");
}
