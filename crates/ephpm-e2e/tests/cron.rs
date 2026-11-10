//! Real-spawn proof for the distributed-cron executor's per-site PHP invocation
//! (PR #698). A `.php` cron job — a `command = ["script.php", …]` entry or any
//! file in a site's `.ephpm/cron/` tier — is run by spawning this binary as
//! `ephpm php --site <site> -f <script> [-- <args>]`.
//!
//! The executor used to emit a *leading* `--` instead
//! (`ephpm php --site <site> -- <script>`). php-cli reads a leading `--` as
//! "take the program from stdin", and the executor closes the child's stdin
//! (`Stdio::null()`), so php compiled an *empty* program, exited 0, and the job
//! was recorded `ok` while never running — every fresh-deploy `.php` job and the
//! whole directory tier silently no-op'd. The unit tests in
//! `cron_scheduler::exec` pin the argv shape; this pins the behaviour by
//! spawning a real binary and asserting the script's side effect.
//!
//! Needs `EPHPM_BINARY` (a real PHP-linked build); without it the test **skips**
//! so the crate still compile-checks on a box with no binary. `cargo xtask e2e`
//! sets `EPHPM_BINARY` for the self-managed suites.

use ephpm_e2e::{DbImportFixture, ephpm_binary_env};

/// Writes a marker file next to itself (so the test can find it from the site
/// dir) and prints a sentinel to stdout. If php ever reads an empty program
/// from stdin instead of running this file, neither side effect happens.
const MARKER_PHP: &str = r#"<?php
file_put_contents(__DIR__ . '/cron-marker.txt', 'ran');
echo "CRON_RAN";
"#;

/// Spawning `ephpm php --site <site> -f <script>` — exactly the argv tail
/// `cron_scheduler::exec::build_argv` now emits for a per-site `.php` job —
/// actually executes the script.
#[test]
fn per_site_php_cron_job_actually_runs_the_script() {
    let Some(binary) = ephpm_binary_env() else {
        eprintln!("EPHPM_BINARY not set — skipping per_site_php_cron_job_actually_runs_the_script");
        return;
    };

    // The script is written as the vhost's `index.php`; run it offline (no
    // server, no DB needed) straight off `prepare`.
    let mut fixture = DbImportFixture::prepare(&binary, "shop", MARKER_PHP).expect("prepare");
    fixture.go_offline();

    let script = fixture.scratch_dir().join("sites").join("shop").join("index.php");
    let marker = script.parent().unwrap().join("cron-marker.txt");
    assert!(!marker.exists(), "marker must not exist before the run");

    let out = fixture.php(&["-f", script.to_str().unwrap()]).expect("spawn `ephpm php --site -f`");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "`ephpm php --site shop -f <script>` exited non-zero\nstdout:\n{stdout}\nstderr:\n{stderr}",
    );
    assert!(
        stdout.contains("CRON_RAN"),
        "script sentinel missing — php did not execute the file (the stdin-program \
         regression would print nothing)\nstdout:\n{stdout}\nstderr:\n{stderr}",
    );
    assert!(
        marker.exists(),
        "the script's marker file was never written — the job did not run\n\
         stdout:\n{stdout}\nstderr:\n{stderr}",
    );
    assert_eq!(std::fs::read_to_string(&marker).unwrap(), "ran");
}
