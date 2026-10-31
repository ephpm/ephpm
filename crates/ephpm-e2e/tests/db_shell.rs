//! End-to-end `ephpm db query` / `ephpm db shell` against a real binary.
//!
//! Exercises both consoles over a self-managed per-site node ([`DbImportFixture`],
//! isolated under its own tempdir — never a shared node or sibling suite,
//! #288/#295):
//!
//! 1. Seed the site offline with `ephpm db import` (the established path).
//! 2. **Offline `db query`** — `SELECT` the imported rows and assert the table
//!    output (default `table` format) and the JSON format carry the fake data.
//! 3. **Offline `db shell`** — drive a piped session (`/tables`, a `SELECT`,
//!    `/quit`) and assert stdout shows the table name and a known row.
//! 4. **Live `/kv-put` round-trip** — start the server, open a `db shell` (live
//!    mode, MySQL dialect), `SELECT` the rows, `/kv-put` them into the live KV,
//!    then `/kv GET` one back and assert the value landed.
//!
//! Needs `EPHPM_BINARY` (a real build); without it every test **skips** so the
//! crate still compile-checks on a box with no binary. `cargo xtask e2e` sets
//! `EPHPM_BINARY` for this suite (listed in `SELF_MANAGED_SUITES`).

use ephpm_e2e::{DbImportFixture, ephpm_binary_env};

/// A vhost `index.php` — unused by these tests (they read through the consoles,
/// not HTTP) but required to create the vhost directory.
const VHOST_INDEX_PHP: &str = "<?php echo 'ok';\n";

/// A small fake `mysqldump` with three obviously-fake customer rows.
fn fake_mysqldump() -> String {
    r#"-- MySQL dump 10.13  Distrib 8.0.36, for Linux (x86_64)
/*!40101 SET NAMES utf8mb4 */;
DROP TABLE IF EXISTS `customers`;
CREATE TABLE `customers` (
  `id` int unsigned NOT NULL AUTO_INCREMENT,
  `name` varchar(128) NOT NULL,
  `email` varchar(190) NOT NULL,
  PRIMARY KEY (`id`)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;
LOCK TABLES `customers` WRITE;
INSERT INTO `customers` VALUES (1,'Ada Fakeington','ada@fake.test'),(2,'Bram Testworth','bram@fake.test'),(3,'Cleo Mockby','cleo@fake.test');
UNLOCK TABLES;
-- Dump completed
"#
    .to_string()
}

/// Import the fake dump offline into a fresh fixture, returning it ready to
/// serve. Panics on any import failure (the point under test is the console,
/// not the importer).
fn seed(binary: &std::path::Path) -> DbImportFixture {
    let fixture = DbImportFixture::prepare(binary, "shop", VHOST_INDEX_PHP).expect("prepare");
    let dump = fixture.scratch_dir().join("customers.sql");
    std::fs::write(&dump, fake_mysqldump()).expect("write dump");
    let out = fixture
        .import(&dump, &["--dialect", "mysql", "--on-error", "skip"])
        .expect("run import");
    assert!(
        out.status.success(),
        "import failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    fixture
}

/// Offline `db query`: a SELECT prints the imported rows (table + json).
#[test]
fn db_query_offline_reads_imported_rows() {
    let Some(binary) = ephpm_binary_env() else {
        eprintln!("EPHPM_BINARY not set — skipping db_query_offline_reads_imported_rows");
        return;
    };
    let fixture = seed(&binary);

    // Default table format.
    let out = fixture
        .query(&["SELECT id, name FROM customers ORDER BY id"])
        .expect("db query");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "db query failed\nstdout:\n{stdout}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stderr),
    );
    assert!(stdout.contains("Ada Fakeington"), "table output missing a known row:\n{stdout}");
    assert!(stdout.contains("(3 rows)"), "table output missing the row count:\n{stdout}");

    // JSON format round-trips the same data.
    let out = fixture
        .query(&["--format", "json", "SELECT id, name FROM customers ORDER BY id"])
        .expect("db query json");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "db query --format json failed:\n{stdout}");
    let rows: serde_json::Value =
        serde_json::from_str(stdout.trim()).unwrap_or_else(|e| panic!("parse json: {e}\n{stdout}"));
    assert_eq!(rows.as_array().map(Vec::len), Some(3), "expected three rows: {rows}");
    assert_eq!(rows[1]["name"], serde_json::json!("Bram Testworth"));
}

/// Offline `db shell`: a piped session lists the table and reads a row back.
#[test]
fn db_shell_offline_session() {
    let Some(binary) = ephpm_binary_env() else {
        eprintln!("EPHPM_BINARY not set — skipping db_shell_offline_session");
        return;
    };
    let fixture = seed(&binary);

    let script = "/tables\nSELECT name FROM customers WHERE id = 2;\n/quit\n";
    let out = fixture.shell(&[], script).expect("db shell");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "db shell exited {:?}\nstderr:\n{stderr}", out.status);
    // /tables output (stdout) names the imported table.
    assert!(stdout.contains("customers"), "/tables did not list the table:\n{stdout}");
    // The SELECT read the known row.
    assert!(stdout.contains("Bram Testworth"), "SELECT did not return the row:\n{stdout}");
}

/// Live `/kv-put`: with the server up, the shell runs in live (MySQL) mode;
/// it SELECTs the rows, writes them into the live KV, and reads one back.
#[tokio::test]
async fn db_shell_live_kv_put_round_trips() {
    let Some(binary) = ephpm_binary_env() else {
        eprintln!("EPHPM_BINARY not set — skipping db_shell_live_kv_put_round_trips");
        return;
    };
    let mut fixture = seed(&binary);
    fixture.serve().await.expect("serve");

    // Live mode forces the MySQL dialect. SELECT populates the shell's "last
    // result set", /kv-put writes one key per row (value = the `name` column),
    // and /kv GET reads one back — all per-site-authenticated over RESP.
    let script = "SELECT id, name FROM customers ORDER BY id;\n\
                  /kv-put 'cust:${id}' name\n\
                  /kv GET cust:2\n\
                  /quit\n";
    let out = fixture.shell(&[], script).expect("db shell live");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "db shell exited {:?}\nstderr:\n{stderr}", out.status);

    // The /kv GET cust:2 reply (stdout) is the value /kv-put wrote for row 2.
    assert!(
        stdout.contains("Bram Testworth"),
        "the /kv-put value did not land in the live KV (/kv GET cust:2):\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    // The /kv-put feedback (stderr) reports three keys written.
    assert!(
        stderr.contains("wrote 3 key"),
        "/kv-put did not report three keys:\nstderr:\n{stderr}"
    );
}
