//! `[php] memory_limit_max`: scripts may lower `memory_limit` and raise it
//! again up to the ceiling, but never above it.
//!
//! `memory_limit` is `PHP_INI_ALL` upstream and PHP's hard limit
//! (`memory_hard_limit_percent`) is derived from it, so without a ceiling a
//! tenant's `ini_set('memory_limit', '-1')` lifts both. ePHPm wraps the entry's
//! `on_modify` handler at module startup; a refused raise makes `ini_set()`
//! return `false` and leaves the value unchanged. The per-request INI channel
//! (`set_request_ini`, `PHP_INI_STAGE_ACTIVATE`) is bounded the same way.
//!
//! This binary sets an explicit ceiling of `256M` over PHP's startup
//! `memory_limit` of `128M`; `memory_limit_ceiling_default.rs` covers the
//! default ceiling (the startup value). Each test binary is its own process,
//! because the ceiling is fixed at module startup.
//!
//! Requires a real libphp link. Run with:
//! `cargo test -p ephpm-php --test memory_limit_ceiling`.

#![cfg(all(test, php_linked))]

mod common_ceiling;

use common_ceiling::{run, text};
use ephpm_php::{MemoryLimitBounds, PhpError, PhpRuntime};
use serial_test::serial;

const MIB: u64 = 1024 * 1024;

fn init() {
    common_ceiling::init_once(|| {
        PhpRuntime::set_memory_limit_max(Some("256M")).expect("set ceiling");
    });
}

/// `ini_set($v)`'s return, then `ini_get` afterwards, as `ret|after`.
fn probe_set(name: &'static str, value: &str) -> String {
    init();
    let body = format!(
        "<?php $r = ini_set('memory_limit', '{value}'); \
         echo var_export($r, true), '|', ini_get('memory_limit');"
    );
    text(&run(name, body, &[]).expect("probe request"))
}

#[test]
#[serial]
fn the_engine_reports_the_configured_bounds() {
    init();
    assert_eq!(
        PhpRuntime::memory_limit_bounds(),
        Some(MemoryLimitBounds { startup_bytes: 128 * MIB, ceiling_bytes: 256 * MIB })
    );
}

#[test]
#[serial]
fn unlimited_is_refused() {
    assert_eq!(probe_set("neg1.php", "-1"), "false|128M");
}

#[test]
#[serial]
fn a_raise_above_the_ceiling_is_refused() {
    assert_eq!(probe_set("above.php", "512M"), "false|128M");
    assert_eq!(probe_set("above_bytes.php", "268435457"), "false|128M");
    assert_eq!(probe_set("above_g.php", "1G"), "false|128M");
}

#[test]
#[serial]
fn a_raise_to_the_ceiling_is_allowed() {
    assert_eq!(probe_set("at.php", "256M"), "'128M'|256M");
}

#[test]
#[serial]
fn lowering_and_raising_back_under_the_ceiling_works() {
    init();
    let body = "<?php $a = ini_set('memory_limit', '64M'); $b = ini_get('memory_limit'); \
                $c = ini_set('memory_limit', '200M'); $d = ini_get('memory_limit'); \
                $e = ini_set('memory_limit', '-1'); $f = ini_get('memory_limit'); \
                ini_restore('memory_limit'); \
                echo implode('|', [var_export($a, true), $b, var_export($c, true), $d, \
                                   var_export($e, true), $f, ini_get('memory_limit')]);";
    let out = text(&run("lower_raise.php", body.to_string(), &[]).expect("probe"));
    assert_eq!(out, "'128M'|64M|'64M'|200M|false|200M|128M");
}

#[test]
#[serial]
fn the_next_request_starts_from_the_startup_limit() {
    // A raise inside one request must not leak into the next on the thread.
    init();
    let _ = probe_set("leak_a.php", "256M");
    let out = text(
        &run("leak_b.php", "<?php echo ini_get('memory_limit');".to_string(), &[]).expect("probe"),
    );
    assert_eq!(out, "128M");
}

#[test]
#[serial]
fn the_refused_raise_does_not_lift_the_limit() {
    // After the refused -1 the 128M limit still holds: filling 200 MB in 1 MB
    // strings fails with PHP's out-of-memory fatal instead of succeeding.
    init();
    let body = "<?php ini_set('memory_limit', '-1'); $a = []; \
                for ($i = 0; $i < 200; $i++) { $a[] = str_repeat('x', 1 << 20); } \
                echo 'allocated ', count($a);";
    let out = run("fill.php", body.to_string(), &[]);
    assert!(
        matches!(out, Err(PhpError::Bailout(_))),
        "expected an out-of-memory fatal, got {out:?}"
    );
    // And the engine keeps serving.
    let ok = run("after_fill.php", "<?php echo 'ok';".to_string(), &[]).expect("next request");
    assert_eq!(text(&ok), "ok");
}

#[test]
#[serial]
fn the_per_request_channel_is_bounded_too() {
    init();
    let probe = "<?php echo ini_get('memory_limit');";
    let above = run("chan_above.php", probe.to_string(), &[("memory_limit", "1G")]).expect("probe");
    assert_eq!(text(&above), "128M", "a per-request value above the ceiling must not apply");
    let neg = run("chan_neg.php", probe.to_string(), &[("memory_limit", "-1")]).expect("probe");
    assert_eq!(text(&neg), "128M");
    let under =
        run("chan_under.php", probe.to_string(), &[("memory_limit", "200M")]).expect("probe");
    assert_eq!(text(&under), "200M", "a per-request value under the ceiling still applies");
}
