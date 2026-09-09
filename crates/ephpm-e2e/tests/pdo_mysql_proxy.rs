//! `pdo_mysql`-through-the-`[db.mysql]`-proxy end-to-end coverage (issue #433).
//!
//! Every other DB-path e2e suite (`rw_split`, `sqlite`, `db_bridge`, ...) drives
//! `pdo_mysql` against the *embedded* Turso engine behind litewire
//! (`[db.sqlite]`). The `[db.mysql]` **proxy** — the `crates/ephpm-db` wire
//! forwarder that sits between PHP and a real MySQL/MariaDB — was covered only
//! by `crates/ephpm-db/tests/proxy_integration.rs`, which drives it with
//! `mysql_async`, not PHP's `pdo_mysql`. So a regression visible only to
//! `pdo_mysql`'s connection/protocol behaviour toward the proxy (exactly the
//! class of bug #425 was about) had no CI guard.
//!
//! This suite closes that gap: it stands up a **PHP-linked** ephpm with a
//! `[db.mysql]` proxy in front of a real backend, then drives a `CREATE` +
//! `INSERT` + `SELECT` round-trip from real PHP over HTTP and asserts the row
//! comes back.
//!
//! ## How it is wired
//!
//! Self-managed suite: the harness (`cargo xtask e2e`) starts no node for it,
//! it just receives `EPHPM_BINARY` (see `SELF_MANAGED_SUITES` in
//! `xtask/src/e2e_bare.rs`) and spawns its own [`MysqlProxyFixture`]. The real
//! backend comes from `EPHPM_MYSQL_PROXY_TEST_URL`
//! (`mysql://user:pass@host:port/db`), set by the CI job that boots the
//! database (`.github/workflows/pdo-mysql-e2e.yml`).
//!
//! ## Skip vs. fail
//!
//! Both env vars unset ⇒ the suite **self-skips** (prints why, returns `ok`) so
//! `cargo xtask e2e` on a host with no MySQL — the normal bare-process lane —
//! does not fail. In the CI job that *does* provision a database,
//! `EPHPM_REQUIRE_DB_TESTS=1` flips a missing prerequisite from a skip into a
//! panic, so the whole point of the job cannot silently evaporate into a green
//! check (the failure mode `.github/workflows/db-integration.yml` documents for
//! issue #238).

use std::path::PathBuf;

use ephpm_e2e::{MysqlProxyFixture, ephpm_binary_env};
use serde_json::Value;
use tempfile::TempDir;

/// The one PHP script this suite serves. Connects through the proxy with the
/// injected `DB_*` env (the proxy discards client credentials, but production
/// PHP sends them, so send them here too), then exercises a full write→read
/// round-trip and reports the result as JSON.
const PDO_SCRIPT: &str = r#"<?php
header('Content-Type: application/json');

// ePHPm injects DB_HOST/DB_PORT/... as $_SERVER entries (the SAPI registers
// them as server variables), NOT into the process environment — so `getenv()`
// does NOT see them and would fall back to 3306, where nothing listens (the
// proxy binds a random loopback port). Read $_SERVER first; getenv/defaults
// are only a fallback for running this script off ePHPm.
function db_var($key, $default) {
    if (isset($_SERVER[$key]) && $_SERVER[$key] !== '') {
        return $_SERVER[$key];
    }
    $v = getenv($key);
    return ($v !== false && $v !== '') ? $v : $default;
}

$host = db_var('DB_HOST', '127.0.0.1');
$port = db_var('DB_PORT', '3306');
$name = db_var('DB_NAME', 'test');
$user = db_var('DB_USER', 'root');
$pass = db_var('DB_PASSWORD', '');
$action = $_GET['action'] ?? 'roundtrip';

try {
    $pdo = new PDO(
        "mysql:host={$host};port={$port};dbname={$name}",
        $user,
        $pass,
        [PDO::ATTR_ERRMODE => PDO::ERRMODE_EXCEPTION]
    );

    switch ($action) {
        case 'roundtrip':
            // Idempotent: a fresh backend each CI run, but tolerate a re-run.
            $pdo->exec('DROP TABLE IF EXISTS ephpm_e2e_pdo');
            $pdo->exec(
                'CREATE TABLE ephpm_e2e_pdo ('
                . 'id INT AUTO_INCREMENT PRIMARY KEY, '
                . 'label VARCHAR(64) NOT NULL)'
            );

            // Write via a prepared statement (COM_STMT_PREPARE/EXECUTE — the
            // protocol path a plain query does not exercise).
            $ins = $pdo->prepare('INSERT INTO ephpm_e2e_pdo (label) VALUES (?)');
            $ins->execute(['hello-from-pdo']);
            $id = (int) $pdo->lastInsertId();

            // Read it back.
            $sel = $pdo->prepare('SELECT id, label FROM ephpm_e2e_pdo WHERE id = ?');
            $sel->execute([$id]);
            $row = $sel->fetch(PDO::FETCH_ASSOC);

            $ver = $pdo->query('SELECT VERSION() AS v')->fetch(PDO::FETCH_ASSOC)['v'] ?? '';

            echo json_encode([
                'status' => 'ok',
                'inserted_id' => $id,
                'row' => $row,
                'server_version' => $ver,
            ]);
            break;

        case 'cleanup':
            $pdo->exec('DROP TABLE IF EXISTS ephpm_e2e_pdo');
            echo json_encode(['status' => 'ok']);
            break;

        default:
            http_response_code(400);
            echo json_encode(['status' => 'error', 'error' => 'unknown action']);
    }
} catch (Throwable $e) {
    http_response_code(500);
    echo json_encode(['status' => 'error', 'error' => $e->getMessage()]);
}
"#;

/// Backend MySQL/MariaDB URL, or `None` when the suite should skip.
fn backend_url() -> Option<String> {
    std::env::var("EPHPM_MYSQL_PROXY_TEST_URL").ok().filter(|s| !s.is_empty())
}

/// Whether a missing prerequisite must fail rather than skip. Set by the CI job
/// that provisions the database; unset everywhere else. Mirrors
/// `EPHPM_REQUIRE_DB_TESTS` in `crates/ephpm-db/tests/common/mod.rs`.
fn require_db_tests() -> bool {
    std::env::var_os("EPHPM_REQUIRE_DB_TESTS").is_some()
}

/// Resolve `(binary, backend_url)` or return `None` to skip — panicking instead
/// when [`require_db_tests`] says a skip is not allowed.
fn prerequisites() -> Option<(PathBuf, String)> {
    let require = require_db_tests();

    let binary = match ephpm_binary_env() {
        Some(b) => b,
        None if require => panic!(
            "EPHPM_BINARY must be set when EPHPM_REQUIRE_DB_TESTS=1 — the CI job \
             that provisions MySQL is supposed to build the PHP-linked binary and \
             run this suite through `cargo xtask e2e`"
        ),
        None => {
            eprintln!("skipping pdo_mysql_proxy: EPHPM_BINARY unset (no built ephpm binary)");
            return None;
        }
    };

    let url = match backend_url() {
        Some(u) => u,
        None if require => panic!(
            "EPHPM_MYSQL_PROXY_TEST_URL must be set when EPHPM_REQUIRE_DB_TESTS=1 — \
             a real MySQL/MariaDB backend was not provisioned"
        ),
        None => {
            eprintln!("skipping pdo_mysql_proxy: EPHPM_MYSQL_PROXY_TEST_URL unset (no backend)");
            return None;
        }
    };

    Some((binary, url))
}

/// Write the single PHP script into a throwaway docroot the caller owns.
///
/// The suite builds its own docroot rather than serving the shared
/// `tests/docroot` so it stays fully self-contained (like `turso_cdc`): nothing
/// about it can perturb — or be perturbed by — another suite's fixtures.
fn make_docroot() -> TempDir {
    let dir = tempfile::Builder::new()
        .prefix("ephpm-e2e-pdo-docroot-")
        .tempdir()
        .expect("create docroot tempdir");
    std::fs::write(dir.path().join("pdo_mysql_test.php"), PDO_SCRIPT)
        .expect("write pdo_mysql_test.php");
    dir
}

/// A `CREATE` + prepared `INSERT` + prepared `SELECT` round-trip driven from
/// real PHP `pdo_mysql`, through the `[db.mysql]` proxy, to a real backend.
#[tokio::test]
async fn pdo_mysql_proxy_write_read_roundtrip() {
    let Some((binary, url)) = prerequisites() else {
        return;
    };

    let docroot = make_docroot();
    let fixture = MysqlProxyFixture::start(&binary, docroot.path(), &url)
        .await
        .expect("start ephpm with a [db.mysql] proxy");

    // The proxy's listener binds immediately, but it *drains* (accepts then
    // closes) connections until its backend pool has seeded — a brief window
    // after the HTTP health endpoint already reports 200. A request that lands
    // in it fails with a connection error, so poll the (idempotent — it
    // DROP/CREATEs its own table) round-trip until the proxy is fully up.
    let roundtrip_url = format!("{}/pdo_mysql_test.php?action=roundtrip", fixture.base_url());
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    let (status, body) = loop {
        let resp = reqwest::get(&roundtrip_url).await.expect("GET roundtrip");
        let status = resp.status();
        let body = resp.text().await.expect("read body");
        if status == 200 || std::time::Instant::now() >= deadline {
            break (status, body);
        }
        eprintln!("roundtrip not ready yet (HTTP {status}); retrying: {body}");
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    };

    assert_eq!(status, 200, "roundtrip should return 200, got {status}: {body}");

    let json: Value = serde_json::from_str(&body)
        .unwrap_or_else(|e| panic!("response was not JSON ({e}): {body}"));
    assert_eq!(json["status"], "ok", "PHP reported an error: {body}");

    // The inserted row must come back through the proxy with both columns
    // intact — the actual proof that pdo_mysql wrote and read via the proxy.
    let id = json["inserted_id"].as_i64().unwrap_or(0);
    assert!(id > 0, "expected a positive AUTO_INCREMENT id, got: {body}");
    assert_eq!(json["row"]["id"].as_i64(), Some(id), "read-back id mismatch: {body}");
    assert_eq!(json["row"]["label"], "hello-from-pdo", "read-back label mismatch: {body}");

    // A real MySQL/MariaDB answered VERSION(); an embedded-engine stand-in would
    // not (this suite must exercise the proxy, not `[db.sqlite]`).
    assert!(
        json["server_version"].as_str().is_some_and(|v| !v.is_empty()),
        "expected a non-empty backend VERSION(): {body}"
    );

    // Best-effort cleanup; the backend is torn down with the CI job regardless.
    let _ = reqwest::get(format!("{}/pdo_mysql_test.php?action=cleanup", fixture.base_url())).await;
}
