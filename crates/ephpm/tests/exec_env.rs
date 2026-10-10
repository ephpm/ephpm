//! End-to-end regression test: `ephpm exec` must not leak its environment
//! into the tenant command.
//!
//! The command used to inherit ePHPm's whole environment, so an operator
//! running `sudo EPHPM_KV__SECRET=... ephpm exec --site blog -- <cmd>` (or a
//! control plane with cloud credentials in its environment) handed those
//! secrets to the tenant's command. This spawns the real binary with secrets
//! in its environment, runs `env` inside it, and asserts only the allowlist
//! arrives.
//!
//! Uses `--no-sandbox` so it runs rootless on any Linux host (the full sandbox
//! needs root + Landlock). The environment is cleared on both paths, so this
//! covers the sandboxed path's environment handling too; the sandboxed path's
//! `sandbox_env` is additionally unit-tested in `exec_sandbox.rs`.
//!
//! Run with: `cargo test -p ephpm --test exec_env`

#![cfg(target_os = "linux")]

use std::process::Command;

const BIN: &str = env!("CARGO_BIN_EXE_ephpm");

#[test]
fn exec_command_gets_a_cleared_environment() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let sites = tmp.path().join("sites");
    std::fs::create_dir_all(sites.join("blog")).expect("create site dir");
    let config = tmp.path().join("ephpm.toml");
    // `--no-sandbox` performs no uid drop, but the tenant user is still
    // resolved; a numeric uid resolves without a passwd entry.
    std::fs::write(
        &config,
        format!("[server]\nsites_dir = \"{}\"\nrun_as_user = \"65534\"\n", sites.display()),
    )
    .expect("write config");

    let out = Command::new(BIN)
        .args(["exec", "--config"])
        .arg(&config)
        .args(["--site", "blog", "--no-sandbox", "--", "env"])
        // Start from a known environment so a stray LC_*/TZ in the test runner
        // cannot change the expected allowlist.
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("EPHPM_EXEC_LEAK_CANARY", "parent-secret")
        .env("AWS_SECRET_ACCESS_KEY", "parent-aws-secret")
        .env("LD_PRELOAD", "")
        .env("LANG", "C.UTF-8")
        .env("TERM", "dumb")
        .output()
        .expect("spawn ephpm exec");

    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "ephpm exec failed\nstdout:\n{stdout}\nstderr:\n{stderr}");

    for leaked in ["EPHPM_EXEC_LEAK_CANARY", "parent-secret", "AWS_SECRET_ACCESS_KEY", "LD_PRELOAD"]
    {
        assert!(!stdout.contains(leaked), "{leaked} leaked into the exec'd command:\n{stdout}");
    }

    let mut names: Vec<&str> =
        stdout.lines().filter_map(|l| l.split_once('=').map(|(k, _)| k)).collect();
    names.sort_unstable();
    assert_eq!(names, ["HOME", "LANG", "PATH", "TERM", "TMPDIR"], "full env:\n{stdout}");
    assert!(stdout.lines().any(|l| l == "LANG=C.UTF-8"));
    assert!(stdout.lines().any(|l| l == "TERM=dumb"));
    assert!(
        stdout.lines().any(|l| l.starts_with("HOME=") && l.contains("ephpm-vhosts")),
        "HOME must point at the tenant state root:\n{stdout}"
    );
}
