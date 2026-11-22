//! Shared PHP extension loading E2E.
//!
//! Proves the glibc-dynamic Linux binary can `dlopen` standard shared PHP
//! extensions at startup via `[php] extensions = [...]` — the payoff of the
//! dynamic baseline (`--export-dynamic` exports the Zend API so an out-of-tree
//! `.so` binds against the running binary). The extensions come from the
//! ePHPm ZTS extension catalog published per PHP minor at
//! `github.com/ephpm/php-sdk` (release tag `ext-<version>`), built ABI-matched
//! to the SDK: same PHP minor + ZTS + glibc + non-debug.
//!
//! Like the other worker/wordpress E2E, this needs a dedicated server instance
//! with the catalog mounted and referenced from config. The harness provides
//! its base URL via `EPHPM_EXT_URL`; the test self-skips (passes) when unset.
//!
//! ## Fixture
//!
//! Any glibc-dynamic ePHPm release image (PHP 8.5.x), plus the matching
//! catalog tarball. Locally:
//!
//! ```sh
//! # 1. fetch + extract the catalog for the image's PHP minor + arch
//! curl -fsSL -o ext.tgz \
//!   https://github.com/ephpm/php-sdk/releases/download/ext-8.5.12/ephpm-ext-8.5.12-linux-x86_64-gnu.tar.gz
//! mkdir ext && tar xzf ext.tgz -C ext
//!
//! # 2. serve a docroot containing tests/fixtures/ext-probe.php with a config:
//! #      [php]
//! #      extensions = ["/ext/igbinary.so","/ext/msgpack.so","/ext/apcu.so",
//! #                    "/ext/redis.so","/ext/mongodb.so"]
//! podman run -d -p 8110:8080 \
//!   -v "$PWD/docroot:/app:ro" -v "$PWD/ext:/ext:ro" \
//!   -v "$PWD/ephpm.toml:/etc/ephpm/ephpm.toml:ro" <ephpm-image>
//!
//! EPHPM_EXT_URL=http://127.0.0.1:8110 cargo test --test extensions
//! ```
//!
//! The probe script (`tests/fixtures/ext-probe.php`, served as `/ext-probe.php`)
//! reports which of the catalog extensions loaded and whether each is
//! functional (a real round-trip, not just `extension_loaded`).
//!
//! ## CLI counterpart
//!
//! `ephpm php --config <path>` honors `[php] extensions` too, so a
//! `wp`/`artisan`/cron run gets the same dynamically-loaded extension set the
//! server has. [`cli_config_loads_php_extensions`] proves it by spawning the
//! binary directly — `ephpm php --config <cfg> -r "var_dump(extension_loaded(…))"`
//! — against a config that lists the catalog `.so` files. The harness supplies
//! the binary via `EPHPM_EXT_BINARY` and the ABI-matched catalog directory via
//! `EPHPM_EXT_DIR` (the same catalog the HTTP fixture mounts); the test
//! self-skips (passes) when either is unset. Like the HTTP proof, this only
//! runs in the nightly/e2e harness — never in the local stub-mode suite, which
//! has no libphp and no catalog.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::Command;

/// Base URL of the extension-loaded instance, or `None` to skip.
fn ext_url() -> Option<String> {
    std::env::var("EPHPM_EXT_URL").ok().filter(|s| !s.is_empty())
}

/// The catalog extensions the fixture is expected to load, each paired with a
/// probe key the script sets `true` when the extension is not just loaded but
/// functionally exercised.
const EXPECTED: &[(&str, &str)] = &[
    ("igbinary", "igbinary"),   // serialize/unserialize round-trip
    ("msgpack", "msgpack"),     // pack/unpack round-trip
    ("apcu", "apcu"),           // store/fetch round-trip
    ("redis", "redis_class"),   // Redis class present
    ("mongodb", "mongodb_class"), // MongoDB\Driver\Manager present
];

#[tokio::test]
async fn shared_extensions_load_and_function() {
    let Some(base) = ext_url() else {
        eprintln!("EPHPM_EXT_URL unset — skipping extension E2E");
        return;
    };

    let body = reqwest::get(format!("{base}/ext-probe.php"))
        .await
        .expect("probe request failed")
        .error_for_status()
        .expect("probe returned non-2xx")
        .text()
        .await
        .expect("probe body read failed");

    let json: serde_json::Value =
        serde_json::from_str(&body).unwrap_or_else(|e| panic!("probe not JSON ({e}): {body}"));

    let loaded: Vec<String> = json["loaded"]
        .as_array()
        .expect("`loaded` missing")
        .iter()
        .map(|v| v.as_str().unwrap_or_default().to_string())
        .collect();

    let functional: BTreeMap<String, bool> = json["functional"]
        .as_object()
        .expect("`functional` missing")
        .iter()
        .map(|(k, v)| (k.clone(), v.as_bool().unwrap_or(false)))
        .collect();

    for (ext, fkey) in EXPECTED {
        assert!(
            loaded.iter().any(|l| l == ext),
            "extension `{ext}` did not load (catalog .so dlopen via [php] extensions); loaded = {loaded:?}",
        );
        assert_eq!(
            functional.get(*fkey),
            Some(&true),
            "extension `{ext}` loaded but its functional probe `{fkey}` was not true; functional = {functional:?}",
        );
    }
}

/// Binary + ABI-matched catalog directory for the CLI extension proof, or
/// `None` to skip.
fn cli_ext_fixture() -> Option<(PathBuf, PathBuf)> {
    let binary = std::env::var("EPHPM_EXT_BINARY").ok().filter(|s| !s.is_empty())?;
    let dir = std::env::var("EPHPM_EXT_DIR").ok().filter(|s| !s.is_empty())?;
    Some((PathBuf::from(binary), PathBuf::from(dir)))
}

/// `ephpm php --config <cfg>` loads `[php] extensions` from the config — the
/// CLI's extension set matches the server's.
///
/// Spawns the real binary once per catalog extension with
/// `-r "var_dump(extension_loaded('<ext>'))"` against a generated config that
/// lists the catalog `.so` files under `[php] extensions`, and asserts each
/// prints `bool(true)`. Proves the `-d extension=` defines the CLI prepends
/// reach PHP's module startup (and that multiple `extension=` directives
/// accumulate — every listed extension loads, not just the last).
#[test]
fn cli_config_loads_php_extensions() {
    let Some((binary, ext_dir)) = cli_ext_fixture() else {
        eprintln!("EPHPM_EXT_BINARY / EPHPM_EXT_DIR unset — skipping CLI extension E2E");
        return;
    };

    // A config whose only job is to list the catalog `.so` files under
    // [php] extensions, each as an absolute path (no extension_dir search) and
    // forward-slashed so the path is a clean TOML basic string on any host.
    use std::fmt::Write as _;
    let mut toml = String::from("[php]\nextensions = [\n");
    for (ext, _) in EXPECTED {
        let so = ext_dir.join(format!("{ext}.so"));
        assert!(so.is_file(), "catalog is missing {}: {}", ext, so.display());
        let _ = writeln!(toml, "    \"{}\",", so.to_string_lossy().replace('\\', "/"));
    }
    toml.push_str("]\n");

    let tmp = tempfile::tempdir().expect("tempdir");
    let cfg = tmp.path().join("ephpm.toml");
    std::fs::write(&cfg, toml).expect("write config");

    for (ext, _) in EXPECTED {
        let out = Command::new(&binary)
            .arg("php")
            .arg("--config")
            .arg(&cfg)
            .arg("-r")
            .arg(format!("var_dump(extension_loaded('{ext}'));"))
            .output()
            .unwrap_or_else(|e| panic!("spawning `ephpm php --config -r` failed: {e}"));
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            out.status.success(),
            "`ephpm php --config -r` exited non-zero for `{ext}`\nstdout:\n{stdout}\nstderr:\n{stderr}",
        );
        assert!(
            stdout.contains("bool(true)"),
            "extension `{ext}` was not loaded by `ephpm php --config` — `[php] extensions` did not \
             reach the CLI\nstdout:\n{stdout}\nstderr:\n{stderr}",
        );
    }
}
