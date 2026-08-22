//! `ephpm php` CLI conformance — fatal-error reporting, exit statuses,
//! startup-time `-d` (OPcache/JIT activation, issue #331), the cli-SAPI
//! process-title functions (issue #316), and the end-of-request lifecycle
//! (shutdown functions / destructors / exit-status-from-shutdown, issue #334).
//!
//! Regression cover for **issue #321**: on v0.7.0 a fatal error or an uncaught
//! exception under `ephpm php -r` produced **no output at all and exit 0**,
//! where php-cli prints a diagnostic and exits 255. Two separate observable
//! defects — the silence and the status — so both are asserted independently
//! here. Asserting only the exit code would let the silence come back.
//!
//! Root cause, for the record: `-r` evaluated the snippet with a plain
//! `zend_eval_string`, which leaves an uncaught exception *pending* rather than
//! reporting it. In PHP 8 almost every CLI-visible fatal arrives as a thrown
//! object — `Call to undefined function` throws `Error`, a compile failure
//! inside `eval` throws `ParseError` — so nothing ever reached
//! `php_error_cb`, nothing was displayed, and `EG(exit_status)` was never set
//! to 255. It is *not* an ini problem: `display_errors` was already `1`, which
//! is why non-fatal diagnostics (warnings) printed correctly the whole time.
//! The fix is `zend_eval_string_ex(..., handle_exceptions = 1)` plus taking
//! `EG(exit_status)` unconditionally (see `cli_eval_protected` and
//! `cli_execute_script_protected` in `crates/ephpm-php/ephpm_wrapper.c`).
//!
//! This suite needs no HTTP server — it execs the release binary directly —
//! so `cargo xtask e2e` runs it without spawning a node (see `NO_NODE_SUITES`
//! in `xtask/src/e2e_bare.rs`) and hands it the binary path in
//! `EPHPM_CLI_BINARY`. The suite self-skips when that is unset, so the file
//! still compiles and links on a checkout with no release build.
//!
//! These assertions only mean anything against a PHP-linked binary: the whole
//! CLI lives behind `#[cfg(php_linked)]`, so a stub-mode unit test could not
//! reach it.
//!
//! Environment variables:
//! - `EPHPM_CLI_BINARY` — path to a PHP-linked `ephpm` binary

use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

fn cli_binary() -> Option<PathBuf> {
    std::env::var_os("EPHPM_CLI_BINARY").map(PathBuf::from).filter(|p| !p.as_os_str().is_empty())
}

/// What one `ephpm php …` invocation produced.
struct Run {
    stdout: String,
    stderr: String,
    code: i32,
}

impl Run {
    /// stdout and stderr concatenated — for assertions that only care that the
    /// diagnostic was emitted *somewhere*, independent of `display_errors`'
    /// stdout-vs-stderr routing.
    fn output(&self) -> String {
        format!("{}{}", self.stdout, self.stderr)
    }
}

/// Run `ephpm php <args…>` with `stdin_data` on stdin.
fn run_php(bin: &PathBuf, args: &[&str], stdin_data: &str) -> Run {
    let mut child = Command::new(bin)
        .arg("php")
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap_or_else(|e| panic!("spawn {} php {args:?}: {e}", bin.display()));

    child
        .stdin
        .take()
        .expect("stdin piped")
        .write_all(stdin_data.as_bytes())
        .unwrap_or_else(|e| panic!("write stdin for {args:?}: {e}"));

    let out = child.wait_with_output().unwrap_or_else(|e| panic!("wait for {args:?}: {e}"));
    Run {
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        // A signal death has no code; -1 is never a legitimate PHP exit status,
        // so it fails every assertion below rather than silently passing.
        code: out.status.code().unwrap_or(-1),
    }
}

/// Run `ephpm php <args…>` with extra environment variables and empty stdin.
/// The parent environment is inherited (PATH and the loader's variables are
/// needed to start the process at all); `env` is layered on top.
fn run_php_env(bin: &PathBuf, args: &[&str], env: &[(&str, &str)]) -> Run {
    let mut cmd = Command::new(bin);
    cmd.arg("php").args(args).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
    for (k, v) in env {
        cmd.env(k, v);
    }
    let mut child =
        cmd.spawn().unwrap_or_else(|e| panic!("spawn {} php {args:?}: {e}", bin.display()));
    drop(child.stdin.take());

    let out = child.wait_with_output().unwrap_or_else(|e| panic!("wait for {args:?}: {e}"));
    Run {
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        code: out.status.code().unwrap_or(-1),
    }
}

/// Assert the run produced a fatal diagnostic containing `needle` AND exited
/// 255 — the two halves of #321, checked separately so a regression in either
/// one is named precisely.
fn assert_fatal(run: &Run, needle: &str, ctx: &str) {
    let combined = run.output();
    assert!(
        !combined.trim().is_empty(),
        "{ctx}: no diagnostic at all (issue #321 regression — the fatal was \
         swallowed). exit={}",
        run.code
    );
    assert!(
        combined.contains(needle),
        "{ctx}: diagnostic did not mention {needle:?}\n--- stdout ---\n{}\n--- stderr ---\n{}",
        run.stdout,
        run.stderr
    );
    assert_eq!(
        run.code, 255,
        "{ctx}: expected php-cli's fatal exit status 255, got {}\n--- output ---\n{combined}",
        run.code
    );
}

/// The control case. If this breaks, the binary or the harness is wrong and
/// every other failure in this file is noise.
#[test]
fn plain_code_runs_and_exits_zero() {
    let Some(bin) = cli_binary() else {
        eprintln!("EPHPM_CLI_BINARY unset — skipping ephpm php CLI tests");
        return;
    };
    let run = run_php(&bin, &["-r", "echo 22+20;"], "");
    assert_eq!(run.stdout, "42", "unexpected stdout (stderr: {})", run.stderr);
    assert_eq!(run.code, 0, "expected exit 0, got {}", run.code);
}

/// `exit(3)` propagates. This half already worked on v0.7.0 and is kept so a
/// fix for the fatal path can't regress the ordinary one.
#[test]
fn explicit_exit_status_propagates() {
    let Some(bin) = cli_binary() else {
        return;
    };
    let run = run_php(&bin, &["-r", "exit(3);"], "");
    assert_eq!(run.code, 3, "expected exit 3, got {} (output: {})", run.code, run.output());
}

/// #321 case 1: an uncaught exception under `-r`. On v0.7.0: silent, exit 0.
#[test]
fn uncaught_exception_reports_and_exits_255() {
    let Some(bin) = cli_binary() else {
        return;
    };
    let run = run_php(&bin, &["-r", "throw new Exception(\"boom\");"], "");
    assert_fatal(&run, "Uncaught Exception: boom", "-r uncaught exception");
    // php-cli labels `-r` code "Command line code"; the message is worthless
    // for debugging without the location.
    assert!(
        run.output().contains("Command line code"),
        "-r fatal is missing php-cli's \"Command line code\" label:\n{}",
        run.output()
    );
    assert!(
        run.output().contains("Stack trace"),
        "-r fatal is missing the stack trace:\n{}",
        run.output()
    );
}

/// #321 case 2: calling an undefined function. In PHP 8 this throws `Error`,
/// so it took the same silent path as an explicit `throw`.
#[test]
fn undefined_function_reports_and_exits_255() {
    let Some(bin) = cli_binary() else {
        return;
    };
    let run = run_php(&bin, &["-r", "nosuchfunc();"], "");
    assert_fatal(&run, "Call to undefined function nosuchfunc()", "-r undefined function");
}

/// #321 case 3: a parse error in `-r` code. Inside `eval` a compile failure is
/// a thrown `ParseError`, which is why it vanished too.
#[test]
fn parse_error_reports_and_exits_255() {
    let Some(bin) = cli_binary() else {
        return;
    };
    let run = run_php(&bin, &["-r", "this is not php <<<"], "");
    assert_fatal(&run, "Parse error", "-r parse error");
}

/// The silence was never an ini default — `display_errors` was already on.
/// Pin that: the diagnostic must appear with the shipped defaults AND under
/// `-n` (no ini file at all), so nobody "fixes" a future regression by
/// changing a default instead of the plumbing.
#[test]
fn fatal_is_reported_with_default_ini_and_with_no_ini() {
    let Some(bin) = cli_binary() else {
        return;
    };
    let defaults = run_php(&bin, &["-r", "echo ini_get(\"display_errors\");"], "");
    assert_eq!(
        defaults.stdout, "1",
        "display_errors is expected to already be on by default; if this \
         changed, the #321 assertions below need rethinking"
    );

    let no_ini = run_php(&bin, &["-n", "-r", "nosuchfunc();"], "");
    assert_fatal(&no_ini, "Call to undefined function", "-n -r undefined function");
}

/// A named script file. v0.7.0 *did* print this diagnostic (php_execute_script
/// takes a different path than eval) but still exited 0 — the exit-status half
/// of #321 on its own.
#[test]
fn script_file_fatal_reports_and_exits_255() {
    let Some(bin) = cli_binary() else {
        return;
    };
    let dir = tempfile::tempdir().expect("tempdir");
    let script = dir.path().join("fatal.php");
    std::fs::write(&script, "<?php\nnosuchfunc();\n").expect("write script");

    let run = run_php(&bin, &[script.to_str().expect("utf8 path")], "");
    assert_fatal(&run, "Call to undefined function nosuchfunc()", "script file fatal");
    assert!(
        run.output().contains("fatal.php"),
        "script-file fatal should name the script:\n{}",
        run.output()
    );
}

/// A program piped in on stdin. php-cli calls it "Standard input code"; the
/// fatal must be reported and exit 255 there too.
#[test]
fn stdin_program_fatal_reports_and_exits_255() {
    let Some(bin) = cli_binary() else {
        return;
    };
    let run = run_php(&bin, &[], "<?php nosuchfunc();\n");
    assert_fatal(&run, "Call to undefined function nosuchfunc()", "stdin program fatal");
    assert!(
        run.output().contains("Standard input code"),
        "stdin fatal should use php-cli's \"Standard input code\" label:\n{}",
        run.output()
    );
}

// ─── Issue #331: `-d` must apply at module startup (OPcache / JIT) ────────
//
// The observable defect: `-d opcache.enable_cli=1` made `ini_get()` report 1
// while `opcache_get_status()` stayed `false`, because OPcache decides once —
// in its MINIT-time startup hook — whether it will ever activate, and `-d`
// used to be applied after init. The JIT (which lives in OPcache SHM) was
// silently dead the same way. These assert the *activation*, not the ini
// value, so the old failure mode cannot pass.

/// `-d opcache.enable_cli=1` must actually activate OPcache, as in php-cli.
#[test]
fn opcache_activates_with_enable_cli() {
    let Some(bin) = cli_binary() else {
        return;
    };
    let run = run_php(
        &bin,
        &[
            "-d",
            "opcache.enable_cli=1",
            "-r",
            // `?? false` so an inactive opcache (status === false) prints
            // bool(false) instead of a bool-offset warning.
            "var_dump(opcache_get_status(false)['opcache_enabled'] ?? false);",
        ],
        "",
    );
    assert_eq!(run.code, 0, "unexpected exit {} (output: {})", run.code, run.output());
    assert_eq!(
        run.stdout, "bool(true)\n",
        "OPcache did not activate under -d opcache.enable_cli=1 (issue #331)\n\
         --- stdout ---\n{}\n--- stderr ---\n{}",
        run.stdout, run.stderr
    );
}

/// Without the flag OPcache must stay inactive — the php-cli default. Pins
/// that the fix didn't force opcache on unconditionally.
#[test]
fn opcache_stays_inactive_without_enable_cli() {
    let Some(bin) = cli_binary() else {
        return;
    };
    let run = run_php(&bin, &["-r", "var_dump(opcache_get_status(false));"], "");
    assert_eq!(run.code, 0, "unexpected exit {} (output: {})", run.code, run.output());
    assert_eq!(
        run.stdout, "bool(false)\n",
        "OPcache should be inactive without opcache.enable_cli, like php-cli\n\
         --- stdout ---\n{}\n--- stderr ---\n{}",
        run.stdout, run.stderr
    );
}

/// The JIT flags must reach OPcache's startup: `jit.on === true` and a
/// non-zero buffer. This is the "CLI benchmarks vs real php are unfair" half
/// of #331 — before the fix these flags silently no-oped.
#[test]
fn jit_activates_with_tracing_flags() {
    let Some(bin) = cli_binary() else {
        return;
    };
    let run = run_php(
        &bin,
        &[
            "-d",
            "opcache.enable_cli=1",
            "-d",
            "opcache.jit=tracing",
            "-d",
            "opcache.jit_buffer_size=64M",
            "-r",
            "$s = opcache_get_status(false); \
             var_dump($s['jit']['on'] ?? false, ($s['jit']['buffer_size'] ?? 0) > 0);",
        ],
        "",
    );
    assert_eq!(run.code, 0, "unexpected exit {} (output: {})", run.code, run.output());
    assert_eq!(
        run.stdout,
        "bool(true)\nbool(true)\n",
        "JIT did not come up under -d opcache.jit=tracing (issue #331)\n\
         --- stdout ---\n{}\n--- stderr ---\n{}",
        run.stdout, run.stderr
    );
}

/// `-d` must override the embed SAPI's HARDCODED_INI defaults (the merge
/// order pin: defines are spliced *after* the hardcoded block). The embed
/// hardcodes max_execution_time=0, so a `-d` for it only wins if the order
/// is right.
#[test]
fn ini_define_overrides_embed_hardcoded_defaults() {
    let Some(bin) = cli_binary() else {
        return;
    };
    let run = run_php(
        &bin,
        &["-d", "max_execution_time=17", "-r", "var_dump(ini_get('max_execution_time'));"],
        "",
    );
    assert_eq!(
        run.stdout,
        "string(2) \"17\"\n",
        "-d must beat the embed SAPI's hardcoded ini defaults\n--- output ---\n{}",
        run.output()
    );
    assert_eq!(run.code, 0);
}

/// A value starting with a non-alphanumeric goes through php-cli's
/// quote-wrapping path (php_ini_builder_define); a path value is the
/// canonical case.
#[test]
fn ini_define_quotes_non_alnum_values_like_php_cli() {
    let Some(bin) = cli_binary() else {
        return;
    };
    let run =
        run_php(&bin, &["-d", "include_path=/e2e/one", "-r", "echo ini_get('include_path');"], "");
    assert_eq!(
        run.stdout,
        "/e2e/one",
        "quoted -d value did not round-trip\n--- output ---\n{}",
        run.output()
    );
    assert_eq!(run.code, 0);
}

// ─── Issue #316: cli_set_process_title / cli_get_process_title ────────────

/// The functions must exist under `ephpm php` on every platform — the `cli`
/// SAPI identity promises them (PsySH calls cli_set_process_title, so
/// `artisan tinker <file>` fataled while they were missing).
#[test]
fn process_title_functions_exist() {
    let Some(bin) = cli_binary() else {
        return;
    };
    let run = run_php(
        &bin,
        &[
            "-r",
            "var_dump(function_exists('cli_set_process_title'), \
             function_exists('cli_get_process_title'));",
        ],
        "",
    );
    assert_eq!(
        run.stdout,
        "bool(true)\nbool(true)\n",
        "cli process-title functions missing (issue #316)\n--- output ---\n{}",
        run.output()
    );
    assert_eq!(run.code, 0);
}

/// On Linux the title must genuinely change: set → true, get round-trips,
/// and /proc/self/cmdline (what `ps` shows) begins with the new title —
/// php-src's PS_USE_CLOBBER_ARGV behavior, not a stored-string fake.
#[cfg(target_os = "linux")]
#[test]
fn process_title_round_trips_and_reaches_proc_cmdline() {
    let Some(bin) = cli_binary() else {
        return;
    };
    let run = run_php(
        &bin,
        &[
            "-r",
            "$t = 'ephpm-e2e-title-316'; \
             var_dump(cli_set_process_title($t)); \
             var_dump(cli_get_process_title() === $t); \
             var_dump(strpos(file_get_contents('/proc/self/cmdline'), $t) === 0);",
        ],
        "",
    );
    assert_eq!(
        run.stdout,
        "bool(true)\nbool(true)\nbool(true)\n",
        "process title did not set / round-trip / reach /proc/self/cmdline\n\
         --- stdout ---\n{}\n--- stderr ---\n{}",
        run.stdout, run.stderr
    );
    assert_eq!(run.code, 0);
}

// ─── Issue #334: shutdown functions and end-of-script destructors ─────────
//
// The observable defect: `ephpm php` never ran php_request_shutdown() while
// its stdout writer was installed — the request was left for
// php_embed_shutdown() at process exit, after the exit code was captured and
// the CLI output path was torn down. So register_shutdown_function()
// callbacks and destructors of objects alive at script end ran invisibly
// (output into a discarded buffer), and exit() inside a shutdown function
// could not set the exit status. Every expectation below is verified
// byte-identical against real php-cli (8.5.4 and 8.3).

/// php-cli's end-of-request order, in one script: shutdown functions in
/// registration order (including one registered *during* shutdown), THEN
/// destructors of objects still alive at script end — with every byte
/// reaching stdout. Asserted as one exact stdout string so an ordering
/// regression (destructors before shutdown functions, missing nested
/// registration, dropped output) cannot pass.
#[test]
fn shutdown_functions_then_destructors_run_in_php_cli_order() {
    let Some(bin) = cli_binary() else {
        return;
    };
    let run = run_php(
        &bin,
        &[],
        "<?php\n\
         class D { public function __construct(private string $n) {}\n\
                   public function __destruct() { echo \"destruct {$this->n}\\n\"; } }\n\
         register_shutdown_function(function () { echo \"shutdown 1\\n\"; });\n\
         register_shutdown_function(function () {\n\
             echo \"shutdown 2\\n\";\n\
             register_shutdown_function(function () { echo \"shutdown nested\\n\"; });\n\
         });\n\
         $global = new D('global');\n\
         $a = new D('a');\n\
         $b = new D('b');\n\
         unset($a);\n\
         echo \"end of script\\n\";\n",
    );
    assert_eq!(
        run.stdout,
        "destruct a\nend of script\nshutdown 1\nshutdown 2\nshutdown nested\n\
         destruct b\ndestruct global\n",
        "shutdown/destructor order diverges from php-cli (issue #334)\n\
         --- stderr ---\n{}",
        run.stderr
    );
    assert_eq!(run.code, 0, "unexpected exit {}", run.code);
}

/// `exit(7)` inside a shutdown function must both print (output during
/// shutdown reaches stdout) and set the process exit status — the second
/// observable half of #334.
#[test]
fn exit_in_shutdown_function_sets_exit_status() {
    let Some(bin) = cli_binary() else {
        return;
    };
    let run = run_php(
        &bin,
        &[],
        "<?php\n\
         register_shutdown_function(function () { echo \"in shutdown\\n\"; exit(7); });\n\
         echo \"main\\n\";\n",
    );
    assert_eq!(run.stdout, "main\nin shutdown\n", "stderr: {}", run.stderr);
    assert_eq!(run.code, 7, "exit() in a shutdown function must set the exit status");
}

/// The status read after request shutdown wins over the script's own exit()
/// — php-cli's do_cli returns EG(exit_status) *after* php_request_shutdown.
/// Both directions verified against real php-cli: exit(1)→exit(7) exits 7,
/// and exit(3)→exit(0) exits 0 (a shutdown function can clear the status).
#[test]
fn exit_in_shutdown_overrides_script_exit_status() {
    let Some(bin) = cli_binary() else {
        return;
    };
    let seven = run_php(
        &bin,
        &[],
        "<?php\n\
         register_shutdown_function(function () { echo \"in shutdown\\n\"; exit(7); });\n\
         echo \"main\\n\";\nexit(1);\n",
    );
    assert_eq!(seven.stdout, "main\nin shutdown\n", "stderr: {}", seven.stderr);
    assert_eq!(seven.code, 7, "shutdown exit(7) must override the script's exit(1)");

    let zero = run_php(
        &bin,
        &[],
        "<?php\n\
         register_shutdown_function(function () { echo \"s1\\n\"; exit(0); });\n\
         echo \"main\\n\";\nexit(3);\n",
    );
    assert_eq!(zero.stdout, "main\ns1\n", "stderr: {}", zero.stderr);
    assert_eq!(zero.code, 0, "shutdown exit(0) must clear the script's exit(3), like php-cli");
}

/// After an uncaught exception php-cli still runs shutdown functions and
/// destructors (WordPress' fatal handler and most loggers rely on this),
/// and the exit status stays 255.
#[test]
fn shutdown_and_destructors_run_after_uncaught_exception() {
    let Some(bin) = cli_binary() else {
        return;
    };
    let run = run_php(
        &bin,
        &[],
        "<?php\n\
         register_shutdown_function(function () { echo \"shutdown after exception\\n\"; });\n\
         class D { public function __destruct() { echo \"destruct\\n\"; } }\n\
         $d = new D();\n\
         throw new RuntimeException('boom');\n",
    );
    assert_fatal(&run, "Uncaught RuntimeException: boom", "uncaught exception before shutdown");
    assert!(
        run.stdout.contains("shutdown after exception"),
        "shutdown function did not run after the uncaught exception:\n--- stdout ---\n{}\n\
         --- stderr ---\n{}",
        run.stdout,
        run.stderr
    );
    assert!(
        run.stdout.contains("destruct"),
        "destructor did not run after the uncaught exception:\n--- stdout ---\n{}",
        run.stdout
    );
}

/// A fatal *inside* a shutdown function: the diagnostic is reported, the
/// remaining shutdown functions are skipped, destructors still run, and the
/// exit status is 255 — verified identical on real php-cli 8.5 and 8.3.
#[test]
fn fatal_inside_shutdown_function_matches_php_cli() {
    let Some(bin) = cli_binary() else {
        return;
    };
    let run = run_php(
        &bin,
        &[],
        "<?php\n\
         register_shutdown_function(function () { echo \"sf1\\n\"; nosuchfunc(); });\n\
         register_shutdown_function(function () { echo \"sf2\\n\"; });\n\
         class D { public function __destruct() { echo \"destruct\\n\"; } }\n\
         $d = new D();\n\
         echo \"main\\n\";\n",
    );
    assert_fatal(&run, "Call to undefined function nosuchfunc()", "fatal in shutdown function");
    assert!(run.stdout.contains("sf1"), "first shutdown function did not run:\n{}", run.stdout);
    assert!(
        !run.output().contains("sf2"),
        "php-cli skips the remaining shutdown functions after a fatal in one:\n{}",
        run.output()
    );
    assert!(
        run.stdout.contains("destruct"),
        "destructors must still run after a fatal in a shutdown function:\n{}",
        run.stdout
    );
}

/// `-r` code goes through a different execute path (zend_eval_string_ex, not
/// php_execute_script); its shutdown functions must fire too.
#[test]
fn shutdown_functions_run_for_r_code() {
    let Some(bin) = cli_binary() else {
        return;
    };
    let run = run_php(
        &bin,
        &["-r", "register_shutdown_function(function () { echo 'SD'; }); echo 'M';"],
        "",
    );
    assert_eq!(run.stdout, "MSD", "-r shutdown function output (stderr: {})", run.stderr);
    assert_eq!(run.code, 0);
}

/// Non-fatal diagnostics were never broken — warnings printed fine on v0.7.0,
/// which is the evidence that ruled out `display_errors`. Keep that asymmetry
/// pinned so the two paths can't silently swap places.
#[test]
fn warning_is_reported_and_execution_continues() {
    let Some(bin) = cli_binary() else {
        return;
    };
    let run = run_php(&bin, &["-r", "echo $nope; echo \"END\";"], "");
    let combined = run.output();
    assert!(combined.contains("Undefined variable"), "warning not reported:\n{combined}");
    assert!(combined.contains("END"), "execution did not continue past the warning:\n{combined}");
    assert_eq!(run.code, 0, "a warning must not change the exit status, got {}", run.code);
}

// ── php-cli conformance cluster: #335 / #336 / #338 / #339 / #340 ───────────
//
// Each of these was found by the nightly CLI conformance harness
// (tests/cli-conformance/, `cargo xtask cli-conformance`) diffing `ephpm php`
// against a real php-cli. The corpus proves *sameness* but only runs on Linux
// with an upstream php installed; these assertions pin the same behavior from
// the ephpm side alone, on every platform the e2e suite runs on.

/// `--rf`/`--rc` on a name that doesn't resolve: php-cli prints
/// `Exception: <message>` and exits **1** (php_cli.c sets
/// `EG(exit_status) = 1` in that branch). ePHPm printed the identical line and
/// exited 0, so `php --rf … || handle_error` never fired (issue #335).
#[test]
fn reflection_of_missing_symbol_exits_one() {
    let Some(bin) = cli_binary() else {
        eprintln!("EPHPM_CLI_BINARY unset — skipping ephpm php CLI tests");
        return;
    };

    let func = run_php(&bin, &["-n", "--rf", "no_such_function_xyz"], "");
    assert!(
        func.stdout.contains("Exception: Function no_such_function_xyz() does not exist"),
        "--rf on a missing function lost php-cli's diagnostic:\n--- stdout ---\n{}\n\
         --- stderr ---\n{}",
        func.stdout,
        func.stderr
    );
    assert_eq!(func.code, 1, "--rf on a missing function must exit 1 like php-cli");

    let class = run_php(&bin, &["-n", "--rc", "NoSuchClass_xyz"], "");
    assert!(
        class.stdout.contains("Exception: Class \"NoSuchClass_xyz\" does not exist"),
        "--rc on a missing class lost php-cli's diagnostic:\n--- stdout ---\n{}\n\
         --- stderr ---\n{}",
        class.stdout,
        class.stderr
    );
    assert_eq!(class.code, 1, "--rc on a missing class must exit 1 like php-cli");
}

/// The other half of #335: a reflection flag that *succeeds* must still exit 0.
/// Without this, "make the failure exit 1" could regress into "always exit 1"
/// and no test would notice.
#[test]
fn reflection_of_present_symbol_exits_zero() {
    let Some(bin) = cli_binary() else {
        return;
    };
    let run = run_php(&bin, &["-n", "--rf", "strlen"], "");
    assert!(
        run.stdout.contains("Function [ <internal"),
        "--rf strlen did not print the reflection dump:\n--- stdout ---\n{}\n\
         --- stderr ---\n{}",
        run.stdout,
        run.stderr
    );
    assert_eq!(run.code, 0, "a successful --rf must exit 0");
}

/// An unrecognized option: php-cli's `php_getopt(…, show_err = 1)` writes
/// `Error in argument N, char M: option not found X` to stderr, `main()` then
/// prints usage on stdout and exits 1. ePHPm ignored the flag entirely, ran
/// nothing, and exited 0 — a typo'd flag silently succeeded (issue #336).
///
/// The argument index is 2 because the C-side argv is
/// `["ephpm", "-n", "-Z"]`, exactly as php-cli's is `["php", "-n", "-Z"]`.
#[test]
fn unknown_option_reports_and_exits_one() {
    let Some(bin) = cli_binary() else {
        return;
    };
    let run = run_php(&bin, &["-n", "-Z"], "");
    assert_eq!(
        run.stderr.trim_end(),
        "Error in argument 2, char 2: option not found Z",
        "unknown-option diagnostic did not match php-cli's byte-for-byte\n\
         --- stdout ---\n{}",
        run.stdout
    );
    assert!(
        run.stdout.starts_with("Usage: ephpm php"),
        "usage text must go to stdout, as php-cli's does:\n--- stdout ---\n{}",
        run.stdout
    );
    assert_eq!(run.code, 1, "an unknown option must exit 1, not succeed silently");
}

/// A *missing required argument* takes the same `PHP_GETOPT_INVALID_ARG`
/// return, so it must also fail loudly rather than fall through to "read the
/// program from stdin".
#[test]
fn option_missing_its_argument_exits_one() {
    let Some(bin) = cli_binary() else {
        return;
    };
    let run = run_php(&bin, &["-n", "-r"], "");
    assert!(
        run.stderr.contains("no argument for option r"),
        "missing -r argument lost php_getopt's diagnostic:\n--- stderr ---\n{}",
        run.stderr
    );
    assert_eq!(run.code, 1, "a missing option argument must exit 1");
}

/// php-cli's CLI `$_SERVER` contains the process environment (its
/// `variables_order` is `EGPCS`, and a CLI process's "S" is its environment)
/// plus an empty-string `DOCUMENT_ROOT`. Composer, PHPUnit and friends read
/// `$_SERVER['HOME']` / `$_SERVER['PATH']` directly; ePHPm's was missing both
/// (issue #338).
#[test]
fn server_superglobal_carries_environment_and_document_root() {
    let Some(bin) = cli_binary() else {
        return;
    };
    let run = run_php_env(
        &bin,
        &[
            "-n",
            "-r",
            "echo $_SERVER['EPHPM_CLI_TEST_VAR'] ?? '(unset)', '|', \
             var_export($_SERVER['DOCUMENT_ROOT'] ?? '(absent)', true);",
        ],
        &[("EPHPM_CLI_TEST_VAR", "hello-from-env")],
    );
    assert_eq!(
        run.stdout, "hello-from-env|''",
        "CLI $_SERVER must carry the environment and an empty DOCUMENT_ROOT\n\
         --- stderr ---\n{}",
        run.stderr
    );
    assert_eq!(run.code, 0);
}

/// `PHP_BINARY` must name the running executable. On Windows this always
/// worked by accident — php-src's `php_binary_init()` asks the OS there — but
/// on Linux/macOS it reads `sapi_module.executable_location`, which the embed
/// SAPI left NULL, so the constant registered as `""` (issue #339).
#[test]
fn php_binary_names_the_running_executable() {
    let Some(bin) = cli_binary() else {
        return;
    };
    let run = run_php(&bin, &["-n", "-r", "echo PHP_BINARY;"], "");
    assert!(!run.stdout.is_empty(), "PHP_BINARY is empty (stderr: {})", run.stderr);

    let reported = std::fs::canonicalize(run.stdout.trim())
        .unwrap_or_else(|e| panic!("PHP_BINARY {:?} is not a real path: {e}", run.stdout));
    let expected =
        std::fs::canonicalize(&bin).unwrap_or_else(|e| panic!("canonicalize {bin:?}: {e}"));
    assert_eq!(
        reported, expected,
        "PHP_BINARY must be the ephpm binary that is running, not another file"
    );
}

/// `STDIN`/`STDOUT`/`STDERR` are opened through the `php://std*` wrappers by
/// php-cli, so `stream_get_meta_data()` reports `wrapper_type = "PHP"` and a
/// `php://…` uri. ePHPm built them from raw `FILE*`s, leaving both absent, so
/// code that sniffs the wrapper to tell real stdio apart misbehaved (#340).
#[test]
fn stdio_constants_carry_php_wrapper_metadata() {
    let Some(bin) = cli_binary() else {
        return;
    };
    let run = run_php(
        &bin,
        &[
            "-n",
            "-r",
            "foreach (['stdin' => STDIN, 'stdout' => STDOUT, 'stderr' => STDERR] as $n => $h) {\
               $m = stream_get_meta_data($h);\
               echo $n, '=', $m['wrapper_type'] ?? 'NONE', ',', $m['uri'] ?? 'NONE', ';';\
             }",
        ],
        "",
    );
    assert_eq!(
        run.stdout, "stdin=PHP,php://stdin;stdout=PHP,php://stdout;stderr=PHP,php://stderr;",
        "stdio constants lost their php:// wrapper metadata\n--- stderr ---\n{}",
        run.stderr
    );
    assert_eq!(run.code, 0);
}

/// The stdio constants must still be usable handles, not just well-labeled
/// ones: reading STDIN and writing STDOUT/STDERR has to keep working after
/// the switch to the wrapper (#340).
#[test]
fn stdio_constants_still_do_io() {
    let Some(bin) = cli_binary() else {
        return;
    };
    let run = run_php(
        &bin,
        &["-n", "-r", "fwrite(STDOUT, 'out:' . trim(fgets(STDIN))); fwrite(STDERR, 'err');"],
        "piped-line\n",
    );
    assert_eq!(run.stdout, "out:piped-line", "STDIN/STDOUT I/O broke");
    assert_eq!(run.stderr, "err", "STDERR I/O broke");
    assert_eq!(run.code, 0);
}
