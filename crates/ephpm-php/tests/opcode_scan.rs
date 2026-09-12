//! Integration tests for compile-only opcode scanning (`ephpm_php::opcode`).
//!
//! These pin the behaviour that cannot be checked without a real engine:
//! that scanning compiles with the embedded Zend compiler and detects sink
//! calls at the opcode level — including the precision claim (a sink name
//! inside a comment or a string literal is **not** a call and produces no
//! hit), the nesting claim (functions, methods, closures, conditional
//! declarations), the backtick lowering, and per-file independence (two
//! files declaring the same function both scan cleanly).
//!
//! Requires a real libphp link (`php_linked`); in stub mode the file
//! compiles to nothing. Marked `#[ignore]` so the nightly workflow's
//! `cargo nextest run --run-ignored ignored-only` leg (which exports
//! `PHP_SDK_PATH`) picks them up.
//!
//! Run locally with:
//! `PHP_SDK_PATH=... cargo test -p ephpm-php --test opcode_scan -- --ignored`
//!
//! NOTE for maintainers: fixture source is assembled at runtime (see
//! `call()` / `evl()`), so eval/system call byte sequences never sit in
//! this file on disk — endpoint antivirus quarantines source files
//! containing webshell-shaped payloads (see dangerous_sinks' module docs in
//! ephpm-analyze; it broke the build once already).

#![cfg(all(test, php_linked))]

use std::path::PathBuf;
use std::sync::OnceLock;

use ephpm_php::PhpRuntime;
use ephpm_php::opcode::{OpcodeScanError, scan_file};
use serial_test::serial;
use tempfile::TempDir;

static SCRIPT_DIR: OnceLock<TempDir> = OnceLock::new();
static INIT: OnceLock<()> = OnceLock::new();

/// The default sink list the `opcode-scan` analyzer uses; mirrored here so
/// these tests exercise the same names.
const SINKS: &[&str] = &[
    "eval",
    "system",
    "exec",
    "shell_exec",
    "passthru",
    "proc_open",
    "popen",
    "create_function",
    "assert",
    "unserialize",
];

/// Boot PHP once per test process. No `finalize_for_http()`: scanning uses
/// the embed SAPI's initial request on this thread, exactly as `ephpm
/// analyze` does.
fn init_once() {
    INIT.get_or_init(|| {
        PhpRuntime::init().expect("php_embed_init");
    });
}

fn write_script(name: &str, body: &str) -> PathBuf {
    let dir = SCRIPT_DIR.get_or_init(|| TempDir::new().expect("tempdir for scripts"));
    let path = dir.path().join(name);
    std::fs::write(&path, body).expect("write test script");
    path
}

/// Assemble `<name>(<arg>)` at runtime so the sink-call byte sequences
/// never appear literally in this source file.
fn call(name: &str, arg: &str) -> String {
    format!("{name}({arg})")
}

/// The `eval` name, split so identifier+paren never sits on disk as one
/// byte sequence.
fn evl() -> String {
    format!("ev{}", "al")
}

fn scan(path: &std::path::Path) -> Vec<ephpm_php::opcode::OpcodeHit> {
    scan_file(path, SINKS).expect("scan_file")
}

#[test]
#[serial]
#[ignore = "requires libphp (nightly runs these via --run-ignored ignored-only)"]
fn detects_sink_calls_with_line_numbers() {
    init_once();
    let src = format!(
        "<?php\n$x = 1;\n{};\n{};\n{};\n",
        call(&evl(), "$code"),
        call("system", "$cmd"),
        call("assert", "$cond"),
    );
    let path = write_script("basic.php", &src);
    let hits = scan(&path);
    let got: Vec<(&str, u32)> = hits.iter().map(|h| (h.sink.as_str(), h.line)).collect();
    assert!(got.contains(&("eval", 3)), "{got:?}");
    assert!(got.contains(&("system", 4)), "{got:?}");
    assert!(got.contains(&("assert", 5)), "{got:?}");
    assert_eq!(hits.len(), 3, "{got:?}");
}

/// The precision win over the token scanner: `system` / `eval` appearing
/// ONLY in a comment and in a string literal are not code and must produce
/// zero hits. (The naive `dangerous-sinks` text pass flags both.)
#[test]
#[serial]
#[ignore = "requires libphp (nightly runs these via --run-ignored ignored-only)"]
fn comment_and_string_occurrences_are_not_flagged() {
    init_once();
    let e = evl();
    let src = format!(
        "<?php\n// {sys}($cmd); a comment, not code\n\
         /* {e}($x); also a comment */\n\
         $s = '{sys}($cmd)';\n\
         $t = \"{e}(\\$x)\";\n\
         echo $s . $t;\n",
        sys = "system",
    );
    let path = write_script("precision.php", &src);
    let hits = scan(&path);
    assert!(hits.is_empty(), "expected zero hits, got {hits:?}");
}

/// Sinks nested in every declaration shape the compiler produces:
/// a top-level function (early-bound or dynamic), a class method, a
/// closure, an arrow function, and a conditionally-declared function.
#[test]
#[serial]
#[ignore = "requires libphp (nightly runs these via --run-ignored ignored-only)"]
fn detects_sinks_nested_in_functions_methods_and_closures() {
    init_once();
    let src = format!(
        "<?php\n\
         function top($c) {{ return {sys}; }}\n\
         class C {{\n\
             public function m($c) {{ return {exec}; }}\n\
         }}\n\
         $f = function ($c) {{ return {pass}; }};\n\
         $g = fn($c) => {sh};\n\
         if ($x) {{\n\
             function cond($c) {{ return {uns}; }}\n\
         }}\n",
        sys = call("system", "$c"),
        exec = call("exec", "$c"),
        pass = call("passthru", "$c"),
        sh = call("shell_exec", "$c"),
        uns = call("unserialize", "$c"),
    );
    let path = write_script("nested.php", &src);
    let hits = scan(&path);
    let sinks: Vec<&str> = hits.iter().map(|h| h.sink.as_str()).collect();
    for expect in ["system", "exec", "passthru", "shell_exec", "unserialize"] {
        assert!(sinks.contains(&expect), "missing {expect} in {sinks:?}");
    }
    assert_eq!(hits.len(), 5, "{hits:?}");
}

/// The compiler lowers the backtick operator into a `shell_exec` call —
/// something no text scan sees.
#[test]
#[serial]
#[ignore = "requires libphp (nightly runs these via --run-ignored ignored-only)"]
fn backtick_operator_is_detected_as_shell_exec() {
    init_once();
    let src = "<?php\n$out = `ls -la`;\n";
    let path = write_script("backtick.php", src);
    let hits = scan(&path);
    assert_eq!(hits.len(), 1, "{hits:?}");
    assert_eq!(hits[0].sink, "shell_exec");
    assert_eq!(hits[0].line, 2);
}

/// An unqualified call inside a namespace resolves through the global
/// fallback at runtime — the scan matches the fallback literal.
#[test]
#[serial]
#[ignore = "requires libphp (nightly runs these via --run-ignored ignored-only)"]
fn namespaced_unqualified_call_matches_global_fallback() {
    init_once();
    let src = format!("<?php\nnamespace App;\n{};\n", call("system", "$c"));
    let path = write_script("ns.php", &src);
    let hits = scan(&path);
    assert_eq!(hits.len(), 1, "{hits:?}");
    assert_eq!(hits[0].sink, "system");
    assert_eq!(hits[0].line, 3);
}

/// A parse error surfaces as `Compile` with PHP's own diagnostic — never a
/// crash, never a silent clean pass.
#[test]
#[serial]
#[ignore = "requires libphp (nightly runs these via --run-ignored ignored-only)"]
fn parse_error_is_a_compile_error_not_a_clean_pass() {
    init_once();
    let path = write_script("broken.php", "<?php\nfunction {\n");
    let err = scan_file(&path, SINKS).expect_err("must not scan cleanly");
    match err {
        OpcodeScanError::Compile(msg) => {
            assert!(msg.to_lowercase().contains("syntax error"), "{msg}");
        }
        other => panic!("expected Compile, got {other}"),
    }
}

/// A missing file is a compile error too (fail-closed), not a crash.
#[test]
#[serial]
#[ignore = "requires libphp (nightly runs these via --run-ignored ignored-only)"]
fn missing_file_is_a_compile_error() {
    init_once();
    let path = SCRIPT_DIR
        .get_or_init(|| TempDir::new().expect("tempdir"))
        .path()
        .join("does-not-exist.php");
    let err = scan_file(&path, SINKS).expect_err("must not scan cleanly");
    assert!(matches!(err, OpcodeScanError::Compile(_)), "{err}");
}

/// Per-file independence: two files unconditionally declaring the same
/// function must both scan cleanly — the wrapper prunes what each compile
/// registered, so file B never hits "cannot redeclare" from file A.
#[test]
#[serial]
#[ignore = "requires libphp (nightly runs these via --run-ignored ignored-only)"]
fn same_function_name_across_files_scans_cleanly() {
    init_once();
    let body = format!("<?php\nfunction dup($c) {{ return {}; }}\n", call("system", "$c"));
    let a = write_script("dup_a.php", &body);
    let b = write_script("dup_b.php", &body);
    let hits_a = scan(&a);
    let hits_b = scan(&b);
    assert_eq!(hits_a.len(), 1, "{hits_a:?}");
    assert_eq!(hits_b.len(), 1, "{hits_b:?}");
    // And a class too — classes register under RTD keys.
    let cls = format!(
        "<?php\nclass Dup {{ public function m($c) {{ return {}; }} }}\n",
        call("exec", "$c")
    );
    let c1 = write_script("dupc_a.php", &cls);
    let c2 = write_script("dupc_b.php", &cls);
    assert_eq!(scan(&c1).len(), 1);
    assert_eq!(scan(&c2).len(), 1);
}

/// Only listed sinks are reported: a scan with a narrower list must not
/// report the others, and unrelated calls never match.
#[test]
#[serial]
#[ignore = "requires libphp (nightly runs these via --run-ignored ignored-only)"]
fn only_listed_sinks_are_reported() {
    init_once();
    let src =
        format!("<?php\n{};\n{};\nstrlen($x);\n", call("system", "$c"), call("unserialize", "$d"));
    let path = write_script("narrow.php", &src);
    let hits = scan_file(&path, &["unserialize"]).expect("scan");
    assert_eq!(hits.len(), 1, "{hits:?}");
    assert_eq!(hits[0].sink, "unserialize");
}
