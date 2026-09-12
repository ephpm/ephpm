//! Build script — only exists for one Windows test-linking flag.
//!
//! With the `php` feature on a PHP-linked Windows build, this crate's test
//! binaries link the static PHP SDK through ephpm-php, which carries one
//! known byte-identical duplicate symbol (`locale_charset`, bundled by both
//! whole-archived GNU libs — see `link_windows_static_deps` in
//! `crates/ephpm-php/build.rs`). The `ephpm` binary crate passes
//! `/FORCE:MULTIPLE` for the final exe and ephpm-php passes it for its own
//! test targets, but `rustc-link-arg-tests` does not propagate to
//! downstream crates — so this crate's tests need their own. Harmless when
//! libphp is absent (stub builds pull no SDK libs into the link, so the
//! flag never fires).

fn main() {
    println!("cargo::rerun-if-changed=build.rs");
    let target_windows = std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows");
    let php_feature = std::env::var_os("CARGO_FEATURE_PHP").is_some();
    if target_windows && php_feature {
        println!("cargo::rustc-link-arg-tests=/FORCE:MULTIPLE");
    }
}
