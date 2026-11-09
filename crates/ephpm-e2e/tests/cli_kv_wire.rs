//! End-to-end `ephpm php --site` KV wire (issue #700): an out-of-band CLI/cron
//! invocation reaches a **running** server's live KV, symmetric to the DB wire
//! (`php_site_env.rs`).
//!
//! Over the self-managed per-site [`DbImportFixture`] (isolated under its own
//! tempdir — never a shared node, #288/#295):
//!
//! 1. **Headline:** with the server up, `ephpm php --site shop -r '…'` reports
//!    `ephpm_kv_attached() === true`, writes a key, and the **serving process**
//!    sees it — an HTTP request to a PHP script reading that key returns it.
//! 2. **Per-site isolation:** a second vhost's CLI attach cannot read a key
//!    written for `shop`.
//! 3. **Operator interop:** the CLI-written value is also readable through the
//!    public `[kv.redis_compat]` listener via `ephpm kv --user shop
//!    --password <derived> get …` (the fixture enables that listener).
//!
//! The `ephpm_kv_attached() === false` (no server) case is pinned by
//! `cli.rs::kv_attached_is_false_with_no_server`, which needs no server and so
//! lives in the no-server CLI suite.
//!
//! **Clustered replication is lab-gated, not covered here.** A CLI write lands
//! in the server's own per-site `Store`; once `[cluster]` is enabled that store
//! replicates across nodes through the existing gossip path (no new code), so a
//! CLI write on one node is observed on another. The transport is proven by
//! `crates/ephpm/tests/internal_kv_wire.rs`; the multi-node observation is the
//! documented manual lab step in `site/content/roadmap/cli-kv-wire.md`.
//!
//! Needs `EPHPM_BINARY` (a real build); without it every test **skips** so the
//! crate still compile-checks on a box with no binary.

use std::process::Command;

use ephpm_e2e::{DbImportFixture, ephpm_binary_env};

/// The per-site RESP password for site `shop` under the fixture's fixed
/// `[kv] secret` (`"e2e-db-import-secret-not-for-prod"`, see
/// `DB_IMPORT_TEMPLATE`): `HMAC-SHA256(secret, "shop")`, lowercase hex. Pinned
/// here rather than derived, to keep this isolated e2e crate free of an
/// `ephpm-kv` dependency. Regenerate with:
///
/// ```text
/// printf '%s' shop | openssl dgst -sha256 \
///     -hmac e2e-db-import-secret-not-for-prod
/// ```
const SHOP_KV_PASSWORD: &str =
    "92db9ab40f6da362836297ee07b21cb171c13748342d139184b2606d9db38bea";

/// A vhost `index.php` that echoes a KV value the way a cached page would read
/// a warmed entry — the "serving process sees the CLI's write" probe.
const INDEX_READS_KV: &str = r#"<?php echo ephpm_kv_get('warm:key') ?? 'MISS';"#;

/// The `[kv.redis_compat]` listener port, parsed from the fixture's config.
fn kv_listener_port(fixture: &DbImportFixture) -> u16 {
    let config = std::fs::read_to_string(fixture.config_path()).expect("read config");
    // The only `listen = "127.0.0.1:N"` under [kv.redis_compat].
    let mut in_section = false;
    for line in config.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            in_section = trimmed == "[kv.redis_compat]";
            continue;
        }
        if in_section && let Some(rest) = trimmed.strip_prefix("listen") {
            let addr = rest.trim_start_matches([' ', '=']).trim().trim_matches('"');
            if let Some((_, port)) = addr.rsplit_once(':') {
                return port.parse().expect("kv port");
            }
        }
    }
    panic!("no [kv.redis_compat] listen in config:\n{config}");
}

/// Run `ephpm php --site <site> --config <cfg> <args>` against the fixture's
/// server for an arbitrary site (the fixture's own `php()` hardcodes its one
/// site; the isolation test needs a second).
fn php_site(binary: &std::path::Path, fixture: &DbImportFixture, site: &str, args: &[&str]) -> std::process::Output {
    let mut cmd = Command::new(binary);
    cmd.args(["php", "--site", site, "--config"]).arg(fixture.config_path()).args(args);
    cmd.output().expect("run ephpm php --site")
}

/// Headline: a CLI write is visible to the serving process, and
/// `ephpm_kv_attached()` is true while the server is up.
#[tokio::test]
async fn cli_kv_write_is_visible_to_the_serving_process() {
    let Some(binary) = ephpm_binary_env() else {
        eprintln!("EPHPM_BINARY not set — skipping cli_kv_write_is_visible_to_the_serving_process");
        return;
    };
    let mut fixture = DbImportFixture::prepare(&binary, "shop", INDEX_READS_KV).expect("prepare");
    fixture.serve().await.expect("serve");

    // CLI: attached, write, read-back — all over the wire to the running server.
    let out = fixture
        .php(&[
            "-r",
            "var_dump(ephpm_kv_attached()); \
             var_dump(ephpm_kv_set('warm:key', 'hot', 0)); \
             var_dump(ephpm_kv_get('warm:key'));",
        ])
        .expect("ephpm php --site (kv)");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "php --site exited {:?}\nstderr:\n{stderr}", out.status);
    assert!(
        stdout.contains("bool(true)") && stdout.contains("string(3) \"hot\""),
        "CLI must report attached + the written value:\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        stderr.contains("attached ephpm_kv_*"),
        "the CLI must log that it attached to the server's KV:\n{stderr}"
    );

    // The serving process reads the same key the CLI warmed.
    let client = reqwest::Client::new();
    let body = client
        .get(format!("{}/index.php", fixture.base_url()))
        .header("Host", fixture.site())
        .send()
        .await
        .expect("GET /index.php")
        .text()
        .await
        .expect("body");
    assert_eq!(body, "hot", "the serving process must see the CLI's KV write (got {body:?})");
}

/// Per-site isolation: a second vhost's CLI attach cannot read `shop`'s key.
#[tokio::test]
async fn cli_kv_is_isolated_per_site() {
    let Some(binary) = ephpm_binary_env() else {
        eprintln!("EPHPM_BINARY not set — skipping cli_kv_is_isolated_per_site");
        return;
    };
    let mut fixture = DbImportFixture::prepare(&binary, "shop", INDEX_READS_KV).expect("prepare");
    // A second vhost directory so `--site other` resolves (fail-closed needs it).
    std::fs::create_dir_all(fixture.scratch_dir().join("sites").join("other"))
        .expect("other vhost dir");
    fixture.serve().await.expect("serve");

    // `shop` writes a key.
    let w = fixture.php(&["-r", "ephpm_kv_set('shop:only', 'S', 0);"]).expect("shop write");
    assert!(w.status.success(), "shop write failed:\n{}", String::from_utf8_lossy(&w.stderr));

    // `other`, attaching as itself, must see a miss.
    let r = php_site(&binary, &fixture, "other", &["-r", "var_dump(ephpm_kv_get('shop:only'));"]);
    let out = String::from_utf8_lossy(&r.stdout);
    assert!(
        out.contains("NULL"),
        "site `other` must not read `shop`'s key:\nstdout:\n{out}\nstderr:\n{}",
        String::from_utf8_lossy(&r.stderr)
    );

    // `shop` still sees its own.
    let s = fixture.php(&["-r", "var_dump(ephpm_kv_get('shop:only'));"]).expect("shop read");
    assert!(
        String::from_utf8_lossy(&s.stdout).contains("string(1) \"S\""),
        "shop must still read its own key:\n{}",
        String::from_utf8_lossy(&s.stdout)
    );
}

/// Operator interop: a value the CLI wrote over the wire is readable through
/// the public `[kv.redis_compat]` listener with the operator `ephpm kv` tool.
#[tokio::test]
async fn cli_kv_write_is_readable_via_operator_tooling() {
    let Some(binary) = ephpm_binary_env() else {
        eprintln!("EPHPM_BINARY not set — skipping cli_kv_write_is_readable_via_operator_tooling");
        return;
    };
    let mut fixture = DbImportFixture::prepare(&binary, "shop", INDEX_READS_KV).expect("prepare");
    fixture.serve().await.expect("serve");

    let w = fixture.php(&["-r", "ephpm_kv_set('warm:key', 'hot', 0);"]).expect("cli write");
    assert!(w.status.success(), "cli write failed:\n{}", String::from_utf8_lossy(&w.stderr));

    // Read it back through the public RESP listener, as site `shop`.
    let port = kv_listener_port(&fixture);
    let out = Command::new(&binary)
        .args([
            "kv",
            "--host",
            "127.0.0.1",
            "--port",
            &port.to_string(),
            "--user",
            "shop",
            "--password",
            SHOP_KV_PASSWORD,
            "get",
            "warm:key",
        ])
        .output()
        .expect("run ephpm kv get");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "ephpm kv get failed:\n{}", String::from_utf8_lossy(&out.stderr));
    assert!(
        stdout.contains("hot"),
        "operator tooling must read the CLI-written value over the public listener:\n{stdout}"
    );
}
